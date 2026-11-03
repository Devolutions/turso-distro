//! A database SQLite left in rollback-journal mode stays in that mode while
//! Turso uses it, with `DatabaseOpts::with_rollback_journal`, and SQLite
//! connections can use it at the same time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use turso_core::{
    ConnectOpts, Connection, Database, DatabaseOpts, LimboError, OpenFlags, SqliteDialect, Value,
};

/// The write and read version bytes of the header (1 for a rollback journal,
/// 2 for WAL).
fn header_versions(path: &Path) -> (u8, u8) {
    let bytes = std::fs::read(path).unwrap();
    (bytes[18], bytes[19])
}

fn side_file(path: &Path, suffix: &str) -> PathBuf {
    let mut side = path.as_os_str().to_owned();
    side.push(suffix);
    PathBuf::from(side)
}

fn wal_path(path: &Path) -> PathBuf {
    side_file(path, "-wal")
}

/// Whether a file SQLite would notice exists next to the database (SQLite
/// treats an empty file as missing).
fn has_side_file(path: &Path, suffix: &str) -> bool {
    std::fs::metadata(side_file(path, suffix)).is_ok_and(|metadata| metadata.len() > 0)
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

fn open_in_turso_with(path: &Path, flags: OpenFlags, rollback_journal: bool) -> Arc<Database> {
    Database::open_file_with_flags(
        Arc::new(turso_core::PlatformIO::new().unwrap()),
        path.to_str().unwrap(),
        flags,
        DatabaseOpts::new().with_rollback_journal(rollback_journal),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap()
}

fn open_in_turso(path: &Path, rollback_journal: bool) -> Arc<Database> {
    open_in_turso_with(path, OpenFlags::default(), rollback_journal)
}

/// A SQLite connection that reports SQLITE_BUSY at once instead of waiting.
fn open_in_sqlite(path: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    conn
}

fn turso_query(conn: &Arc<Connection>, sql: &str) -> Result<Vec<Vec<Value>>, LimboError> {
    conn.prepare(sql)?.run_collect_rows()
}

fn turso_texts(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    turso_query(conn, sql)
        .unwrap()
        .into_iter()
        .map(|row| row[0].to_string())
        .collect()
}

fn sqlite_texts(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// What a new SQLite connection sees: its journal mode, integrity_check and
/// the rows of `t`.
fn sqlite_sees(path: &Path) -> (String, String, Vec<String>) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let journal_mode = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let integrity = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    let rows = sqlite_texts(&conn, "SELECT b FROM t ORDER BY a");
    (journal_mode, integrity, rows)
}

fn rows(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn is_busy(result: &Result<Vec<Vec<Value>>, LimboError>) -> bool {
    matches!(result, Err(LimboError::Busy))
}

fn is_sqlite_busy<T>(result: &rusqlite::Result<T>) -> bool {
    matches!(
        result,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

#[test]
fn test_rollback_journal_database_stays_in_rollback_journal_mode() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso(&path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("INSERT INTO t(b) VALUES ('from turso')")
        .unwrap();
    // While Turso has the database open, the file is as SQLite left it: the
    // header still says rollback journal and the WAL is empty.
    assert_eq!(header_versions(&path), (1, 1));
    assert!(!has_side_file(&path, "-wal"));
    // Write transactions that change no page leave the WAL empty too.
    conn.execute("UPDATE t SET b = b WHERE a = 0").unwrap();
    conn.execute("BEGIN IMMEDIATE").unwrap();
    conn.execute("COMMIT").unwrap();
    assert!(!has_side_file(&path, "-wal"));
    assert_eq!(turso_texts(&conn, "PRAGMA journal_mode"), rows(&["delete"]));
    // Switching to WAL would leave SQLite's locks behind; the mode stays.
    assert_eq!(
        turso_texts(&conn, "PRAGMA journal_mode = wal"),
        rows(&["delete"])
    );
    assert_eq!(header_versions(&path), (1, 1));
    assert_eq!(
        sqlite_sees(&path),
        (
            "delete".to_string(),
            "ok".to_string(),
            rows(&["from sqlite", "from turso"])
        )
    );
    drop(conn);
    drop(db);
    assert!(!wal_path(&path).exists());
    assert_eq!(header_versions(&path), (1, 1));
}

/// Each engine sees what the other one commits, without reopening.
#[test]
fn test_rollback_journal_database_shared_with_sqlite() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso(&path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    let sqlite = open_in_sqlite(&path);
    assert_eq!(
        turso_texts(&conn, "SELECT b FROM t"),
        rows(&["from sqlite"])
    );

    sqlite
        .execute("UPDATE t SET b = 'renamed by sqlite'", [])
        .unwrap();
    assert_eq!(
        turso_texts(&conn, "SELECT b FROM t"),
        rows(&["renamed by sqlite"])
    );
    // A second database object for the file shares the first one.
    let other = open_in_turso(&path, true)
        .connect(ConnectOpts::new())
        .unwrap();
    assert_eq!(
        turso_texts(&other, "SELECT b FROM t"),
        rows(&["renamed by sqlite"])
    );

    conn.execute("INSERT INTO t(b) VALUES ('from turso')")
        .unwrap();
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT b FROM t ORDER BY a"),
        rows(&["renamed by sqlite", "from turso"])
    );

    // Schema changes too.
    sqlite
        .execute_batch("CREATE TABLE u(x); INSERT INTO u VALUES ('new table');")
        .unwrap();
    assert_eq!(turso_texts(&conn, "SELECT x FROM u"), rows(&["new table"]));
    conn.execute("CREATE INDEX t_b ON t(b)").unwrap();
    assert_eq!(
        sqlite_texts(
            &sqlite,
            "SELECT name FROM sqlite_schema WHERE type = 'index'"
        ),
        rows(&["t_b"])
    );
    assert_eq!(
        sqlite_texts(&sqlite, "PRAGMA integrity_check"),
        rows(&["ok"])
    );
}

#[test]
fn test_rollback_journal_database_locks_against_sqlite() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso(&path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    let sqlite = open_in_sqlite(&path);

    // A SQLite writer holding EXCLUSIVE keeps Turso from reading.
    sqlite
        .execute_batch("BEGIN EXCLUSIVE; UPDATE t SET b = 'pending';")
        .unwrap();
    assert!(is_busy(&turso_query(&conn, "SELECT b FROM t")));
    sqlite.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        turso_texts(&conn, "SELECT b FROM t"),
        rows(&["from sqlite"])
    );

    // A SQLite writer holding RESERVED lets Turso read the committed data,
    // but not write.
    sqlite
        .execute_batch("BEGIN IMMEDIATE; UPDATE t SET b = 'pending';")
        .unwrap();
    assert_eq!(
        turso_texts(&conn, "SELECT b FROM t"),
        rows(&["from sqlite"])
    );
    assert!(matches!(
        conn.execute("INSERT INTO t(b) VALUES ('from turso')"),
        Err(LimboError::Busy)
    ));
    sqlite.execute_batch("COMMIT").unwrap();
    assert_eq!(turso_texts(&conn, "SELECT b FROM t"), rows(&["pending"]));

    // A Turso read transaction keeps SQLite from committing.
    conn.execute("BEGIN").unwrap();
    assert_eq!(turso_texts(&conn, "SELECT b FROM t"), rows(&["pending"]));
    assert!(is_sqlite_busy(
        &sqlite.execute("UPDATE t SET b = 'blocked'", [])
    ));
    conn.execute("COMMIT").unwrap();
    sqlite.execute("UPDATE t SET b = 'committed'", []).unwrap();

    // A SQLite read transaction keeps Turso from committing until it ends.
    sqlite.execute_batch("BEGIN").unwrap();
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT b FROM t"),
        rows(&["committed"])
    );
    assert!(matches!(
        conn.execute("UPDATE t SET b = 'from turso'"),
        Err(LimboError::Busy)
    ));
    sqlite.execute_batch("COMMIT").unwrap();
    conn.execute("UPDATE t SET b = 'from turso'").unwrap();
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT b FROM t"),
        rows(&["from turso"])
    );
    assert!(!has_side_file(&path, "-wal"));
}

/// Opening works while a SQLite writer holds EXCLUSIVE, as in SQLite; the
/// first statement is busy until the writer is done.
#[test]
fn test_rollback_journal_open_while_sqlite_holds_exclusive() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let sqlite = open_in_sqlite(&path);
    sqlite
        .execute_batch("BEGIN EXCLUSIVE; UPDATE t SET b = 'pending';")
        .unwrap();
    let db = open_in_turso(&path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    assert!(is_busy(&turso_query(&conn, "SELECT b FROM t")));
    assert!(matches!(
        conn.execute("CREATE TABLE t(x)"),
        Err(LimboError::Busy)
    ));
    sqlite.execute_batch("COMMIT").unwrap();
    assert_eq!(turso_texts(&conn, "SELECT b FROM t"), rows(&["pending"]));
    assert!(conn.execute("CREATE TABLE t(x)").is_err());
}

/// The busy timeout lets a Turso commit wait for a SQLite reader.
#[test]
fn test_rollback_journal_commit_waits_for_sqlite_reader() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso(&path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.set_busy_timeout(Duration::from_secs(10));

    let reader = std::thread::spawn({
        let path = path.clone();
        move || {
            let sqlite = open_in_sqlite(&path);
            sqlite.execute_batch("BEGIN").unwrap();
            let before = sqlite_texts(&sqlite, "SELECT b FROM t");
            std::thread::sleep(Duration::from_millis(300));
            sqlite.execute_batch("COMMIT").unwrap();
            before
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    conn.execute("UPDATE t SET b = 'after the reader'").unwrap();
    assert_eq!(reader.join().unwrap(), rows(&["from sqlite"]));
    assert_eq!(sqlite_sees(&path).2, rows(&["after the reader"]));
}

/// A SQLite writer that died mid-transaction leaves a hot journal, which
/// Turso rolls back before it reads.
#[test]
fn test_rollback_journal_hot_journal_is_rolled_back() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200) \
             INSERT INTO t(b) SELECT printf('row %d %s', i, hex(randomblob(200))) FROM n;",
        )
        .unwrap();
    }
    let before = sqlite_sees(&path).2;

    // A small cache makes SQLite write changed pages into the database file
    // before the commit; a copy taken then is what a crash leaves behind.
    let crashed = TempDir::new().unwrap();
    let crashed_path = crashed.path().join("rollback.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA cache_size = 2; BEGIN; UPDATE t SET b = 'changed ' || b; DELETE FROM t WHERE a % 3 = 0;",
        )
        .unwrap();
        std::fs::copy(&path, &crashed_path).unwrap();
        std::fs::copy(
            side_file(&path, "-journal"),
            side_file(&crashed_path, "-journal"),
        )
        .unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
    }
    assert!(has_side_file(&crashed_path, "-journal"));
    assert_ne!(
        std::fs::read(&crashed_path).unwrap(),
        std::fs::read(&path).unwrap()
    );

    let db = open_in_turso(&crashed_path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    assert_eq!(turso_texts(&conn, "SELECT b FROM t ORDER BY a"), before);
    assert!(!side_file(&crashed_path, "-journal").exists());
    drop(conn);
    drop(db);
    assert_eq!(
        sqlite_sees(&crashed_path),
        ("delete".to_string(), "ok".to_string(), before)
    );
}

/// A crash in the middle of a commit leaves frames in the WAL; Turso
/// checkpoints them before it reads.
#[test]
fn test_rollback_journal_leftover_wal_is_checkpointed() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    // SQLite in WAL mode writes the WAL a Turso crash would leave: frames
    // for pages other than page 1, next to a database file whose header says
    // rollback journal.
    let crashed = TempDir::new().unwrap();
    let crashed_path = crashed.path().join("rollback.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        conn.execute("INSERT INTO t(b) VALUES ('in the wal')", [])
            .unwrap();
        let mut database = std::fs::read(&path).unwrap();
        database[18] = 1;
        database[19] = 1;
        std::fs::write(&crashed_path, database).unwrap();
        std::fs::copy(wal_path(&path), wal_path(&crashed_path)).unwrap();
    }
    assert!(has_side_file(&crashed_path, "-wal"));

    let db = open_in_turso(&crashed_path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    assert_eq!(
        turso_texts(&conn, "SELECT b FROM t ORDER BY a"),
        rows(&["from sqlite", "in the wal"])
    );
    assert!(!has_side_file(&crashed_path, "-wal"));
    drop(conn);
    drop(db);
    assert_eq!(header_versions(&crashed_path), (1, 1));
    assert_eq!(
        sqlite_sees(&crashed_path),
        (
            "delete".to_string(),
            "ok".to_string(),
            rows(&["from sqlite", "in the wal"])
        )
    );
}

/// A read-only database sees SQLite's commits too.
#[test]
fn test_rollback_journal_read_only_database_sees_sqlite_commits() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso_with(&path, OpenFlags::ReadOnly, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    assert_eq!(
        turso_texts(&conn, "SELECT b FROM t"),
        rows(&["from sqlite"])
    );
    let sqlite = open_in_sqlite(&path);
    sqlite.execute("UPDATE t SET b = 'renamed'", []).unwrap();
    assert_eq!(turso_texts(&conn, "SELECT b FROM t"), rows(&["renamed"]));
    sqlite
        .execute_batch("BEGIN EXCLUSIVE; UPDATE t SET b = 'pending';")
        .unwrap();
    assert!(is_busy(&turso_query(&conn, "SELECT b FROM t")));
    sqlite.execute_batch("ROLLBACK").unwrap();
    drop(conn);
    drop(db);
    assert!(!wal_path(&path).exists());
}

/// Transactions bigger than the page cache spill pages to the WAL, which
/// needs EXCLUSIVE too.
#[test]
fn test_rollback_journal_large_transaction() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let db = open_in_turso(&path, true);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA cache_size = 10").unwrap();
    conn.execute("BEGIN").unwrap();
    for i in 0..2000 {
        conn.execute(format!(
            "INSERT INTO t(b) VALUES ('row {i} {}')",
            "x".repeat(300)
        ))
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    assert!(!has_side_file(&path, "-wal"));
    let (journal_mode, integrity, values) = sqlite_sees(&path);
    assert_eq!(
        (journal_mode.as_str(), integrity.as_str()),
        ("delete", "ok")
    );
    assert_eq!(values.len(), 2001);

    // A rolled-back transaction leaves nothing behind either.
    conn.execute("BEGIN").unwrap();
    conn.execute("DELETE FROM t").unwrap();
    conn.execute("ROLLBACK").unwrap();
    assert!(!has_side_file(&path, "-wal"));
    assert_eq!(sqlite_sees(&path).2.len(), 2001);
}

#[test]
fn test_rollback_journal_database_goes_to_wal_without_the_option() {
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

/// Through the Rust crate.
#[tokio::test]
async fn test_builder_rollback_journal() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    {
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .rollback_journal(true)
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("INSERT INTO t(b) VALUES ('from the crate')", ())
            .await
            .unwrap();
        assert_eq!(header_versions(&path), (1, 1));
        assert_eq!(
            sqlite_sees(&path).2,
            rows(&["from sqlite", "from the crate"])
        );
    }
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
    assert_eq!(turso_texts(&conn, "SELECT count(*) FROM t"), rows(&["1"]));
    conn.close().unwrap();
    drop(conn);
    drop(db);

    assert_eq!(header_versions(&path), (1, 1));
    assert!(!wal_path(&path).exists());
}

/// VACUUM rewrites the file under EXCLUSIVE, and a SQLite connection that
/// cached pages before sees the new file.
#[test]
fn test_rollback_journal_vacuum() {
    let dir = TempDir::new().unwrap();
    let path = create_rollback_journal_database(dir.path());
    let sqlite = open_in_sqlite(&path);
    sqlite
        .execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 500) \
             INSERT INTO t(b) SELECT printf('row %d %s', i, hex(randomblob(100))) FROM n; \
             DELETE FROM t WHERE a % 2 = 0;",
        )
        .unwrap();
    let expected = sqlite_texts(&sqlite, "SELECT b FROM t ORDER BY a");
    let db = Database::open_file_with_flags(
        Arc::new(turso_core::PlatformIO::new().unwrap()),
        path.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new()
            .with_rollback_journal(true)
            .with_vacuum(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();

    // A SQLite read transaction keeps VACUUM out.
    sqlite.execute_batch("BEGIN").unwrap();
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT b FROM t ORDER BY a"),
        expected
    );
    assert!(matches!(conn.execute("VACUUM"), Err(LimboError::Busy)));
    sqlite.execute_batch("COMMIT").unwrap();

    let size_before = std::fs::metadata(&path).unwrap().len();
    conn.execute("VACUUM").unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < size_before);
    assert!(!has_side_file(&path, "-wal"));
    assert_eq!(header_versions(&path), (1, 1));
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT b FROM t ORDER BY a"),
        expected
    );
    assert_eq!(
        sqlite_texts(&sqlite, "PRAGMA integrity_check"),
        rows(&["ok"])
    );
    assert_eq!(turso_texts(&conn, "SELECT b FROM t ORDER BY a"), expected);
}

/// Opens a database through the SDK kit with `features`, as the RDM devo
/// does.
fn open_in_sdk(
    path: &Path,
    flags: OpenFlags,
    features: &str,
) -> Result<Arc<turso_sdk_kit::rsapi::TursoDatabase>, turso_sdk_kit::rsapi::TursoError> {
    let db = turso_sdk_kit::rsapi::TursoDatabase::new(turso_sdk_kit::rsapi::TursoDatabaseConfig {
        path: path.to_str().unwrap().to_string(),
        experimental_features: Some(features.to_string()),
        async_io: false,
        encryption: None,
        vfs: turso::IoBackend::Default,
        io: None,
        db_file: None,
        page_codec: None,
        open_flags: flags,
    });
    let _ = db.open()?;
    Ok(db)
}

fn sdk_execute(db: &turso_sdk_kit::rsapi::TursoDatabase, sql: &str) {
    let conn = db.connect().unwrap();
    for statement in sql.split(';').filter(|s| !s.trim().is_empty()) {
        conn.prepare_single(statement)
            .unwrap()
            .execute(None)
            .unwrap();
    }
}

/// A database Turso creates in rollback-journal mode is a rollback-journal
/// database, as SQLite creates one: from a missing file or an empty one.
#[test]
fn test_rollback_journal_new_database_starts_in_rollback_journal_mode() {
    for empty_file in [false, true] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.db");
        if empty_file {
            std::fs::write(&path, b"").unwrap();
        }
        {
            let db = open_in_sdk(&path, OpenFlags::Create, "autovacuum,rollback_journal").unwrap();
            sdk_execute(
                &db,
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); INSERT INTO t(b) VALUES ('from turso')",
            );
            assert_eq!(header_versions(&path), (1, 1), "empty_file={empty_file}");
            assert!(!has_side_file(&path, "-wal"), "empty_file={empty_file}");
        }
        assert!(!wal_path(&path).exists(), "empty_file={empty_file}");
        assert!(
            !side_file(&path, "-shm").exists(),
            "empty_file={empty_file}"
        );
        assert_eq!(header_versions(&path), (1, 1), "empty_file={empty_file}");
        assert_eq!(
            sqlite_sees(&path),
            (
                "delete".to_string(),
                "ok".to_string(),
                rows(&["from turso"])
            ),
            "empty_file={empty_file}"
        );
    }
}

/// Without `OpenFlags::Create`, an existing database opens read-write, as
/// with SQLITE_OPEN_READWRITE; only a missing database file fails.
#[test]
fn test_open_existing_database_without_create() {
    for features in ["autovacuum,rollback_journal", "autovacuum"] {
        let dir = TempDir::new().unwrap();
        let path = create_rollback_journal_database(dir.path());
        {
            let db = open_in_sdk(&path, OpenFlags::None, features)
                .unwrap_or_else(|err| panic!("features={features}: {err:?}"));
            sdk_execute(&db, "INSERT INTO t(b) VALUES ('from turso')");
        }
        assert_eq!(
            sqlite_sees(&path).2,
            rows(&["from sqlite", "from turso"]),
            "features={features}"
        );
        let missing = dir.path().join("missing.db");
        assert!(open_in_sdk(&missing, OpenFlags::None, features).is_err());
        assert!(!missing.exists());
    }
}

/// The option does not apply to in-memory databases, which have no file to
/// lock.
#[test]
fn test_rollback_journal_option_on_in_memory_database() {
    let db = Database::open_file_with_flags(
        Arc::new(turso_core::MemoryIO::new()),
        ":memory:",
        OpenFlags::default(),
        DatabaseOpts::new().with_rollback_journal(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE t(a)").unwrap();
    conn.execute("INSERT INTO t VALUES ('in memory')").unwrap();
    assert_eq!(turso_texts(&conn, "SELECT a FROM t"), rows(&["in memory"]));
    assert_eq!(turso_texts(&conn, "PRAGMA journal_mode"), rows(&["wal"]));
}

/// DDL run through both engines by `test_rollback_journal_schema_text_matches_sqlite`:
/// odd spacing and case, comments, IF NOT EXISTS, schema-qualified names,
/// triggers with comments, a view, AUTOINCREMENT and every ALTER TABLE form.
const SCHEMA_SCRIPT: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS Connections (\n    ID BLOB NOT NULL PRIMARY KEY, -- the key\n    Name TEXT   COLLATE NOCASE,\n    \"Data\" TEXT,\n    [Key] TEXT\n);",
    "CREATE TABLE ConnectionHistory (ID INTEGER PRIMARY KEY AUTOINCREMENT, ConnectionID BLOB, Version int default 0)",
    "create table main.Handbook(ID INTEGER PRIMARY KEY AUTOINCREMENT, Title TEXT);",
    "CREATE INDEX IF NOT EXISTS IX_ConnectionHistory on ConnectionHistory (ConnectionID asc);",
    "CREATE UNIQUE INDEX main.IX_Name ON Connections(Name) WHERE Name IS NOT NULL ;",
    "CREATE TABLE t2(a, b, CONSTRAINT pk PRIMARY KEY (a));",
    "CREATE VIEW v1 AS\n   SELECT Name FROM Connections   ;",
    "CREATE TRIGGER IF NOT EXISTS Connections_Trigger_Delete DELETE ON Connections\nBEGIN\n    -- v1\n\n    -- comment\n    UPDATE ConnectionHistory SET Version = Version + 1 WHERE ConnectionID = old.ID;\nEND;",
    "ALTER TABLE Connections ADD COLUMN Extra TEXT DEFAULT 'x' ;",
    "ALTER TABLE t2 ADD c INTEGER",
    "ALTER TABLE ConnectionHistory RENAME COLUMN Version TO Revision",
    "ALTER TABLE Connections RENAME COLUMN Name TO \"Display Name\"",
    "ALTER TABLE t2 RENAME TO t3",
    "ALTER TABLE t3 DROP COLUMN c",
];

fn schema_rows(conn: &rusqlite::Connection) -> Vec<(String, String, String, Option<String>)> {
    conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// sqlite_master.sql holds the statement text as written, from the object
/// name on, and ALTER TABLE edits it in place, as in SQLite.
#[test]
fn test_rollback_journal_schema_text_matches_sqlite() {
    let dir = TempDir::new().unwrap();
    let sqlite_path = dir.path().join("sqlite.db");
    let turso_path = dir.path().join("turso.db");
    let sqlite = rusqlite::Connection::open(&sqlite_path).unwrap();
    let db = Database::open_file_with_flags(
        Arc::new(turso_core::PlatformIO::new().unwrap()),
        turso_path.to_str().unwrap(),
        OpenFlags::Create,
        DatabaseOpts::new()
            .with_rollback_journal(true)
            .with_views(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    for statement in SCHEMA_SCRIPT {
        sqlite.execute_batch(statement).unwrap();
        conn.execute(statement)
            .unwrap_or_else(|err| panic!("{statement}: {err}"));
    }
    drop(conn);
    drop(db);
    let turso = rusqlite::Connection::open(&turso_path).unwrap();
    let expected = schema_rows(&sqlite);
    let actual = schema_rows(&turso);
    for (expected, actual) in expected.iter().zip(&actual) {
        assert_eq!(actual, expected);
    }
    assert_eq!(actual.len(), expected.len(), "{actual:#?}");
    assert_eq!(
        sqlite_texts(&turso, "PRAGMA integrity_check"),
        rows(&["ok"])
    );
}

/// AUTOINCREMENT keeps its counters in sqlite_sequence, as in SQLite, and
/// both engines continue each other's counters.
#[test]
fn test_rollback_journal_autoincrement_uses_sqlite_sequence() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("autoinc.db");
    {
        let db = open_in_sdk(&path, OpenFlags::Create, "rollback_journal").unwrap();
        sdk_execute(
            &db,
            "CREATE TABLE h(ID INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT); \
             INSERT INTO h(v) VALUES ('a'); INSERT INTO h(v) VALUES ('b'); \
             INSERT INTO h(v) VALUES ('c'); DELETE FROM h WHERE ID = 3",
        );
    }
    let sqlite = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        sqlite_texts(
            &sqlite,
            "SELECT name FROM sqlite_master WHERE name LIKE '%turso%' OR name = 'sqlite_sequence'"
        ),
        rows(&["sqlite_sequence"])
    );
    assert_eq!(
        sqlite_texts(
            &sqlite,
            "SELECT sql FROM sqlite_master WHERE name = 'sqlite_sequence'"
        ),
        rows(&["CREATE TABLE sqlite_sequence(name,seq)"])
    );
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT name || '=' || seq FROM sqlite_sequence"),
        rows(&["h=3"])
    );
    sqlite
        .execute("INSERT INTO h(v) VALUES ('from sqlite')", [])
        .unwrap();
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT CAST(ID AS TEXT) FROM h ORDER BY ID"),
        rows(&["1", "2", "4"])
    );
    drop(sqlite);
    {
        let db = open_in_sdk(&path, OpenFlags::None, "rollback_journal").unwrap();
        sdk_execute(
            &db,
            "DELETE FROM h WHERE ID = 4; INSERT INTO h(v) VALUES ('turso again')",
        );
    }
    let sqlite = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT CAST(ID AS TEXT) FROM h ORDER BY ID"),
        rows(&["1", "2", "5"])
    );
    assert_eq!(
        sqlite_texts(&sqlite, "SELECT name || '=' || seq FROM sqlite_sequence"),
        rows(&["h=5"])
    );
    assert_eq!(
        sqlite_texts(&sqlite, "PRAGMA integrity_check"),
        rows(&["ok"])
    );
}
