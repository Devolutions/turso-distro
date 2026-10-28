using AwesomeAssertions;
using Turso.Data.Sqlite;

namespace Turso.Tests;

public class SqliteLocalTriggerTests
{
    private const string CreateTablesSql = """
        CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT);
        CREATE TABLE audit(id INTEGER PRIMARY KEY, "end" TEXT, kind TEXT);
        """;

    [Test]
    public void CreateTriggerAloneInCommand()
    {
        using var connection = OpenMemory();
        Execute(connection, CreateTablesSql);

        Execute(connection, """
            CREATE TRIGGER items_audit AFTER INSERT ON items BEGIN
                INSERT INTO audit("end", kind) VALUES (NEW.value, 'insert');
                UPDATE audit SET kind = kind || '!' WHERE "end" = NEW.value;
            END;
            """);
        Execute(connection, "INSERT INTO items(value) VALUES ('a')");

        Scalar(connection, "SELECT kind FROM audit WHERE \"end\" = 'a'").Should().Be("insert!");
    }

    [Test]
    public void PreparedCreateTriggerIsOneStatement()
    {
        using var connection = OpenMemory();
        Execute(connection, CreateTablesSql);

        using var command = connection.CreateCommand();
        command.CommandText = """
            CREATE TEMP TRIGGER items_audit AFTER INSERT ON items BEGIN
                INSERT INTO audit(kind) VALUES ('first');
                INSERT INTO audit(kind) VALUES ('second');
            END
            """;
        command.Prepare();
        command.ExecuteNonQuery();
        Execute(connection, "INSERT INTO items(value) VALUES ('a')");

        Scalar(connection, "SELECT group_concat(kind, ',') FROM audit").Should().Be("first,second");
    }

    [Test]
    public void BatchKeepsTriggerBodiesWhole()
    {
        using var connection = OpenMemory();
        using var command = connection.CreateCommand();
        command.CommandText = CreateTablesSql + """
            /* a block comment; with a semicolon */
            CREATE TABLE [odd;name](`semi;colon` TEXT);
            CREATE TRIGGER IF NOT EXISTS items_audit
            AFTER INSERT ON items
            WHEN NEW.value <> 'skip;me'
            BEGIN
                -- line comment; END;
                INSERT INTO audit("end", kind)
                VALUES (NEW.value, CASE WHEN NEW.value = ';' THEN 'semicolon' ELSE CASE NEW.value WHEN 'b' THEN 'bee' ELSE 'other' END END);
                /* END; */
                INSERT INTO [odd;name](`semi;colon`) VALUES ('END;');
            END;
            INSERT INTO items(value) VALUES (';'), ('b'), ('skip;me'), ('c');
            SELECT kind FROM audit ORDER BY id;
            """;

        using var reader = command.ExecuteReader();
        var kinds = new List<string>();
        while (reader.Read())
            kinds.Add(reader.GetString(0));

        kinds.Should().Equal("semicolon", "bee", "other");
        reader.Close();
        Scalar(connection, "SELECT count(*) FROM [odd;name] WHERE `semi;colon` = 'END;'").Should().Be(3L);
        Scalar(connection, "SELECT count(*) FROM sqlite_master WHERE type = 'trigger'").Should().Be(1L);
    }

    [Test]
    public void StatementsAfterTriggerStillSplit()
    {
        using var connection = OpenMemory();
        using var command = connection.CreateCommand();
        command.CommandText = CreateTablesSql + """
            CREATE TRIGGER t1 AFTER INSERT ON items BEGIN INSERT INTO audit(kind) VALUES ('t1'); END;
            CREATE TRIGGER t2 AFTER INSERT ON items BEGIN INSERT INTO audit(kind) VALUES ('t2'); END;
            INSERT INTO items(value) VALUES ('a');
            """;

        command.ExecuteNonQuery().Should().Be(1);
        Scalar(connection, "SELECT count(*) FROM audit").Should().Be(2L);
    }

    private static SqliteConnection OpenMemory()
    {
        var connection = new SqliteConnection("Data Source=:memory:");
        connection.Open();
        return connection;
    }

    private static void Execute(SqliteConnection connection, string sql)
    {
        using var command = connection.CreateCommand();
        command.CommandText = sql;
        command.ExecuteNonQuery();
    }

    private static object? Scalar(SqliteConnection connection, string sql)
    {
        using var command = connection.CreateCommand();
        command.CommandText = sql;
        return command.ExecuteScalar();
    }
}
