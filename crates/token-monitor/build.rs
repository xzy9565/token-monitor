use std::process::Command;

// `--version` names the commit too: every build between version bumps otherwise reads the same.
// ponytail: no -dirty marker; installs are built from committed trees.
fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let version = env!("CARGO_PKG_VERSION");
    match git(&["rev-parse", "--short", "HEAD"]) {
        Some(commit) => println!("cargo:rustc-env=TM_VERSION={version} ({commit})"),
        None => println!("cargo:rustc-env=TM_VERSION={version}"),
    }
    // Every commit, checkout or reset appends to HEAD's reflog, so this re-stamps on each one.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
}
