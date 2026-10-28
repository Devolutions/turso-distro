using AwesomeAssertions;
using Turso.Data.Sqlite;
using Turso.Tests.Support;

namespace Turso.Tests;

/// <summary>
/// Closing the last handle to a database checkpoints its WAL, as SQLite does, so the .db file
/// alone holds every committed change.
/// </summary>
public class CheckpointOnCloseTests
{
    [Test]
    public void ClosingLastFacadeConnectionCheckpoints()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("plain.db");
        using (var connection = new SqliteConnection($"Data Source={path};Pooling=False"))
            Write(connection);

        AssertDatabaseFileAloneHoldsData(directory, path);
    }

    [Test]
    public void ReleasingPooledDatabaseCheckpoints()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("pooled.db");
        using (var connection = new SqliteConnection($"Data Source={path};Pooling=True"))
            Write(connection);

        SqliteConnection.ClearAllPools();
        AssertDatabaseFileAloneHoldsData(directory, path);
    }

    [Test]
    public void ClosingLastTursoConnectionCheckpoints()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("turso.db");
        using (var connection = new TursoConnection($"Data Source={path}"))
            Write(connection);

        AssertDatabaseFileAloneHoldsData(directory, path);
    }

    [Test]
    public void OpenConnectionKeepsWalUntilTheLastOneCloses()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("shared.db");
        using var first = new SqliteConnection($"Data Source={path};Pooling=False");
        first.Open();
        using (var second = new SqliteConnection($"Data Source={path};Pooling=False"))
            Write(second);

        new FileInfo(path + "-wal").Length.Should().BeGreaterThan(0);
        first.Close();
        AssertDatabaseFileAloneHoldsData(directory, path);
    }

    private static void Write(System.Data.Common.DbConnection connection)
    {
        connection.Open();
        foreach (var sql in new[] { "CREATE TABLE t(x TEXT)", "INSERT INTO t VALUES ('kept')" })
        {
            using var command = connection.CreateCommand();
            command.CommandText = sql;
            command.ExecuteNonQuery();
        }
    }

    private static void AssertDatabaseFileAloneHoldsData(TemporaryDirectory directory, string path)
    {
        var wal = path + "-wal";
        (File.Exists(wal) ? new FileInfo(wal).Length : 0).Should().Be(0);

        // Copy only the .db, as a backup or file sync would.
        var copy = directory.File("copy-" + Path.GetFileName(path));
        File.Copy(path, copy);
        using var connection = new SqliteConnection($"Data Source={copy};Pooling=False");
        connection.Open();
        using var command = connection.CreateCommand();
        command.CommandText = "SELECT x FROM t";
        command.ExecuteScalar().Should().Be("kept");
    }
}
