use crate::common::TempDatabase;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use turso_core::SqliteDialect;
use turso_core::{Database, DatabaseOpts, OpenFlags, PlatformIO};

#[test]
fn test_autovacuum_readonly_behavior() {
    // (autovacuum_mode, enable_autovacuum_flag, expected_readonly)
    // TODO: Add encrypted case ("NONE", false, false) after fixing https://github.com/tursodatabase/turso/issues/4519
    let test_cases = [
        ("NONE", false, false),
        ("NONE", true, false),
        ("FULL", false, true),
        ("FULL", true, false),
        ("INCREMENTAL", false, true),
        ("INCREMENTAL", true, false),
    ];

    for (autovacuum_mode, enable_autovacuum_flag, expected_readonly) in test_cases {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "auto_vacuum", autovacuum_mode)
                .unwrap();
        }

        let io = Arc::new(PlatformIO::new().unwrap()) as Arc<dyn turso_core::IO>;
        let opts = DatabaseOpts::new().with_autovacuum(enable_autovacuum_flag);
        let db = Database::open_file_with_flags(
            io,
            db_path.to_str().unwrap(),
            OpenFlags::default(),
            opts,
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();

        assert_eq!(
            db.is_readonly(),
            expected_readonly,
            "autovacuum={autovacuum_mode}, flag={enable_autovacuum_flag}: expected readonly={expected_readonly}"
        );
    }
}

/// What real SQLite reports about a database file.
#[derive(Debug, PartialEq, Eq)]
struct SqliteView {
    integrity_check: String,
    page_count: i64,
    freelist_count: i64,
    auto_vacuum: i64,
    root_pages: Vec<(String, i64)>,
    row_count: usize,
    rows_hash: u64,
}

fn sqlite_view(path: &Path) -> SqliteView {
    let conn = rusqlite::Connection::open(path).unwrap();
    let scalar = |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
    let integrity_check = conn
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .join("\n");
    let root_pages: Vec<(String, i64)> = conn
        .prepare("SELECT name, rootpage FROM sqlite_schema WHERE rootpage > 0 ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut rows = Vec::new();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for table in tables {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM \"{table}\" ORDER BY rowid"))
            .unwrap();
        let columns = stmt.column_count();
        let mut result = stmt.query([]).unwrap();
        while let Some(row) = result.next().unwrap() {
            let values: Vec<String> = (0..columns)
                .map(|idx| format!("{:?}", row.get_ref(idx).unwrap()))
                .collect();
            rows.push(format!("{table}: {}", values.join(", ")));
        }
    }
    SqliteView {
        integrity_check,
        page_count: scalar("PRAGMA page_count"),
        freelist_count: scalar("PRAGMA freelist_count"),
        auto_vacuum: scalar("PRAGMA auto_vacuum"),
        root_pages,
        row_count: rows.len(),
        rows_hash: {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            rows.hash(&mut hasher);
            hasher.finish()
        },
    }
}

/// Creates a database with real SQLite, the way RDM creates its files.
fn create_sqlite_database(
    dir: &Path,
    name: &str,
    auto_vacuum: &str,
    journal_mode: &str,
) -> PathBuf {
    let path = dir.join(name);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA page_size = 1024; PRAGMA auto_vacuum = {auto_vacuum}; \
         PRAGMA journal_mode = {journal_mode}; CREATE TABLE seed(x); DROP TABLE seed;"
    ))
    .unwrap();
    path
}

fn run_in_turso(path: &Path, statements: &[&str]) {
    let opts = DatabaseOpts::new().with_autovacuum(true);
    let db = TempDatabase::new_with_existent_with_opts(path, opts);
    let conn = db.connect_limbo();
    for statement in statements {
        conn.execute(statement)
            .unwrap_or_else(|err| panic!("{statement}: {err}"));
    }
    conn.close().unwrap();
}

/// Steps every statement to completion: `PRAGMA incremental_vacuum` moves
/// one page per row it returns.
fn run_in_sqlite(path: &Path, statements: &[&str]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    for statement in statements {
        let mut stmt = conn.prepare(statement).unwrap();
        let mut rows = stmt.raw_query();
        while rows.next().unwrap().is_some() {}
    }
}

fn run_both(
    dir: &Path,
    auto_vacuum: &str,
    journal_mode: &str,
    statements: &[&str],
) -> (SqliteView, SqliteView) {
    let by_sqlite = create_sqlite_database(dir, "sqlite.db", auto_vacuum, journal_mode);
    let by_turso = dir.join("turso.db");
    std::fs::copy(&by_sqlite, &by_turso).unwrap();
    run_in_sqlite(&by_sqlite, statements);
    run_in_turso(&by_turso, statements);
    (sqlite_view(&by_sqlite), sqlite_view(&by_turso))
}

fn used_pages(view: &SqliteView) -> i64 {
    view.page_count - view.freelist_count
}

/// Runs `statements` with Turso and with SQLite on copies of the same
/// SQLite-created auto-vacuum database, for both auto-vacuum modes and both
/// journal modes, and checks that SQLite sees the same file in both cases.
///
/// Turso's b-tree balancing does not always split and merge pages exactly
/// like SQLite's, so the same statements can need a different number of
/// pages even without auto-vacuum. That difference is measured on a database
/// without auto-vacuum and allowed; everything else must match.
fn assert_same_file_as_sqlite(statements: &[&str]) {
    for journal_mode in ["WAL", "DELETE"] {
        let dir = TempDir::new().unwrap();
        let (expected, actual) = run_both(dir.path(), "NONE", journal_mode, statements);
        let layout_difference = used_pages(&actual) - used_pages(&expected);
        for auto_vacuum in ["FULL", "INCREMENTAL"] {
            let dir = TempDir::new().unwrap();
            let (expected, actual) = run_both(dir.path(), auto_vacuum, journal_mode, statements);
            let context = format!(
                "auto_vacuum={auto_vacuum}, journal_mode={journal_mode}, statements={statements:?}"
            );
            assert_eq!(expected.integrity_check, "ok", "{context}");
            assert_eq!(
                used_pages(&actual) - used_pages(&expected),
                layout_difference,
                "{context}: {actual:?} vs {expected:?}"
            );
            if layout_difference == 0 {
                assert_eq!(actual, expected, "{context}");
            } else {
                assert_eq!(
                    SqliteView {
                        page_count: expected.page_count,
                        freelist_count: expected.freelist_count,
                        ..actual
                    },
                    expected,
                    "{context}"
                );
                if auto_vacuum == "FULL" {
                    assert_eq!(actual.freelist_count, 0, "{context}");
                }
            }
        }
    }
}

#[test]
fn test_autovacuum_btree_split_matches_sqlite() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 2000) SELECT 'row' || value FROM s",
    ]);
}

#[test]
fn test_autovacuum_overflow_row_matches_sqlite() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)",
        "INSERT INTO t(b) VALUES (zeroblob(5000))",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 20) SELECT zeroblob(3000) FROM s",
    ]);
}

#[test]
fn test_autovacuum_delete_truncates_like_sqlite() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 2000) SELECT 'row' || value FROM s",
        "DELETE FROM t",
    ]);
}

#[test]
fn test_autovacuum_partial_delete_moves_pages_like_sqlite() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX t_b ON t(b)",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 400) SELECT hex(zeroblob(150)) || value FROM s",
        "DELETE FROM t WHERE a BETWEEN 100 AND 300",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 10) SELECT hex(zeroblob(1000)) || value FROM s",
    ]);
}

#[test]
fn test_autovacuum_create_table_places_root_pages_like_sqlite() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t1(a)",
        "INSERT INTO t1 WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 200) SELECT zeroblob(500) FROM s",
        "DELETE FROM t1 WHERE rowid > 50",
        "CREATE TABLE t2(x)",
        "CREATE INDEX i2 ON t2(x)",
        "INSERT INTO t2 WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 300) SELECT value FROM s",
    ]);
}

#[test]
fn test_autovacuum_drop_table_moves_largest_root_like_sqlite() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t1(a)",
        "CREATE TABLE t2(a)",
        "CREATE INDEX i1 ON t1(a)",
        "CREATE TABLE t3(a, b)",
        "CREATE INDEX i3 ON t3(b)",
        "INSERT INTO t1 WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 500) SELECT value FROM s",
        "INSERT INTO t2 WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 500) SELECT value FROM s",
        "INSERT INTO t3 WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 500) SELECT value, value FROM s",
        "DROP TABLE t1",
        "INSERT INTO t3 VALUES (1, 2)",
        "DROP INDEX i3",
        "INSERT INTO t2 WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 100) SELECT value FROM s",
    ]);
}

/// The sqlite_schema row of a moved root page is rewritten in place, so the
/// rows of sqlite_schema stay in rowid order. These are the statements of a
/// randomized run that found rows out of order.
#[test]
fn test_autovacuum_drop_table_keeps_sqlite_schema_in_rowid_order() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t0(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX t0_b ON t0(b)",
        "CREATE TABLE t1(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX t1_b ON t1(b)",
        "CREATE TABLE t2(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX t2_b ON t2(b)",
        "CREATE TABLE t3(a INTEGER PRIMARY KEY, b TEXT)",
        "DROP TABLE t0",
        "CREATE TABLE t4(a INTEGER PRIMARY KEY, b TEXT)",
        "DROP TABLE t1",
        "CREATE TABLE t5(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX t5_b ON t5(b)",
        "CREATE TABLE t6(a INTEGER PRIMARY KEY, b TEXT)",
        "CREATE INDEX t6_b ON t6(b)",
        "CREATE TABLE t7(a INTEGER PRIMARY KEY, b TEXT)",
        "DROP TABLE t4",
        "INSERT INTO t7 VALUES (1, 'x')",
    ]);
}

#[test]
fn test_autovacuum_incremental_vacuum_matches_sqlite() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 300) SELECT zeroblob(800) FROM s",
        "DELETE FROM t WHERE a % 2 = 0",
        "PRAGMA incremental_vacuum(10)",
        "PRAGMA incremental_vacuum(0)",
    ]);
}

#[test]
fn test_autovacuum_statement_rollback_keeps_pointer_map() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE)",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 100) SELECT hex(zeroblob(350)) || value FROM s",
        "BEGIN",
        "DELETE FROM t WHERE a > 50",
        "SAVEPOINT s",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 30) SELECT hex(zeroblob(1500)) || value FROM s",
        "ROLLBACK TO s",
        "INSERT INTO t(b) VALUES (hex(zeroblob(2000)))",
        "COMMIT",
    ]);
}

fn turso_integrity_check(path: &Path) -> String {
    let opts = DatabaseOpts::new().with_autovacuum(true);
    let db = TempDatabase::new_with_existent_with_opts(path, opts);
    let conn = db.connect_limbo();
    let rows: Vec<(String,)> = crate::common::ExecRows::exec_rows(&conn, "PRAGMA integrity_check");
    conn.close().unwrap();
    rows.into_iter()
        .map(|(text,)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn sqlite_integrity_check(path: &Path) -> String {
    sqlite_view(path).integrity_check
}

fn overwrite_bytes(path: &Path, offset: u64, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

/// SQLite's integrity_check verifies every pointer-map entry; Turso's must
/// find the same corruption instead of reporting "ok".
#[test]
fn test_integrity_check_verifies_pointer_map_entries() {
    let dir = TempDir::new().unwrap();
    let path = create_sqlite_database(dir.path(), "corrupt.db", "FULL", "DELETE");
    run_in_sqlite(
        &path,
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
          "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 500) SELECT 'row' || value FROM s"],
    );
    assert_eq!(sqlite_integrity_check(&path), "ok");
    assert_eq!(turso_integrity_check(&path), "ok");

    // Page 2 is the pointer-map page; the entry of page 4 is the second one.
    // Point it at page 7 instead of its real parent.
    let page_size = 1024;
    overwrite_bytes(&path, page_size + 5 + 1, &7u32.to_be_bytes());
    let sqlite_result = sqlite_integrity_check(&path);
    assert!(
        sqlite_result.contains("Bad ptr map entry key=4"),
        "{sqlite_result}"
    );
    let turso_result = turso_integrity_check(&path);
    assert!(
        turso_result.contains("Bad ptr map entry key=4"),
        "{turso_result}"
    );
}

#[test]
fn test_integrity_check_verifies_largest_root_page() {
    let dir = TempDir::new().unwrap();
    let path = create_sqlite_database(dir.path(), "corrupt.db", "FULL", "DELETE");
    run_in_sqlite(&path, &["CREATE TABLE t(a)", "CREATE TABLE u(a)"]);
    assert_eq!(turso_integrity_check(&path), "ok");

    // The largest root page lives at offset 52 of the database header.
    overwrite_bytes(&path, 52, &3u32.to_be_bytes());
    let sqlite_result = sqlite_integrity_check(&path);
    assert!(
        sqlite_result.contains("max rootpage (4) disagrees with header (3)"),
        "{sqlite_result}"
    );
    let turso_result = turso_integrity_check(&path);
    assert!(
        turso_result.contains("max rootpage (4) disagrees with header (3)"),
        "{turso_result}"
    );
}

/// The pager moves the largest root page when it creates and drops b-trees;
/// an MVCC checkpoint must not put back the value its logical header had.
#[test]
fn test_mvcc_checkpoint_keeps_the_largest_root_page() {
    let dir = TempDir::new().unwrap();
    let path = create_sqlite_database(dir.path(), "mvcc.db", "FULL", "WAL");
    let opts = DatabaseOpts::new().with_autovacuum(true);
    let db = TempDatabase::new_with_existent_with_opts(&path, opts);
    let conn = db.connect_limbo();
    conn.execute("PRAGMA journal_mode = 'mvcc'").unwrap();
    conn.execute("CREATE TABLE a(x)").unwrap();
    conn.execute("CREATE TABLE b(x)").unwrap();
    conn.execute("CREATE INDEX b_x ON b(x)").unwrap();
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let rows: Vec<(i64,)> =
        crate::common::ExecRows::exec_rows(&conn, "SELECT max(rootpage) FROM sqlite_schema");
    conn.close().unwrap();
    drop(db);

    let header = std::fs::read(&path).unwrap();
    let largest_root_page = u32::from_be_bytes(header[52..56].try_into().unwrap());
    assert_eq!(largest_root_page as i64, rows[0].0);
}
