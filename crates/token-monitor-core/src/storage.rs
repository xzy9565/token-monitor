//! Token Monitor-owned persistent state.
//!
//! The desktop app's archive is treated as an import source only. Runtime
//! snapshots and usage aggregates live in this SQLite database so the TUI has
//! no ongoing dependency on Electron's files or synchronous JSON rewrites.

use crate::usage::UsageSnapshot;
use crate::ProviderSnapshot;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoragePaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub database: PathBuf,
}

impl StoragePaths {
    pub fn discover() -> Result<Self, String> {
        let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("token-monitor");
        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("token-monitor");
        let cache_dir = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("token-monitor");
        Ok(Self {
            config_dir,
            data_dir: data_dir.clone(),
            cache_dir,
            database: data_dir.join("token-monitor.sqlite3"),
        })
    }

    pub fn ensure_dirs(&self) -> Result<(), String> {
        for path in [&self.config_dir, &self.data_dir, &self.cache_dir] {
            fs::create_dir_all(path)
                .map_err(|error| format!("create {}: {error}", path.display()))?;
        }
        Ok(())
    }
}

pub struct Storage {
    connection: Connection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub source: PathBuf,
    pub digest: String,
    pub bytes: usize,
    pub imported: bool,
}

impl Storage {
    pub fn open_default() -> Result<Self, String> {
        let paths = StoragePaths::discover()?;
        paths.ensure_dirs()?;
        Self::open_at(&paths.database)
    }

    pub fn open_at(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let connection =
            Connection::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|error| format!("configure SQLite timeout: {error}"))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| format!("enable SQLite WAL: {error}"))?;
        let storage = Self { connection };
        storage.migrate()?;
        Ok(storage)
    }

    fn migrate(&self) -> Result<(), String> {
        self.connection
            .execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS schema_meta (
                   key TEXT PRIMARY KEY NOT NULL,
                   value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS provider_snapshots (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   collected_at_ms INTEGER NOT NULL,
                   provider_id TEXT NOT NULL,
                   account_key TEXT NOT NULL,
                   source_health TEXT NOT NULL,
                   availability TEXT NOT NULL,
                   payload_json TEXT NOT NULL,
                   payload_sha256 TEXT NOT NULL UNIQUE
                 );
                 CREATE INDEX IF NOT EXISTS provider_snapshots_lookup
                   ON provider_snapshots(provider_id, account_key, collected_at_ms DESC);
                 CREATE TABLE IF NOT EXISTS usage_snapshots (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   collected_at_ms INTEGER NOT NULL,
                   payload_json TEXT NOT NULL,
                   payload_sha256 TEXT NOT NULL UNIQUE
                 );
                 CREATE TABLE IF NOT EXISTS legacy_imports (
                   source_path TEXT PRIMARY KEY NOT NULL,
                   source_sha256 TEXT NOT NULL,
                   imported_at_ms INTEGER NOT NULL,
                   bytes INTEGER NOT NULL,
                   payload_json TEXT NOT NULL
                 );
                 INSERT INTO schema_meta(key, value)
                   VALUES ('schema_version', '1')
                   ON CONFLICT(key) DO UPDATE SET value=excluded.value;
                 COMMIT;",
            )
            .map_err(|error| format!("migrate SQLite: {error}"))
    }

    pub fn save_provider_snapshots(&self, snapshots: &[ProviderSnapshot]) -> Result<usize, String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| format!("begin snapshot transaction: {error}"))?;
        let mut inserted = 0;
        for snapshot in snapshots {
            let payload = serde_json::to_string(snapshot)
                .map_err(|error| format!("encode snapshot: {error}"))?;
            let digest = sha256(payload.as_bytes());
            let source_health = format!("{:?}", snapshot.source_health);
            let availability = format!("{:?}", snapshot.availability);
            inserted += transaction
                .execute(
                    "INSERT OR IGNORE INTO provider_snapshots
                     (collected_at_ms, provider_id, account_key, source_health, availability, payload_json, payload_sha256)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        snapshot.collected_at_ms,
                        snapshot.provider_id,
                        snapshot.account_key,
                        source_health,
                        availability,
                        payload,
                        digest,
                    ],
                )
                .map_err(|error| format!("insert provider snapshot: {error}"))?;
        }
        transaction
            .commit()
            .map_err(|error| format!("commit snapshot transaction: {error}"))?;
        Ok(inserted)
    }

    pub fn latest_provider_snapshots(&self) -> Result<Vec<ProviderSnapshot>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload_json FROM provider_snapshots
                 WHERE id IN (SELECT max(id) FROM provider_snapshots GROUP BY provider_id, account_key)
                 ORDER BY collected_at_ms DESC",
            )
            .map_err(|error| format!("prepare snapshot query: {error}"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| format!("query snapshots: {error}"))?;
        rows.map(|row| {
            let payload = row.map_err(|error| format!("read snapshot: {error}"))?;
            serde_json::from_str(&payload).map_err(|error| format!("decode snapshot: {error}"))
        })
        .collect()
    }

    pub fn save_usage_snapshot(
        &self,
        snapshot: &UsageSnapshot,
        collected_at_ms: i64,
    ) -> Result<bool, String> {
        let payload = serde_json::to_string(snapshot)
            .map_err(|error| format!("encode usage snapshot: {error}"))?;
        let digest = sha256(payload.as_bytes());
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO usage_snapshots(collected_at_ms, payload_json, payload_sha256)
                 VALUES (?1, ?2, ?3)",
                params![collected_at_ms, payload, digest],
            )
            .map_err(|error| format!("store usage snapshot: {error}"))?;
        Ok(inserted > 0)
    }

    pub fn latest_usage_snapshot(&self) -> Result<Option<UsageSnapshot>, String> {
        let payload = self
            .connection
            .query_row(
                "SELECT payload_json FROM usage_snapshots ORDER BY collected_at_ms DESC, id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("query latest usage snapshot: {error}"))?;
        payload
            .map(|payload| {
                serde_json::from_str(&payload)
                    .map_err(|error| format!("decode usage snapshot: {error}"))
            })
            .transpose()
    }

    pub fn import_legacy_json(&self, source: &Path, now_ms: i64) -> Result<ImportReport, String> {
        let bytes =
            fs::read(source).map_err(|error| format!("read {}: {error}", source.display()))?;
        let digest = sha256(&bytes);
        let existing = self
            .connection
            .query_row(
                "SELECT source_sha256 FROM legacy_imports WHERE source_path=?1",
                params![source.to_string_lossy().as_ref()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("check legacy import: {error}"))?;
        if existing.as_deref() == Some(digest.as_str()) {
            return Ok(ImportReport {
                source: source.to_owned(),
                digest,
                bytes: bytes.len(),
                imported: false,
            });
        }
        let value: serde_json::Value = if let Ok(value) = serde_json::from_slice(&bytes) {
            value
        } else {
            let rows = String::from_utf8_lossy(&bytes)
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>();
            if rows.is_empty() {
                return Err(format!(
                    "legacy source is not valid JSON/JSONL: {}",
                    source.display()
                ));
            }
            serde_json::json!({"format": "jsonl", "rows": rows})
        };
        let payload = serde_json::to_string(&value)
            .map_err(|error| format!("normalize legacy JSON: {error}"))?;
        self.connection
            .execute(
                "INSERT INTO legacy_imports(source_path, source_sha256, imported_at_ms, bytes, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(source_path) DO UPDATE SET source_sha256=excluded.source_sha256,
                   imported_at_ms=excluded.imported_at_ms, bytes=excluded.bytes, payload_json=excluded.payload_json",
                params![source.to_string_lossy().as_ref(), digest, now_ms, bytes.len() as i64, payload],
            )
            .map_err(|error| format!("store legacy import: {error}"))?;
        Ok(ImportReport {
            source: source.to_owned(),
            digest,
            bytes: bytes.len(),
            imported: true,
        })
    }

    pub fn imported_payload(&self, source: &Path) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT payload_json FROM legacy_imports WHERE source_path=?1",
                params![source.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("read legacy import: {error}"))
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Availability, LimitWindow, ProviderSnapshot, SourceHealth, WindowKind, WindowMetric,
    };

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "token-monitor-{name}-{}-{}.sqlite3",
            std::process::id(),
            chrono_like_now()
        ))
    }

    fn chrono_like_now() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn snapshot() -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "fixture".into(),
            account_key: "account".into(),
            account_label: "Fixture · Pro".into(),
            plan: "Pro".into(),
            source: "test".into(),
            collected_at_ms: 1,
            source_health: SourceHealth::Connected,
            availability: Availability::Available,
            windows: vec![LimitWindow {
                label: "Weekly".into(),
                kind: WindowKind::Weekly,
                metric: WindowMetric::Quota,
                remaining_percent: Some(75.0),
                remaining_amount: None,
                currency: None,
                resets_at_ms: Some(2),
                reset_text: None,
                estimated: false,
            }],
            diagnostics: vec![],
            hue: 45,
        }
    }

    #[test]
    fn provider_snapshots_round_trip_and_deduplicate() {
        let path = temp_db("snapshot");
        let storage = Storage::open_at(&path).unwrap();
        assert_eq!(
            storage
                .save_provider_snapshots(&[snapshot(), snapshot()])
                .unwrap(),
            1
        );
        assert_eq!(
            storage.latest_provider_snapshots().unwrap(),
            vec![snapshot()]
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn legacy_import_is_idempotent() {
        let db = temp_db("import");
        let source = db.with_extension("json");
        fs::write(&source, br#"{"days":[]}"#).unwrap();
        let storage = Storage::open_at(&db).unwrap();
        let first = storage.import_legacy_json(&source, 1).unwrap();
        let second = storage.import_legacy_json(&source, 2).unwrap();
        assert!(first.imported);
        assert!(!second.imported);
        assert_eq!(
            storage.imported_payload(&source).unwrap().unwrap(),
            r#"{"days":[]}"#
        );
        let _ = fs::remove_file(&db);
        let _ = fs::remove_file(db.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(db.with_extension("sqlite3-shm"));
        let _ = fs::remove_file(source);
    }

    #[test]
    fn jsonl_import_keeps_valid_rows() {
        let db = temp_db("jsonl");
        let source = db.with_extension("jsonl");
        fs::write(&source, b"{\"provider\":\"codex\"}\nnot-json\n").unwrap();
        let storage = Storage::open_at(&db).unwrap();
        let report = storage.import_legacy_json(&source, 1).unwrap();
        assert!(report.imported);
        let payload = storage.imported_payload(&source).unwrap().unwrap();
        assert!(payload.contains("jsonl"));
        assert!(payload.contains("codex"));
        assert!(!payload.contains("not-json"));
        let _ = fs::remove_file(&db);
        let _ = fs::remove_file(db.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(db.with_extension("sqlite3-shm"));
        let _ = fs::remove_file(source);
    }

    #[test]
    fn usage_snapshot_writes_idempotently() {
        let path = temp_db("usage");
        let storage = Storage::open_at(&path).unwrap();
        let snapshot = UsageSnapshot {
            records: vec![],
            processing_time_ms: 1,
            tokscale_revision: "test".into(),
        };
        assert!(storage.save_usage_snapshot(&snapshot, 1).unwrap());
        assert!(!storage.save_usage_snapshot(&snapshot, 2).unwrap());
        assert_eq!(storage.latest_usage_snapshot().unwrap(), Some(snapshot));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
