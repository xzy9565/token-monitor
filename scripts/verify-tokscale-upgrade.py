#!/usr/bin/env python3
"""Check a tokscale upgrade against this Mac's own usage data before merging it.

Builds the checked-out tree and runs it next to the installed `token-monitor` over the same local
usage, each in a throwaway XDG_DATA_HOME so the real database is untouched. Reports every
(client, session, day) whose rows or tokens changed, the $ per client, and each build's AGY CLI rows
against AGY's own transcripts. Today is skipped: live sessions grow between runs.
Exit 0 when nothing changed and the candidate's AGY gaps are no worse; otherwise read the diff and
decide.
"""
import collections
import datetime
import glob
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TODAY = datetime.date.today().isoformat()


def ledger(binary, data_home):
    args = [binary, "--consumption", "--json", "--skip-import", "--days", "0", "--audit-pricing"]
    env = dict(os.environ, XDG_DATA_HOME=data_home)
    quotes = json.loads(subprocess.run(args, env=env, check=True, capture_output=True).stdout)["quotes"]
    db = f"file:{data_home}/token-monitor/token-monitor.sqlite3?mode=ro"
    payload = sqlite3.connect(db, uri=True).execute(
        "select payload_json from usage_snapshots order by id desc limit 1").fetchone()[0]
    records = json.loads(payload)["records"]
    assert len(records) == len(quotes), "quotes no longer line up with snapshot records"
    return [(r, q) for r, q in zip(records, quotes) if r["date"] < TODAY]


def client(record):
    return record["client"].split(" (")[0]


def summarize(rows):
    groups, dollars = collections.defaultdict(lambda: [0, 0]), collections.Counter()
    for r, q in rows:
        g = groups[(client(r), r["sessionId"][:8], r["date"])]
        g[0] += 1
        g[1] += sum(r["tokens"].values())
        dollars[client(r)] += q.get("valueUsd") or 0
    return groups, dollars


def agy_responses_per_day(days=7):
    """AGY's own count of model responses per local day: its append-only transcripts, which no
    dehydration or resume rewrites."""
    window = {(datetime.date.today() - datetime.timedelta(days=n)).isoformat() for n in range(1, days + 1)}
    counts = collections.Counter({d: 0 for d in window})
    pattern = os.path.expanduser("~/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript_full.jsonl")
    for path in glob.glob(pattern):
        for line in open(path, errors="ignore"):
            if '"PLANNER_RESPONSE"' not in line:
                continue
            step = json.loads(line)
            if step.get("type") == "PLANNER_RESPONSE":
                at = datetime.datetime.fromisoformat(step["created_at"].replace("Z", "+00:00"))
                day = at.astimezone().date().isoformat()
                if day in window:
                    counts[day] += 1
    return counts


def agy_gaps(rows, transcript):
    """Days where ledger rows and transcript responses disagree. Known history (AGY 1.2.15 overwrote
    usage on 10-02) shows up for both builds, so compare the builds' gaps, not each against zero."""
    ledger_days = collections.Counter(r["date"] for r, _ in rows if r["client"].startswith("antigravity-cli"))
    return {d: (n, ledger_days[d]) for d, n in sorted(transcript.items()) if n != ledger_days[d]}


def main():
    installed = shutil.which("token-monitor")
    if not installed:
        sys.exit("no installed token-monitor on PATH to compare against")
    subprocess.run(["cargo", "build", "--release", "--bin", "token-monitor"], cwd=REPO, check=True)
    with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
        old_rows = ledger(installed, a)
        new_rows = ledger(os.path.join(REPO, "target/release/token-monitor"), b)
    (old_groups, old_usd), (new_groups, new_usd) = summarize(old_rows), summarize(new_rows)

    changed = sorted(k for k in old_groups.keys() | new_groups.keys() if old_groups.get(k) != new_groups.get(k))
    print(f"{len(changed)} (client, session, day) groups changed")
    for k in changed[:40]:
        (r0, t0), (r1, t1) = old_groups.get(k, [0, 0]), new_groups.get(k, [0, 0])
        print(f"  {k[0]:24} {k[1]} {k[2]}  rows {r0:>5} -> {r1:<5}  tokens {t0 / 1e6:9.2f}M -> {t1 / 1e6:.2f}M")
    usd = [c for c in sorted(old_usd.keys() | new_usd.keys()) if round(old_usd[c], 2) != round(new_usd[c], 2)]
    for c in usd:
        print(f"  ${c:24} {old_usd[c]:10.2f} -> {new_usd[c]:.2f}")
    transcript = agy_responses_per_day()
    old_gaps, new_gaps = agy_gaps(old_rows, transcript), agy_gaps(new_rows, transcript)
    print("AGY (transcript responses, ledger rows) on mismatched days, last 7 full days:")
    print("  installed:", old_gaps or "all match")
    print("  candidate:", new_gaps or "all match")
    worse = sum(abs(t - n) for t, n in new_gaps.values()) > sum(abs(t - n) for t, n in old_gaps.values())
    sys.exit(1 if changed or usd or worse else 0)


if __name__ == "__main__":
    main()
