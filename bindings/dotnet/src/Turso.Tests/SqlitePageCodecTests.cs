using AwesomeAssertions;
using Microsoft.EntityFrameworkCore;
using Turso.Data.Sqlite;
using Turso.Raw.Public;
using Turso.Tests.Support;
using static Turso.Tests.Support.PageCodecTestFiles;

namespace Turso.Tests;

public class SqlitePageCodecTests
{
    [TestCase(true)]
    [TestCase(false)]
    public void FacadeRoundTripsThroughCodecWithoutPlaintext(bool pooling)
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("codec.db");
        using var codec = new AesGcmTestPageCodec(AesGcmTestPageCodec.NewKey());

        using (var connection = OpenFacade(path, codec, pooling))
        {
            WriteRows(connection);

            ReadFile(path + "-wal").Length.Should().BeGreaterThan(0);
            AssertNoPlaintext(path + "-wal");

            connection.ExecuteNonQuery("PRAGMA wal_checkpoint(TRUNCATE)");
            AssertNoPlaintext(path);
        }

        using (var reopened = OpenFacade(path, codec, pooling))
        {
            CountRows(reopened).Should().Be(RowCount);
            Scalar(reopened, "PRAGMA integrity_check").Should().Be("ok");
        }

        SqliteConnection.ClearAllPools();
        AssertNoPlaintext(path);
        AssertNoPlaintext(path + "-wal");
    }

    [Test]
    public void PoolDoesNotShareDatabaseAcrossCodecIds()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("codec.db");
        using var codec = new AesGcmTestPageCodec(AesGcmTestPageCodec.NewKey());
        using var otherCodec = new AesGcmTestPageCodec(AesGcmTestPageCodec.NewKey());
        otherCodec.CodecId.Should().NotBe(codec.CodecId);

        // Closing leaves the database open in the pool.
        using (var connection = OpenFacade(path, codec, pooling: true))
            WriteRows(connection);

        // Sharing the pooled database would let the wrong key read the data.
        FluentActions.Invoking(() =>
            {
                using var connection = OpenFacade(path, otherCodec, pooling: true);
                CountRows(connection);
            })
            .Should().Throw<SqliteException>();

        SqliteConnection.ClearAllPools();
        FluentActions.Invoking(() =>
            {
                using var connection = OpenFacade(path, otherCodec, pooling: true);
                CountRows(connection);
            })
            .Should().Throw<SqliteException>();

        using var reopened = OpenFacade(path, codec, pooling: true);
        CountRows(reopened).Should().Be(RowCount);
    }

    [Test]
    public void ReadOnlyOpenUsesCodec()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("codec.db");
        using var codec = new AesGcmTestPageCodec(AesGcmTestPageCodec.NewKey());
        using (var connection = OpenFacade(path, codec, pooling: false))
            WriteRows(connection);

        using var readOnly = new SqliteConnection($"Data Source={path};Mode=ReadOnly;Pooling=False") { PageCodec = codec };
        readOnly.Open();

        CountRows(readOnly).Should().Be(RowCount);
        readOnly.Invoking(x => x.ExecuteNonQuery($"INSERT INTO items(name) VALUES ('{Marker}')"))
            .Should().Throw<SqliteException>();
    }

    [Test]
    public void RejectsPasswordAndRemotePageCodec()
    {
        using var directory = new TemporaryDirectory();
        using var codec = new AesGcmTestPageCodec(AesGcmTestPageCodec.NewKey());

        using (var password = new SqliteConnection($"Data Source={directory.File("password.db")};Password=secret") { PageCodec = codec })
            password.Invoking(x => x.Open()).Should().Throw<InvalidOperationException>();

        using (var remote = new SqliteConnection("Data Source=libsql://example.turso.io") { PageCodec = codec })
            remote.Invoking(x => x.Open()).Should().Throw<InvalidOperationException>().WithMessage("*PageCodec*");

        using var open = new SqliteConnection("Data Source=:memory:");
        open.Open();
        open.Invoking(x => x.PageCodec = codec).Should().Throw<InvalidOperationException>();
    }

    [Test]
    public async Task EfCoreUsesPreconfiguredCodecConnection()
    {
        using var directory = new TemporaryDirectory();
        var path = directory.File("ef.db");
        using var codec = new AesGcmTestPageCodec(AesGcmTestPageCodec.NewKey());

        await using (var connection = new SqliteConnection($"Data Source={path}") { PageCodec = codec })
        await using (var context = CreateContext(connection))
        {
            await context.Database.EnsureCreatedAsync();
            context.Notes.Add(new Note { Id = Guid.NewGuid(), Text = Marker });
            await context.SaveChangesAsync();
        }

        // EnsureCreated probes the file through a separate read-only connection, which must
        // carry the codec too.
        await using (var connection = new SqliteConnection($"Data Source={path}") { PageCodec = codec })
        await using (var context = CreateContext(connection))
        {
            (await context.Database.EnsureCreatedAsync()).Should().BeFalse();
            (await context.Notes.SingleAsync()).Text.Should().Be(Marker);
        }

        SqliteConnection.ClearAllPools();
        AssertNoPlaintext(path);
        AssertNoPlaintext(path + "-wal");
    }

    private static SqliteConnection OpenFacade(string path, ITursoPageCodec codec, bool pooling)
    {
        var connection = new SqliteConnection($"Data Source={path};Pooling={pooling}") { PageCodec = codec };
        connection.Open();
        return connection;
    }

    private static NotesContext CreateContext(SqliteConnection connection)
        => new(new DbContextOptionsBuilder<NotesContext>().UseTurso(connection).Options);

    private sealed class NotesContext(DbContextOptions<NotesContext> options) : DbContext(options)
    {
        public DbSet<Note> Notes => Set<Note>();
    }

    private sealed class Note
    {
        public Guid Id { get; set; }

        public string Text { get; set; } = "";
    }
}
