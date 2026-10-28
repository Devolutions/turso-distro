using AwesomeAssertions;
using Turso.Data.Sqlite;
using Turso.Tests.Support;

namespace Turso.Tests;

public class ExperimentalFeaturesTests
{
    [TestCase("Experimental Features")]
    [TestCase("ExperimentalFeatures")]
    [TestCase("experimentalfeatures")]
    public void BuildersAcceptKeywordAliases(string keyword)
    {
        new SqliteConnectionStringBuilder($"Data Source=x.db;{keyword}=autovacuum,vacuum")
            .ExperimentalFeatures.Should().Be("autovacuum,vacuum");
        new TursoConnectionStringBuilder($"Data Source=x.db;{keyword}=autovacuum,vacuum")
            .ExperimentalFeatures.Should().Be("autovacuum,vacuum");
    }

    [Test]
    public void FeatureListIsNormalized()
    {
        TursoConnectionOptions.NormalizeExperimentalFeatures(" Vacuum, autovacuum,,vacuum ")
            .Should().Be("autovacuum,vacuum");
        TursoConnectionOptions.NormalizeExperimentalFeatures(" , ").Should().BeNull();
        TursoConnectionOptions.NormalizeExperimentalFeatures(null).Should().BeNull();
    }

    [TestCase(true)]
    [TestCase(false)]
    public void AutovacuumEnablesPragmaAutoVacuum(bool pooling)
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("autovacuum.db");

        using (var connection = OpenFacade(path, pooling, "autovacuum"))
        {
            Execute(connection, "PRAGMA auto_vacuum = FULL");
            Execute(connection, "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)");
            Execute(connection, "INSERT INTO items(value) VALUES ('a')");
            Scalar(connection, "PRAGMA auto_vacuum").Should().Be(1L);
        }

        using (var reopened = OpenFacade(path, pooling, "autovacuum"))
        {
            Execute(reopened, "INSERT INTO items(value) VALUES ('b')");
            Scalar(reopened, "PRAGMA auto_vacuum").Should().Be(1L);
            Scalar(reopened, "SELECT count(*) FROM items").Should().Be(2L);
        }
    }

    [Test]
    public void PragmaAutoVacuumFailsWithoutAutovacuum()
    {
        using var directory = new TemporaryDirectory();
        using var connection = OpenFacade(directory.File("plain.db"), pooling: false, experimentalFeatures: null);

        connection.Invoking(x => Execute(x, "PRAGMA auto_vacuum = FULL"))
            .Should().Throw<SqliteException>()
            .WithMessage("*Autovacuum is not enabled*");
    }

    [Test]
    public void AutovacuumDatabaseOpensReadOnlyWithoutAutovacuum()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("autovacuum.db");
        using (var connection = OpenFacade(path, pooling: false, "autovacuum"))
        {
            Execute(connection, "PRAGMA auto_vacuum = FULL");
            Execute(connection, "CREATE TABLE items(value TEXT)");
        }

        using var reopened = OpenFacade(path, pooling: false, experimentalFeatures: null);
        reopened.Invoking(x => Execute(x, "INSERT INTO items VALUES ('a')"))
            .Should().Throw<SqliteException>()
            .WithMessage("*readonly*");
    }

    [TestCase(true)]
    [TestCase(false)]
    public void VacuumEnablesVacuum(bool pooling)
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("vacuum.db");

        using (var connection = OpenFacade(path, pooling, experimentalFeatures: null))
        {
            Execute(connection, "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)");
            Execute(connection, "INSERT INTO items(value) SELECT 'row-' || value FROM generate_series(1, 200)");
            Execute(connection, "DELETE FROM items WHERE id > 10");
            connection.Invoking(x => Execute(x, "VACUUM"))
                .Should().Throw<SqliteException>()
                .WithMessage("*VACUUM is an experimental feature*");
        }

        SqliteConnection.ClearAllPools();
        using var withVacuum = OpenFacade(path, pooling, "vacuum");
        Execute(withVacuum, "VACUUM");
        Scalar(withVacuum, "SELECT count(*) FROM items").Should().Be(10L);
        Scalar(withVacuum, "PRAGMA integrity_check").Should().Be("ok");
    }

    [Test]
    public void PoolRefusesToShareDatabaseAcrossFeatureSets()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("pooled.db");

        // Closing leaves the database open in the pool.
        using (var connection = OpenFacade(path, pooling: true, experimentalFeatures: null))
            Execute(connection, "CREATE TABLE items(value TEXT)");

        FluentActions.Invoking(() => OpenFacade(path, pooling: true, "vacuum").Dispose())
            .Should().Throw<InvalidOperationException>()
            .WithMessage("*Experimental Features*ClearPool*");

        SqliteConnection.ClearAllPools();
        using (var vacuum = OpenFacade(path, pooling: true, "vacuum"))
            Execute(vacuum, "VACUUM");

        // Spelling and order do not make another feature set.
        using var same = OpenFacade(path, pooling: true, " VACUUM ");
        Execute(same, "VACUUM");
    }

    [Test]
    public void TursoConnectionPassesFeaturesAndKeepsEncryption()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("encrypted.db");
        const string hexkey = "b1bbfda4f589dc9daaf004fe21111e00dc00c98237102f5c7002a5669fc76327";

        using var connection = new TursoConnection(
            $"Data Source={path};Encryption Cipher=aegis256;Encryption Key={hexkey};Experimental Features=autovacuum");
        connection.Open();

        using (var pragma = new TursoCommand(connection, "PRAGMA auto_vacuum = FULL"))
            pragma.ExecuteNonQuery();
        using (var create = new TursoCommand(connection, "CREATE TABLE t(x TEXT)"))
            create.ExecuteNonQuery();
        using (var insert = new TursoCommand(connection, "INSERT INTO t VALUES ('secret')"))
            insert.ExecuteNonQuery();
        using var select = new TursoCommand(connection, "PRAGMA auto_vacuum");
        select.ExecuteScalar().Should().Be(1L);
    }

    [Test]
    public void ExperimentalFeaturesAreRejectedForRemoteConnections()
    {
        using var remote = new TursoConnection("Data Source=https://example.test;Experimental Features=vacuum");
        remote.Invoking(x => x.Open())
            .Should().Throw<InvalidOperationException>()
            .WithMessage("*Experimental Features*");

        using var replica = new TursoConnection(
            "Data Source=turso://example.test;Replica Path=replica.db;Experimental Features=vacuum");
        replica.Invoking(x => x.CreateReplicaOptions())
            .Should().Throw<InvalidOperationException>()
            .WithMessage("*Sync Experimental Features*");

        using var facade = new SqliteConnection("Data Source=https://example.test;Experimental Features=vacuum");
        facade.Invoking(x => x.Open())
            .Should().Throw<InvalidOperationException>()
            .WithMessage("*Experimental Features*");
    }

    private static SqliteConnection OpenFacade(string path, bool pooling, string? experimentalFeatures)
    {
        var builder = new SqliteConnectionStringBuilder { DataSource = path, Pooling = pooling };
        if (experimentalFeatures is not null)
            builder.ExperimentalFeatures = experimentalFeatures;

        var connection = new SqliteConnection(builder.ConnectionString);
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
