using System.Data;
using System.Data.Common;
using AwesomeAssertions;
using Turso.Data.Sqlite;
using MsSqlite = Microsoft.Data.Sqlite;

namespace Turso.Tests;

/// <summary>
/// GetFieldType reports a value's storage class, as Microsoft.Data.Sqlite does, and falls back
/// to the declared type only when no row has a value. GUID columns are the one difference:
/// the facade reports Guid where Microsoft.Data.Sqlite reports the stored class.
/// </summary>
public class FieldTypeTests
{
    private const string Setup = """
        CREATE TABLE H(IsDefault bit, B boolean, N numeric, I int, D datetime, Dec decimal(10,2), V varchar(5), X);
        INSERT INTO H VALUES (0, 1, 3.5, 2, '2024-01-02 03:04:05', 1.25, 'v', 7);
        INSERT INTO H VALUES (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
        """;

    [TestCase("SELECT * FROM H ORDER BY rowid")]
    [TestCase("SELECT * FROM H WHERE 0")]
    [TestCase("SELECT * FROM H ORDER BY rowid DESC")]
    public void FieldTypesMatchMicrosoftDataSqlite(string query)
    {
        using var turso = Open(new SqliteConnection("Data Source=:memory:"));
        using var sqlite = Open(new MsSqlite.SqliteConnection("Data Source=:memory:"));

        FieldTypesPerRow(turso, query).Should().Equal(FieldTypesPerRow(sqlite, query));
    }

    [Test]
    public void DataTableLoadKeepsIntegerColumnsNumeric()
    {
        using var connection = Open(new SqliteConnection("Data Source=:memory:"));
        using var command = connection.CreateCommand();
        command.CommandText = "SELECT IsDefault, B, N, D FROM H ORDER BY rowid";
        var table = new DataTable();
        using (var reader = command.ExecuteReader())
            table.Load(reader);

        table.Columns.Cast<DataColumn>().Select(c => c.DataType)
            .Should().Equal(typeof(long), typeof(long), typeof(double), typeof(string));
        Convert.ToBoolean(table.Rows[0]["IsDefault"]).Should().BeFalse();
        Convert.ToBoolean(table.Rows[0]["B"]).Should().BeTrue();
    }

    [Test]
    public void GuidColumnsStillReportGuid()
    {
        using var connection = Open(new SqliteConnection("Data Source=:memory:"));
        using var command = connection.CreateCommand();
        command.CommandText = """
            CREATE TABLE G(Id guid, U uniqueidentifier);
            INSERT INTO G VALUES (x'00112233445566778899aabbccddeeff', 'b6c7a3e1-4f1d-4b5e-9b3a-0d6c1f9e2a7b');
            SELECT * FROM G;
            """;
        using var reader = command.ExecuteReader();
        reader.GetFieldType(0).Should().Be(typeof(Guid));
        reader.Read().Should().BeTrue();
        reader.GetFieldType(0).Should().Be(typeof(Guid));
        reader.GetFieldType(1).Should().Be(typeof(Guid));
        reader.GetGuid(0).Should().Be(new Guid(Convert.FromHexString("00112233445566778899aabbccddeeff")));
    }

    [Test]
    public void GuidColumnHoldingOtherTextReadsItAsStored()
    {
        using var connection = Open(new SqliteConnection("Data Source=:memory:"));
        using var command = connection.CreateCommand();
        command.CommandText = """
            CREATE TABLE UserInfo (ID guid NOT NULL, Name varchar, PRIMARY KEY (ID));
            INSERT INTO UserInfo (ID, Name) VALUES ('user-1', 'operator');
            SELECT * FROM UserInfo;
            """;
        using (var reader = command.ExecuteReader())
        {
            reader.GetFieldType(0).Should().Be(typeof(string));
            reader.Read().Should().BeTrue();
            reader["ID"].Should().Be("user-1");
            reader.GetFieldType(0).Should().Be(typeof(string));
            reader.Invoking(r => r.GetGuid(0)).Should().Throw<FormatException>();
        }

        command.CommandText = "SELECT * FROM UserInfo";
        var table = new DataTable();
        using (var reader = command.ExecuteReader())
            table.Load(reader);
        table.Rows[0]["ID"].Should().Be("user-1");
    }

    [Test]
    public void GuidColumnMixesGuidAndOtherValues()
    {
        var guid = new Guid("b6c7a3e1-4f1d-4b5e-9b3a-0d6c1f9e2a7b");
        using var connection = Open(new SqliteConnection("Data Source=:memory:"));
        using var command = connection.CreateCommand();
        command.CommandText = $"""
            CREATE TABLE M(ID guid);
            INSERT INTO M VALUES (x'{Convert.ToHexString(guid.ToByteArray())}'), ('{guid}'), ('user-1'), (x'0102');
            SELECT ID FROM M ORDER BY rowid;
            """;
        using var reader = command.ExecuteReader();
        var values = new List<object>();
        while (reader.Read())
            values.Add(reader.GetValue(0));

        values.Take(3).Should().Equal(guid, guid, "user-1");
        values[3].Should().BeOfType<byte[]>().Which.Should().Equal(1, 2);
    }

    [Test]
    public void ReadingAheadForFieldTypesKeepsTheFirstRow()
    {
        using var connection = Open(new SqliteConnection("Data Source=:memory:"));
        using var command = connection.CreateCommand();
        command.CommandText = "SELECT I FROM H ORDER BY rowid";
        using var reader = command.ExecuteReader();

        reader.GetFieldType(0).Should().Be(typeof(long));
        reader.Read().Should().BeTrue();
        reader.GetInt64(0).Should().Be(2);
        reader.Read().Should().BeTrue();
        reader.IsDBNull(0).Should().BeTrue();
        reader.Read().Should().BeFalse();
    }

    private static List<string> FieldTypesPerRow(DbConnection connection, string query)
    {
        using var command = connection.CreateCommand();
        command.CommandText = query;
        using var reader = command.ExecuteReader();
        var rows = new List<string> { Describe(reader) };
        while (reader.Read())
            rows.Add(Describe(reader));
        return rows;
    }

    private static string Describe(DbDataReader reader)
        => string.Join(", ", Enumerable.Range(0, reader.FieldCount).Select(i => $"{reader.GetName(i)}={reader.GetFieldType(i).Name}"));

    private static T Open<T>(T connection)
        where T : DbConnection
    {
        connection.Open();
        using var command = connection.CreateCommand();
        command.CommandText = Setup;
        command.ExecuteNonQuery();
        return connection;
    }
}
