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

/// Steps `PRAGMA incremental_vacuum` with Turso, reading the table on the
/// same connection after every row. Returns the number of rows.
fn incremental_vacuum_in_turso(path: &Path, pragma: &str, check: &str) -> usize {
    let opts = DatabaseOpts::new().with_autovacuum(true);
    let db = TempDatabase::new_with_existent_with_opts(path, opts);
    let conn = db.connect_limbo();
    let expected = crate::common::limbo_exec_rows(&conn, check);
    let mut stmt = conn.prepare(pragma).unwrap();
    let mut rows = 0;
    loop {
        match stmt.step().unwrap() {
            turso_core::StepResult::Row => {
                assert_eq!(stmt.row().unwrap().get_values().count(), 0);
                rows += 1;
                assert_eq!(crate::common::limbo_exec_rows(&conn, check), expected);
            }
            turso_core::StepResult::Done => break,
            turso_core::StepResult::IO => stmt.get_pager().io.step().unwrap(),
            other => panic!("{pragma}: unexpected {other:?}"),
        }
    }
    drop(stmt);
    conn.close().unwrap();
    rows
}

fn incremental_vacuum_in_sqlite(path: &Path, pragma: &str) -> usize {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn.prepare(pragma).unwrap();
    let mut result = stmt.raw_query();
    let mut rows = 0;
    while result.next().unwrap().is_some() {
        rows += 1;
    }
    rows
}

/// SQLite returns one row without columns for each page that
/// `PRAGMA incremental_vacuum` moves or frees (incrvacuum.test 7.* counts
/// them), and the connection can read between those rows.
#[test]
fn test_autovacuum_incremental_vacuum_returns_a_row_per_page() {
    for journal_mode in ["WAL", "DELETE"] {
        for pragma in [
            "PRAGMA incremental_vacuum(5)",
            "PRAGMA incremental_vacuum",
            "PRAGMA incremental_vacuum(-1)",
        ] {
            let dir = TempDir::new().unwrap();
            let by_sqlite =
                create_sqlite_database(dir.path(), "sqlite.db", "INCREMENTAL", journal_mode);
            run_in_sqlite(
                &by_sqlite,
                &[
                    "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)",
                    "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 100) SELECT zeroblob(800) FROM s",
                    "DELETE FROM t WHERE a % 3 <> 0",
                ],
            );
            let by_turso = dir.path().join("turso.db");
            std::fs::copy(&by_sqlite, &by_turso).unwrap();
            let expected_rows = incremental_vacuum_in_sqlite(&by_sqlite, pragma);
            let rows = incremental_vacuum_in_turso(
                &by_turso,
                pragma,
                "SELECT count(*), sum(length(b)), group_concat(a) FROM t",
            );
            let context = format!("journal_mode={journal_mode}, {pragma}");
            assert!(expected_rows > 0, "{context}");
            assert_eq!(rows, expected_rows, "{context}");
            assert_eq!(sqlite_view(&by_turso), sqlite_view(&by_sqlite), "{context}");
        }
    }
}

#[test]
fn test_autovacuum_incremental_vacuum_then_growth_in_one_transaction() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 300) SELECT zeroblob(800) FROM s",
        "BEGIN",
        "DELETE FROM t WHERE a % 2 = 0",
        "PRAGMA incremental_vacuum",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 200) SELECT zeroblob(800) FROM s",
        "COMMIT",
    ]);
}

#[test]
fn test_autovacuum_rollback_to_savepoint_restores_pages_cut_by_incremental_vacuum() {
    assert_same_file_as_sqlite(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)",
        "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 300) SELECT zeroblob(800) FROM s",
        "BEGIN",
        "UPDATE t SET b = zeroblob(790)",
        "DELETE FROM t WHERE a % 2 = 0",
        "SAVEPOINT s",
        "PRAGMA incremental_vacuum",
        "ROLLBACK TO s",
        "INSERT INTO t(b) VALUES (zeroblob(800))",
        "COMMIT",
    ]);
}

/// Pages cut by `PRAGMA incremental_vacuum` and reused when the file grows
/// again must come back as they were on `ROLLBACK TO` (savepoint6.test).
#[test]
fn test_autovacuum_rollback_to_savepoint_restores_pages_cut_and_reused() {
    for nested in [false, true] {
        let mut statements = vec![
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE)",
            "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 300) SELECT hex(zeroblob(200)) || value FROM s",
            "DELETE FROM t WHERE a % 3 = 0",
            "SAVEPOINT a",
            "PRAGMA incremental_vacuum",
        ];
        if nested {
            statements.extend(["SAVEPOINT b", "RELEASE b"]);
        }
        statements.extend([
            "INSERT INTO t(b) WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < 150) SELECT hex(zeroblob(300)) || value FROM s",
            "ROLLBACK TO a",
            "INSERT INTO t(b) VALUES ('after')",
            "RELEASE a",
        ]);
        assert_same_file_as_sqlite(&statements);
    }
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
