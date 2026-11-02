//! A database SQLite left in rollback-journal mode goes back to that mode
//! after Turso closes it, with `DatabaseOpts::with_restore_rollback_journal`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use turso_core::{ConnectOpts, Database, DatabaseOpts, OpenFlags, SqliteDialect};

/// The write and read version bytes of the header (1 for a rollback journal,
/// 2 for WAL).
fn header_versions(path: &Path) -> (u8, u8) {
    let bytes = std::fs::read(path).unwrap();
    (bytes[18], bytes[19])
}

fn wal_path(path: &Path) -> PathBuf {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    PathBuf::from(wal)
}

fn create_rollback_journal_database(dir: &Path) -> PathBuf {
    let path = dir.join("rollback.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = DELETE; CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); \
         INSERT INTO t(b) VALUES ('from sqlite');",
    )
    .unwrap();
    drop(conn);
    assert_eq!(header_versions(&path), (1, 1));
    path
}

fn open_in_turso(path: &Path, restore: bool) -> Arc<Database> {
    Database::open_file_with_flags(
        Arc::new(turso_core::PlatformIO::new().unwrap()),
        path.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new().with_restore_rollback_journal(restore),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap()
}

/// What SQLite sees: its journal mode, integrity_check and the rows of `t`.
fn sqlite_sees(path: &Path) -> (String, String, Vec<String>) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let journal_mode = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let integrity = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    let rows = conn
        .prepare("SELECT b FROM t ORDER BY a")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (journal_mode, integrity, rows)
}

fn rows(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn test_rollback_journal_database_goes_back_to_rollback_journal_mode() {
    // Closed explicitly, or only dropped as the Rust crate does.
    for close in [true, false] {
        let dir = TempDir::new().unwrap();
        let path = create_rollback_journal_database(dir.path());
        let db = open_in_turso(&path, true);
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("INSERT INTO t(b) VALUES ('from turso')")
            .unwrap();
        if close {
            conn.close().unwrap();
        }
        drop(conn);
        assert_eq!(header_versions(&path), (1, 1), "close={close}");
        drop(db);

        assert!(!wal_path(&path).exists(), "close={close}");
        assert_eq!(
            sqlite_sees(&path),
            (
                "delete".to_string(),
                "ok".to_string(),
                rows(&["from sqlite", "from turso"])
            ),
            "close={close}"
        );
        assert!(!wal_path(&path).exists(), "close={close}");
    }
}

/// The database stays usable after its last connection closed: a new
/// connection writes through the WAL again, and the next close switches the
/// file back once more.
#[test]
fn test_rollback_journal_database_reconnected_after_restore() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso(&path, true);

    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("INSERT INTO t(b) VALUES ('first')").unwrap();
    conn.close().unwrap();
    drop(conn);
    assert_eq!(header_versions(&path), (1, 1));

    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("INSERT INTO t(b) VALUES ('second')").unwrap();
    // Not closed yet: the write is only in the WAL, which SQLite reads
    // because the file exists, whatever the header says. A copy of both
    // files is what a crash would leave.
    assert!(std::fs::metadata(wal_path(&path)).unwrap().len() > 0);
    let crashed = TempDir::new().unwrap();
    let crashed_path = crashed.path().join("rollback.db");
    std::fs::copy(&path, &crashed_path).unwrap();
    std::fs::copy(wal_path(&path), wal_path(&crashed_path)).unwrap();
    let (_, integrity, values) = sqlite_sees(&crashed_path);
    assert_eq!(integrity, "ok");
    assert_eq!(values, rows(&["from sqlite", "first", "second"]));
    conn.close().unwrap();
    drop(conn);
    drop(db);

    assert!(!wal_path(&path).exists());
    assert_eq!(
        sqlite_sees(&path),
        (
            "delete".to_string(),
            "ok".to_string(),
            rows(&["from sqlite", "first", "second"])
        )
    );
}

#[test]
fn test_rollback_journal_database_stays_wal_without_the_option() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso(&path, false);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("INSERT INTO t(b) VALUES ('from turso')")
        .unwrap();
    conn.close().unwrap();
    drop(db);
    assert_eq!(header_versions(&path), (2, 2));
}

#[test]
fn test_wal_database_stays_wal_with_the_option() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); \
         INSERT INTO t(b) VALUES ('from sqlite');",
    )
    .unwrap();
    drop(conn);
    let db = open_in_turso(&path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("INSERT INTO t(b) VALUES ('from turso')")
        .unwrap();
    conn.close().unwrap();
    drop(db);
    assert_eq!(header_versions(&path), (2, 2));
    let (journal_mode, integrity, values) = sqlite_sees(&path);
    assert_eq!((journal_mode.as_str(), integrity.as_str()), ("wal", "ok"));
    assert_eq!(values, rows(&["from sqlite", "from turso"]));
}

/// Through the Rust crate, which drops connections without closing them.
#[tokio::test]
async fn test_builder_restore_rollback_journal() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    {
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .restore_rollback_journal(true)
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("INSERT INTO t(b) VALUES ('from the crate')", ())
            .await
            .unwrap();
    }
    assert_eq!(header_versions(&path), (1, 1));
    assert!(!wal_path(&path).exists());
    assert_eq!(
        sqlite_sees(&path),
        (
            "delete".to_string(),
            "ok".to_string(),
            rows(&["from sqlite", "from the crate"])
        )
    );
}

/// Without the autovacuum flag, an auto-vacuum database opens read-only, so
/// its header must not be switched to WAL either.
#[test]
fn test_auto_vacuum_database_opened_read_only_keeps_its_journal_mode() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("autovacuum.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA auto_vacuum = FULL; PRAGMA journal_mode = DELETE; \
         CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); INSERT INTO t(b) VALUES ('from sqlite');",
    )
    .unwrap();
    drop(conn);
    assert_eq!(header_versions(&path), (1, 1));

    let db = open_in_turso(&path, false);
    assert!(db.is_readonly());
    let conn = db.connect(ConnectOpts::new()).unwrap();
    let mut stmt = conn.prepare("SELECT count(*) FROM t").unwrap();
    stmt.run_with_row_callback(|_| Ok(())).unwrap();
    drop(stmt);
    conn.close().unwrap();
    drop(conn);
    drop(db);

    assert_eq!(header_versions(&path), (1, 1));
    assert!(!wal_path(&path).exists());
}
