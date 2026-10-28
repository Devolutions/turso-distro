using Turso.Raw.Public;
using Turso.Raw.Public.Handles;

namespace Turso.Data.Sqlite;

/// <summary>
/// Keeps local database files open between connections when <c>Pooling</c> is enabled.
/// </summary>
/// <remarks>
/// Opening a native database reads the schema and replays the WAL, which dominates the cost of
/// <see cref="SqliteConnection.Open"/>. The pool keeps one opened database per file and gives each
/// <see cref="SqliteConnection"/> a fresh native connection on it, so per-connection state such as
/// PRAGMAs, functions and temp tables never leaks between opens. Native connections are never
/// reused, so this pools databases rather than connections. Each entry also holds one idle anchor
/// connection: closing the last connection to a database checkpoints the whole WAL, and the anchor
/// keeps that from happening on every <see cref="SqliteConnection.Close"/>.
/// Entries are keyed by file, page codec ID and experimental features, so a connection never joins a
/// database opened with another codec configuration (for example another key). The engine itself
/// shares one database per file, keeping the features of the first open, so a connection that asks
/// for other features than an entry already pooled for the file throws instead.
/// </remarks>
internal static class SqliteDatabasePool
{
    private static readonly StringComparer PathComparer =
        OperatingSystem.IsWindows() ? StringComparer.OrdinalIgnoreCase : StringComparer.Ordinal;
    private static readonly object Gate = new();
    private static readonly Dictionary<Key, Entry> Entries = new(new KeyComparer());
    private static bool _processExitHooked;

    public static TursoDatabaseHandle Connect(string filename, ITursoPageCodec? pageCodec, string? experimentalFeatures)
    {
        var key = new Key(Path.GetFullPath(filename), pageCodec?.CodecId, experimentalFeatures);
        while (true)
        {
            Entry? entry;
            lock (Gate)
            {
                if (!Entries.TryGetValue(key, out entry))
                {
                    ThrowIfPooledWithOtherFeatures(key);
                    entry = new Entry();
                    Entries.Add(key, entry);
                }

                if (!_processExitHooked)
                {
                    // Close idle databases on exit so their WAL is checkpointed, as it would be
                    // when the last connection closes without a pool.
                    AppDomain.CurrentDomain.ProcessExit += (_, _) => ClearAll();
                    _processExitHooked = true;
                }
            }

            lock (entry)
            {
                if (entry.Cleared)
                    continue;

                if (entry.Database is null)
                {
                    TursoSharedDatabaseHandle? database = null;
                    try
                    {
                        database = TursoBindings.OpenSharedDatabase(filename, pageCodec, experimentalFeatures);
                        entry.Anchor = TursoBindings.Connect(database);
                    }
                    catch
                    {
                        database?.Dispose();
                        RemoveFailedEntry(key, entry);
                        throw;
                    }

                    entry.Database = database;
                }

                return TursoBindings.Connect(entry.Database);
            }
        }
    }

    // Called with Gate held.
    private static void ThrowIfPooledWithOtherFeatures(Key key)
    {
        foreach (var pooled in Entries.Keys)
        {
            if (PathComparer.Equals(pooled.Path, key.Path)
                && !string.Equals(pooled.ExperimentalFeatures, key.ExperimentalFeatures, StringComparison.Ordinal))
            {
                throw new InvalidOperationException(
                    $"The pooled database '{key.Path}' was opened with Experimental Features="
                    + $"'{pooled.ExperimentalFeatures}', and the engine shares one database per file. "
                    + $"Open it with the same features, or call SqliteConnection.ClearPool first to use "
                    + $"'{key.ExperimentalFeatures}'.");
            }
        }
    }

    // Called with the entry lock held. Taking Gate here is safe because Gate is never held
    // while waiting for an entry lock. Marking the entry cleared sends connections waiting on
    // it back to the dictionary, so none of them opens a database on an entry that is no
    // longer reachable from ClearPool.
    private static void RemoveFailedEntry(Key key, Entry entry)
    {
        entry.Cleared = true;
        lock (Gate)
        {
            if (Entries.TryGetValue(key, out var current) && ReferenceEquals(current, entry))
                Entries.Remove(key);
        }
    }

    // Clears the entries for every codec configuration of the file.
    public static void Clear(string filename)
    {
        var path = Path.GetFullPath(filename);
        List<Entry> entries = [];
        lock (Gate)
        {
            foreach (var key in Entries.Keys.Where(key => PathComparer.Equals(key.Path, path)).ToList())
            {
                Entries.Remove(key, out var entry);
                entries.Add(entry!);
            }
        }

        foreach (var entry in entries)
            entry.Release();
    }

    public static void ClearAll()
    {
        List<Entry> entries;
        lock (Gate)
        {
            entries = [.. Entries.Values];
            Entries.Clear();
        }

        foreach (var entry in entries)
            entry.Release();
    }

    private readonly record struct Key(string Path, TursoPageCodecId? CodecId, string? ExperimentalFeatures);

    private sealed class KeyComparer : IEqualityComparer<Key>
    {
        public bool Equals(Key x, Key y)
            => PathComparer.Equals(x.Path, y.Path)
               && x.CodecId == y.CodecId
               && string.Equals(x.ExperimentalFeatures, y.ExperimentalFeatures, StringComparison.Ordinal);

        public int GetHashCode(Key key)
            => HashCode.Combine(
                PathComparer.GetHashCode(key.Path),
                key.CodecId,
                key.ExperimentalFeatures is null ? 0 : StringComparer.Ordinal.GetHashCode(key.ExperimentalFeatures));
    }

    private sealed class Entry
    {
        public TursoSharedDatabaseHandle? Database;
        public TursoDatabaseHandle? Anchor;
        public bool Cleared;

        // Connections still open on this database keep it alive until they close.
        public void Release()
        {
            lock (this)
            {
                Cleared = true;
                Anchor?.Dispose();
                Database?.Dispose();
                Anchor = null;
                Database = null;
            }
        }
    }
}
