using AwesomeAssertions;
using Turso.Data.Sqlite;
using Turso.Tests.Support;
using MsSqlite = Microsoft.Data.Sqlite;

namespace Turso.Tests;

/// <summary>
/// sqlite_schema keeps names as written, as SQLite does, while lookups stay case-insensitive.
/// Each test runs the same SQL through SQLite (Microsoft.Data.Sqlite) and compares.
/// </summary>
public class IdentifierCaseTests
{
    private const string Ddl = """
        CREATE TABLE Parent(Id INTEGER PRIMARY KEY, Code TEXT UNIQUE);
        CREATE TABLE T1(Name TEXT);
        ALTER TABLE T1 ADD COLUMN RepositoryID TEXT;
        CREATE INDEX IX_T1_Name ON t1(Name);
        CREATE UNIQUE INDEX UX_Parent ON PARENT(Code, Id);
        CREATE VIEW V_Parent AS SELECT Id, Code FROM Parent;
        CREATE TRIGGER TR_T1 AFTER INSERT ON t1 BEGIN SELECT 1; END;
        CREATE TABLE Auto(Id INTEGER PRIMARY KEY AUTOINCREMENT, X);
        INSERT INTO Auto(X) VALUES (1);
        CREATE TABLE OldName(A TEXT UNIQUE);
        ALTER TABLE oldname RENAME TO NewName;
        ALTER TABLE T1 RENAME COLUMN Name TO FullName;
        ALTER TABLE NewName ADD COLUMN B;
        """;

    private const string SchemaNamesSql = """
        SELECT type || '|' || name || '|' || tbl_name FROM sqlite_master
        WHERE name NOT LIKE '\_\_turso\_internal%' ESCAPE '\'
        ORDER BY type, name
        """;

    [Test]
    public void SchemaRowsMatchSqlite()
    {
        using var directory = new TemporaryDirectory();
        var expected = RunSqlite(directory.File("sqlite.db"), Ddl);

        using var connection = OpenTurso(directory.File("turso.db"));
        Execute(connection, Ddl);

        Strings(connection, SchemaNamesSql).Should().Equal(expected.SchemaNames);
        Strings(connection, "SELECT name FROM sqlite_sequence").Should().Equal(expected.SequenceNames);
        expected.SchemaNames.Should().Contain(["table|T1|T1", "index|sqlite_autoindex_NewName_1|NewName"]);
    }

    [Test]
    public void RewrittenSqlKeepsTableAndColumnCase()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("turso.db");
        using (var connection = OpenTurso(path))
            Execute(connection, Ddl);

        // Reopen so the names come back from the file, not from the in-memory schema.
        using var reopened = OpenTurso(path);
        var sql = (string)Scalar(reopened, "SELECT sql FROM sqlite_master WHERE name = 'T1'")!;
        sql.Should().StartWith("CREATE TABLE T1 ").And.Contain("FullName TEXT").And.Contain("RepositoryID TEXT");
        ((string)Scalar(reopened, "SELECT sql FROM sqlite_master WHERE name = 'NewName'")!)
            .Should().StartWith("CREATE TABLE NewName ");
        Strings(reopened, "SELECT name FROM pragma_table_info('t1')").Should().Equal("FullName", "RepositoryID");

        // The case-sensitive probes callers run against sqlite_master find the objects.
        Scalar(reopened, "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'T1'").Should().Be(1L);
        Scalar(reopened, "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'IX_T1_Name'").Should().Be(1L);

        // Lookups still ignore case.
        Execute(reopened, "INSERT INTO t1(fullname) VALUES ('a'); INSERT INTO T1(FULLNAME) VALUES ('b')");
        Scalar(reopened, "SELECT count(*) FROM t1").Should().Be(2L);
        Execute(reopened, "INSERT INTO AUTO(x) VALUES (2)");
        Strings(reopened, "SELECT name || '=' || seq FROM sqlite_sequence").Should().Equal("Auto=2");
    }

    [Test]
    public void FileWrittenBySqliteKeepsWorking()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("from-sqlite.db");
        RunSqlite(path, Ddl);

        const string Changes = """
            ALTER TABLE t1 ADD COLUMN Extra TEXT;
            ALTER TABLE parent RENAME TO Parents;
            DROP VIEW v_parent;
            DROP TRIGGER tr_t1;
            DROP INDEX ix_t1_name;
            INSERT INTO auto(x) VALUES (2);
            """;
        var expected = RunSqlite(directory.File("reference.db"), Ddl + Changes);

        using (var connection = OpenTurso(path))
        {
            Execute(connection, Changes);
            Strings(connection, SchemaNamesSql).Should().Equal(expected.SchemaNames);
            Strings(connection, "SELECT name || '=' || seq FROM sqlite_sequence").Should().Equal("Auto=2");
            Execute(connection, "PRAGMA wal_checkpoint(TRUNCATE)");
        }

        SqliteConnection.ClearAllPools();
        using var sqlite = new MsSqlite.SqliteConnection($"Data Source={path};Pooling=False");
        sqlite.Open();
        Scalar(sqlite, "PRAGMA integrity_check").Should().Be("ok");
        Strings(sqlite, SchemaNamesSql).Should().Equal(expected.SchemaNames);
        Strings(sqlite, "SELECT name FROM pragma_table_info('T1')").Should().Equal("FullName", "RepositoryID", "Extra");
    }

    private static (string[] SchemaNames, string[] SequenceNames) RunSqlite(string path, string sql)
    {
        using var connection = new MsSqlite.SqliteConnection($"Data Source={path};Pooling=False");
        connection.Open();
        Execute(connection, sql);
        return (Strings(connection, SchemaNamesSql), Strings(connection, "SELECT name FROM sqlite_sequence"));
    }

    private static SqliteConnection OpenTurso(string path)
    {
        var connection = new SqliteConnection($"Data Source={path};Pooling=False");
        connection.Open();
        return connection;
    }

    private static void Execute(System.Data.Common.DbConnection connection, string sql)
    {
        using var command = connection.CreateCommand();
        command.CommandText = sql;
        command.ExecuteNonQuery();
    }

    private static object? Scalar(System.Data.Common.DbConnection connection, string sql)
    {
        using var command = connection.CreateCommand();
        command.CommandText = sql;
        return command.ExecuteScalar();
    }

    private static string[] Strings(System.Data.Common.DbConnection connection, string sql)
    {
        using var command = connection.CreateCommand();
        command.CommandText = sql;
        using var reader = command.ExecuteReader();
        var values = new List<string>();
        while (reader.Read())
            values.Add(reader.GetString(0));
        return [.. values];
    }
}
