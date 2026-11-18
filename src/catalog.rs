use crate::language::tr;
use crate::{discovery, err, Result};
use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows_sys::Win32::System::Time::FileTimeToSystemTime;

pub struct SidebarCatalog {
    home: PathBuf,
    path: PathBuf,
}

pub struct CatalogRow {
    pub id: String,
    pub title: String,
    data: Map<String, Value>,
}

impl Default for SidebarCatalog {
    fn default() -> Self {
        let home = std::env::var_os("CODEX_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| discovery::home().join(".codex"));
        Self::new(home)
    }
}

impl SidebarCatalog {
    pub fn new(home: PathBuf) -> Self {
        let home = std::path::absolute(&home).unwrap_or(home);
        let path = home.join("sqlite/codex.db");
        Self { home, path }
    }

    fn protected_ids(&self) -> Result<HashSet<String>> {
        let version = Regex::new(r"^state_([0-9]+)\.sqlite$").unwrap();
        let state = fs::read_dir(&self.home)?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let number = version
                    .captures(entry.file_name().to_str()?)?
                    .get(1)?
                    .as_str()
                    .parse::<u64>()
                    .ok()?;
                Some((number, entry.path()))
            })
            .max()
            .map(|(_, path)| path)
            .ok_or_else(|| {
                err(tr(
                    "Не найдена основная база Codex. Очистка панели остановлена.",
                    "The main Codex database was not found. Sidebar cleanup stopped.",
                ))
            })?;
        let db = Connection::open_with_flags(state, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut statement = db.prepare("SELECT id FROM threads")?;
        let mut ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        let pattern = Regex::new(r"-([0-9a-fA-F-]{36})\.jsonl$").unwrap();
        for name in ["sessions", "archived_sessions"] {
            let root = self.home.join(name);
            match root.try_exists() {
                Ok(false) => {}
                Ok(true) => collect_rollouts(&root, &pattern, &mut ids)?,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(ids)
    }

    fn local_rows(connection: &Connection) -> Result<Vec<CatalogRow>> {
        let kind: Option<String> = connection
            .query_row(
                "SELECT host_kind FROM local_thread_catalog_hosts WHERE host_id = ?",
                ["local"],
                |row| row.get(0),
            )
            .optional()?;
        if kind.as_deref() != Some("local") {
            return Err(err(tr(
                "Не удалось проверить локальный каталог Codex. Очистка остановлена.",
                "Could not verify the local Codex catalog. Cleanup stopped.",
            )));
        }
        let mut statement =
            connection.prepare("SELECT * FROM local_thread_catalog WHERE host_id = ?")?;
        let columns: Vec<_> = statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut rows = statement.query(["local"])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let mut data = Map::new();
            for (index, name) in columns.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(value) => json!(value),
                    ValueRef::Real(value) => json!(value),
                    ValueRef::Text(value) => json!(std::str::from_utf8(value)?),
                    ValueRef::Blob(_) => {
                        return Err(err(tr(
                            "Каталог Codex содержит несовместимое поле.",
                            "The Codex catalog contains an incompatible field.",
                        )))
                    }
                };
                data.insert(name.clone(), value);
            }
            result.push(CatalogRow {
                id: row.get("thread_id")?,
                title: data
                    .get("display_title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                data,
            });
        }
        Ok(result)
    }

    pub fn stale(&self) -> Result<Vec<CatalogRow>> {
        if !self.path.try_exists()? {
            return Ok(Vec::new());
        }
        let protected = self.protected_ids()?;
        let db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self::local_rows(&db)?
            .into_iter()
            .filter(|row| !protected.contains(&row.id))
            .collect())
    }

    pub fn clean(&self, ids: HashSet<String>) -> Result<Vec<String>> {
        if ids.is_empty() || !self.path.try_exists()? {
            return Ok(Vec::new());
        }
        let mut db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(Duration::from_secs(10))?;
        let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let protected = self.protected_ids()?;
        let rows: Vec<_> = Self::local_rows(&transaction)?
            .into_iter()
            .filter(|row| ids.contains(&row.id) && !protected.contains(&row.id))
            .collect();
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let directory = self.home.join("chat-manager-backups");
        fs::create_dir_all(&directory)?;
        let backup = directory.join(backup_name()?);
        let data = json!({"database": self.path, "table": "local_thread_catalog",
            "rows": rows.iter().map(|row| &row.data).collect::<Vec<_>>()});
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(backup)?;
        serde_json::to_writer_pretty(&mut file, &data)?;
        file.flush()?;
        file.sync_all()?;
        {
            let mut delete = transaction
                .prepare("DELETE FROM local_thread_catalog WHERE host_id = ? AND thread_id = ?")?;
            for row in &rows {
                delete.execute(params!["local", row.id])?;
            }
        }
        transaction.execute(
            "UPDATE local_thread_catalog_metadata SET catalog_revision = catalog_revision + 1 WHERE id = 1", [])?;
        transaction.commit()?;
        Ok(rows.into_iter().map(|row| row.id).collect())
    }
}

fn backup_name() -> Result<String> {
    let time = unsafe {
        let (mut utc, mut local): (FILETIME, FILETIME) = std::mem::zeroed();
        let mut calendar: SYSTEMTIME = std::mem::zeroed();
        GetSystemTimePreciseAsFileTime(&mut utc);
        if FileTimeToLocalFileTime(&utc, &mut local) == 0
            || FileTimeToSystemTime(&local, &mut calendar) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let ticks = (u64::from(local.dwHighDateTime) << 32) | u64::from(local.dwLowDateTime);
        (calendar, ticks / 10 % 1_000_000)
    };
    let (date, micros) = time;
    Ok(format!(
        "sidebar-{:04}{:02}{:02}-{:02}{:02}{:02}-{micros:06}.json",
        date.wYear, date.wMonth, date.wDay, date.wHour, date.wMinute, date.wSecond
    ))
}

fn collect_rollouts(root: &Path, pattern: &Regex, ids: &mut HashSet<String>) -> Result<()> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
            } else if let Some(capture) = entry
                .file_name()
                .to_str()
                .and_then(|name| pattern.captures(name))
            {
                ids.insert(capture[1].to_owned());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEP: &str = "01a1b1e2-89de-7622-aa0b-758896a60685";
    const STALE: &str = "01a1b1b8-75a8-7743-8d57-1a96bc03a75a";
    const ROLLOUT: &str = "01a192a0-81db-7133-8f97-fe2f633e9db9";

    struct Fixture {
        _directory: tempfile::TempDir,
        catalog: SidebarCatalog,
        db: Connection,
        state: PathBuf,
        rollouts: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let home = directory.path();
            let catalog = SidebarCatalog::new(home.to_owned());
            let state = home.join("state_5.sqlite");
            {
                let db = Connection::open(&state).unwrap();
                db.execute("CREATE TABLE threads (id TEXT PRIMARY KEY)", [])
                    .unwrap();
                db.execute("INSERT INTO threads VALUES (?)", [KEEP])
                    .unwrap();
            }
            let rollouts = home.join("archived_sessions");
            fs::create_dir(&rollouts).unwrap();
            fs::write(
                rollouts.join(format!("rollout-test-{ROLLOUT}.jsonl")),
                "untouched",
            )
            .unwrap();
            fs::create_dir(home.join("sqlite")).unwrap();
            let db = Connection::open(&catalog.path).unwrap();
            db.execute_batch("
                CREATE TABLE local_thread_catalog_hosts (host_id TEXT PRIMARY KEY, host_kind TEXT);
                INSERT INTO local_thread_catalog_hosts VALUES ('local', 'local'), ('ssh:example', 'ssh');
                CREATE TABLE local_thread_catalog (
                    host_id TEXT, thread_id TEXT, display_title TEXT, PRIMARY KEY(host_id, thread_id));
                CREATE TABLE local_thread_catalog_metadata (id INTEGER PRIMARY KEY, catalog_revision INTEGER);
                INSERT INTO local_thread_catalog_metadata VALUES (1, 10);
                CREATE TABLE automations (id TEXT);
                INSERT INTO automations VALUES ('keep-automation');").unwrap();
            for (host, id, title) in [
                ("local", KEEP, "Existing chat"),
                ("local", STALE, "Deleted chat"),
                ("ssh:example", STALE, "Remote chat"),
                ("local", ROLLOUT, "Unindexed rollout"),
            ] {
                db.execute(
                    "INSERT INTO local_thread_catalog VALUES (?, ?, ?)",
                    params![host, id, title],
                )
                .unwrap();
            }
            Self {
                _directory: directory,
                catalog,
                db,
                state,
                rollouts,
            }
        }
        fn count(&self) -> i64 {
            self.db
                .query_row("SELECT count(*) FROM local_thread_catalog", [], |r| {
                    r.get(0)
                })
                .unwrap()
        }
        fn all_ids() -> HashSet<String> {
            [KEEP, STALE, ROLLOUT]
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
        fn backups(&self) -> PathBuf {
            self.catalog.home.join("chat-manager-backups")
        }
    }

    #[test]
    fn scan_is_read_only_and_cleanup_touches_only_local_stale_rows() {
        let fixture = Fixture::new();
        let preview = fixture.catalog.stale().unwrap();
        assert_eq!(
            preview.iter().map(|row| &row.id).collect::<Vec<_>>(),
            [STALE]
        );
        assert_eq!(fixture.count(), 4);
        assert!(!fixture.backups().exists());
        assert_eq!(fixture.catalog.clean(Fixture::all_ids()).unwrap(), [STALE]);
        assert_eq!(fixture.count(), 3);
        let remote: String = fixture
            .db
            .query_row(
                "SELECT display_title FROM local_thread_catalog
            WHERE host_id='ssh:example'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remote, "Remote chat");
        let automation: String = fixture
            .db
            .query_row("SELECT id FROM automations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(automation, "keep-automation");
        let rev: i64 = fixture
            .db
            .query_row(
                "SELECT catalog_revision FROM local_thread_catalog_metadata",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rev, 11);
        let backup = fs::read_dir(fixture.backups())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let json: Value = serde_json::from_reader(std::fs::File::open(backup).unwrap()).unwrap();
        assert_eq!(
            json["rows"],
            json!([{"host_id":"local","thread_id":STALE,"display_title":"Deleted chat"}])
        );
        let live: String = Connection::open(&fixture.state)
            .unwrap()
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(live, KEEP);
        assert_eq!(
            fs::read_to_string(
                fixture
                    .rollouts
                    .join(format!("rollout-test-{ROLLOUT}.jsonl"))
            )
            .unwrap(),
            "untouched"
        );
        assert!(fixture
            .catalog
            .clean(Fixture::all_ids())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rechecks_new_database_entries_and_rollouts_after_preview() {
        for database in [false, true] {
            let fixture = Fixture::new();
            let preview = fixture.catalog.stale().unwrap();
            if database {
                Connection::open(&fixture.state)
                    .unwrap()
                    .execute("INSERT INTO threads VALUES (?)", [STALE])
                    .unwrap();
            } else {
                fs::write(
                    fixture.rollouts.join(format!("rollout-test-{STALE}.jsonl")),
                    "",
                )
                .unwrap();
            }
            assert!(fixture
                .catalog
                .clean(preview.into_iter().map(|row| row.id).collect())
                .unwrap()
                .is_empty());
            assert_eq!(fixture.count(), 4);
        }
    }

    #[test]
    fn unrequested_rows_are_preserved_and_failures_roll_back() {
        let fixture = Fixture::new();
        assert!(fixture
            .catalog
            .clean(HashSet::from([ROLLOUT.to_owned()]))
            .unwrap()
            .is_empty());
        assert_eq!(fixture.count(), 4);
        for failure in ["source", "backup", "schema"] {
            let fixture = Fixture::new();
            match failure {
                "source" => fs::remove_file(&fixture.state).unwrap(),
                "backup" => fs::write(fixture.backups(), "not a directory").unwrap(),
                _ => fixture
                    .db
                    .execute("DROP TABLE local_thread_catalog_metadata", [])
                    .map(|_| ())
                    .unwrap(),
            }
            assert!(fixture.catalog.clean(Fixture::all_ids()).is_err());
            assert_eq!(fixture.count(), 4);
        }
    }
}
