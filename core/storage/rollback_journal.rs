//! Rollback-journal mode, for databases that SQLite and Turso share (see
//! [`crate::DatabaseOpts::with_rollback_journal`]).
//!
//! Turso keeps such a database in rollback-journal mode while it has it open
//! and takes the same file locks as SQLite's VFS (os_win.c, os_unix.c): a
//! SHARED lock while it reads, RESERVED while it writes, and EXCLUSIVE to
//! change the database file, so SQLite and Turso readers and writers exclude
//! each other the way two SQLite processes do.
//!
//! Turso still writes a transaction through its WAL, but only while it holds
//! the EXCLUSIVE lock, and it checkpoints and truncates the WAL before it
//! lets go of the lock. Between transactions the database file alone holds
//! the data, and the empty `-wal` file is invisible to SQLite, which treats
//! an empty file as missing. A crash in the middle of a commit leaves a WAL
//! that holds frames: SQLite recovers it in WAL mode, and Turso checkpoints
//! it before its next read.
//!
//! Each commit increments the file change counter, which SQLite compares to
//! decide whether its page cache is still valid, and Turso compares it the
//! same way before each read transaction. A reader also rolls back the hot
//! journal of a SQLite writer that died (pager.c `hasHotJournal`,
//! `pager_playback`).

use crate::io::{Completion, File, FileSyncType, OpenFlags, IO};
use crate::storage::wal::WalFileShared;
use crate::sync::{Arc, Mutex, RwLock};
use crate::{turso_assert, Buffer, LimboError, Result};

/// Byte offsets of SQLite's locks (os.h). The page that holds them is never
/// used for data.
pub(crate) const PENDING_BYTE: u64 = 0x4000_0000;
const RESERVED_BYTE: u64 = PENDING_BYTE + 1;
const SHARED_FIRST: u64 = PENDING_BYTE + 2;
const SHARED_SIZE: u64 = 510;

/// Magic at the start of each rollback-journal header (pager.c).
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// Offset of the 16 bytes SQLite compares to see whether the database
/// changed (change counter, page count, freelist trunk and count).
const FILE_VERSION_OFFSET: usize = 24;
pub(crate) type FileVersion = [u8; 16];

/// The lock a pager holds on the database file, in SQLite's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum LockLevel {
    None = 0,
    Shared = 1,
    Reserved = 2,
    Exclusive = 3,
}

impl LockLevel {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Shared,
            2 => Self::Reserved,
            3 => Self::Exclusive,
            _ => Self::None,
        }
    }
}

/// The locks this process holds. File locks belong to the file handle, which
/// every connection of the database shares, so they are counted here.
#[derive(Default)]
struct ProcessLocks {
    /// Pagers holding at least SHARED.
    shared: usize,
    /// The pager holding RESERVED, the one writer.
    reserved_owner: Option<usize>,
    /// The pager holding PENDING, on its way to EXCLUSIVE. New readers wait
    /// until it is done, as in SQLite.
    pending_owner: Option<usize>,
    exclusive: bool,
}

pub(crate) struct RollbackJournal {
    io: Arc<dyn IO>,
    file: Arc<dyn File>,
    journal_path: String,
    wal_path: String,
    wal_flags: OpenFlags,
    read_only: bool,
    locks: Mutex<ProcessLocks>,
    /// The WAL state of the database, reloaded after another process left
    /// frames in the WAL file.
    shared_wal: RwLock<Option<Arc<RwLock<WalFileShared>>>>,
}

impl RollbackJournal {
    pub(crate) fn new(
        io: Arc<dyn IO>,
        file: Arc<dyn File>,
        db_path: &str,
        wal_path: &str,
        wal_flags: OpenFlags,
    ) -> Self {
        Self {
            io,
            file,
            journal_path: format!("{db_path}-journal"),
            wal_path: wal_path.to_string(),
            wal_flags,
            read_only: wal_flags.contains(OpenFlags::ReadOnly),
            locks: Mutex::new(ProcessLocks::default()),
            shared_wal: RwLock::new(None),
        }
    }

    /// Whether the WAL file holds anything. Turso empties it before it
    /// releases EXCLUSIVE, so a reader finding frames there finds the
    /// leftover of a crash.
    pub(crate) fn wal_has_frames(&self) -> bool {
        std::fs::metadata(&self.wal_path).is_ok_and(|metadata| metadata.len() > 0)
    }

    pub(crate) fn set_shared_wal(&self, shared_wal: Arc<RwLock<WalFileShared>>) {
        *self.shared_wal.write() = Some(shared_wal);
    }

    /// Takes SHARED for one pager (os_win.c `winLock`, os_unix.c `unixLock`).
    pub(crate) fn lock_shared(&self) -> Result<()> {
        let mut locks = self.locks.lock();
        if locks.pending_owner.is_some() || locks.exclusive {
            return Err(LimboError::Busy);
        }
        if locks.shared == 0 {
            // PENDING is held while taking SHARED so that a writer waiting
            // for EXCLUSIVE is not starved. Windows takes it exclusively,
            // Unix with a read lock.
            if !self.file.try_lock_range(PENDING_BYTE, 1, cfg!(windows))? {
                return Err(LimboError::Busy);
            }
            let shared = self.file.try_lock_range(SHARED_FIRST, SHARED_SIZE, false);
            self.file.unlock_range(PENDING_BYTE, 1)?;
            if !shared? {
                return Err(LimboError::Busy);
            }
        }
        locks.shared += 1;
        Ok(())
    }

    pub(crate) fn unlock_shared(&self) {
        let mut locks = self.locks.lock();
        turso_assert!(locks.shared > 0, "SHARED released more often than taken");
        locks.shared -= 1;
        if locks.shared == 0 {
            if let Err(err) = self.file.unlock_range(SHARED_FIRST, SHARED_SIZE) {
                tracing::error!("releasing the SHARED lock failed: {err}");
            }
        }
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub(crate) fn lock_reserved(&self, owner: usize) -> Result<()> {
        let mut locks = self.locks.lock();
        if locks.reserved_owner.is_some() || !self.file.try_lock_range(RESERVED_BYTE, 1, true)? {
            return Err(LimboError::Busy);
        }
        locks.reserved_owner = Some(owner);
        Ok(())
    }

    pub(crate) fn unlock_reserved(&self, owner: usize) {
        let mut locks = self.locks.lock();
        if locks.reserved_owner == Some(owner) {
            if let Err(err) = self.file.unlock_range(RESERVED_BYTE, 1) {
                tracing::error!("releasing the RESERVED lock failed: {err}");
            }
            locks.reserved_owner = None;
        }
    }

    /// Takes EXCLUSIVE for the pager `owner`, which holds SHARED. Fails with
    /// `Busy` while other readers, in this process or another one, are
    /// active; PENDING stays held so that no new reader starts meanwhile, and
    /// the caller retries or gives up with [`Self::unlock_exclusive`].
    pub(crate) fn lock_exclusive(&self, owner: usize) -> Result<()> {
        let mut locks = self.locks.lock();
        match locks.pending_owner {
            Some(holder) if holder != owner => return Err(LimboError::Busy),
            Some(_) if locks.exclusive => return Ok(()),
            Some(_) => {}
            None => {
                if !self.file.try_lock_range(PENDING_BYTE, 1, true)? {
                    return Err(LimboError::Busy);
                }
                locks.pending_owner = Some(owner);
            }
        }
        if locks.shared > 1 {
            return Err(LimboError::Busy);
        }
        self.file.unlock_range(SHARED_FIRST, SHARED_SIZE)?;
        let exclusive = self.file.try_lock_range(SHARED_FIRST, SHARED_SIZE, true);
        if !matches!(exclusive, Ok(true)) {
            if !self.file.try_lock_range(SHARED_FIRST, SHARED_SIZE, false)? {
                return Err(LimboError::LockingError(
                    "lost the SHARED lock while waiting for EXCLUSIVE".into(),
                ));
            }
            exclusive?;
            return Err(LimboError::Busy);
        }
        locks.exclusive = true;
        Ok(())
    }

    /// Goes back from EXCLUSIVE (or PENDING) to SHARED.
    pub(crate) fn unlock_exclusive(&self, owner: usize) {
        let mut locks = self.locks.lock();
        if locks.pending_owner != Some(owner) {
            return;
        }
        if locks.exclusive {
            let result = self
                .file
                .unlock_range(SHARED_FIRST, SHARED_SIZE)
                .and_then(|()| self.file.try_lock_range(SHARED_FIRST, SHARED_SIZE, false));
            if !matches!(result, Ok(true)) {
                tracing::error!("going back from EXCLUSIVE to SHARED failed: {result:?}");
            }
            locks.exclusive = false;
        }
        if let Err(err) = self.file.unlock_range(PENDING_BYTE, 1) {
            tracing::error!("releasing the PENDING lock failed: {err}");
        }
        locks.pending_owner = None;
    }

    /// The bytes SQLite compares to detect a change, read from the file.
    /// Fails if another process switched the database to WAL mode.
    pub(crate) fn read_file_version(&self) -> Result<FileVersion> {
        let header = self.read_at(0, 100)?;
        let mut version = FileVersion::default();
        if header.len() < 100 {
            return Ok(version);
        }
        if header[18] == 2 || header[19] == 2 {
            return Err(LimboError::LockingError(
                "another connection switched this rollback-journal database to WAL mode".into(),
            ));
        }
        version.copy_from_slice(&header[FILE_VERSION_OFFSET..FILE_VERSION_OFFSET + 16]);
        Ok(version)
    }

    /// Increments the change counter in the database file, for a writer
    /// holding EXCLUSIVE that changed the file without the usual commit.
    pub(crate) fn increment_change_counter(&self) -> Result<()> {
        let header = self.read_at(0, 100)?;
        if header.len() < 100 {
            return Ok(());
        }
        let counter = read_u32(&header, FILE_VERSION_OFFSET)
            .expect("header length checked")
            .wrapping_add(1)
            .to_be_bytes();
        for offset in [FILE_VERSION_OFFSET as u64, 92] {
            let c = self.file.pwrite(
                offset,
                Arc::new(Buffer::new(counter.to_vec())),
                Completion::new_write(|_| {}),
            )?;
            self.io.wait_for_completion(c)?;
        }
        let c = self
            .file
            .sync(Completion::new_sync(|_| {}), FileSyncType::Fsync)?;
        self.io.wait_for_completion(c)
    }

    /// Whether a SQLite writer died and left a journal that must be rolled
    /// back before the database can be read (pager.c `hasHotJournal`). The
    /// caller holds SHARED.
    pub(crate) fn has_hot_journal(&self) -> Result<bool> {
        match std::fs::metadata(&self.journal_path) {
            Ok(metadata) if metadata.len() > 0 => {}
            _ => return Ok(false),
        }
        // A writer holding RESERVED is still writing its journal.
        if self.locks.lock().reserved_owner.is_some()
            || !self.file.try_lock_range(RESERVED_BYTE, 1, false)?
        {
            return Ok(false);
        }
        self.file.unlock_range(RESERVED_BYTE, 1)?;
        if self.file.size()? == 0 {
            return Ok(false);
        }
        // A journal whose header was zeroed (journal_mode=PERSIST) is not hot.
        let first = match std::fs::read(&self.journal_path) {
            Ok(journal) => journal.first().copied(),
            Err(_) => None,
        };
        Ok(first.is_some_and(|byte| byte != 0))
    }

    /// Restores the pages saved in a hot journal, truncates the database to
    /// its size before the failed transaction and deletes the journal
    /// (pager.c `pager_playback`). The caller holds EXCLUSIVE.
    pub(crate) fn roll_back_hot_journal(&self) -> Result<()> {
        let journal = match std::fs::read(&self.journal_path) {
            Ok(journal) => journal,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(crate::error::io_error(err, "read journal")),
        };
        if let Some(playback) = parse_journal(&journal)? {
            if !playback.super_journal_committed() {
                let page_size = playback.page_size as u64;
                let original_size = u64::from(playback.original_pages) * page_size;
                if self.file.size()? > original_size {
                    let c = self
                        .file
                        .truncate(original_size, Completion::new_trunc(|_| {}))?;
                    self.io.wait_for_completion(c)?;
                }
                for (page_no, data) in playback.pages {
                    let offset = u64::from(page_no - 1) * page_size;
                    let c = self.file.pwrite(
                        offset,
                        Arc::new(Buffer::new(data.to_vec())),
                        Completion::new_write(|_| {}),
                    )?;
                    self.io.wait_for_completion(c)?;
                }
                let c = self
                    .file
                    .sync(Completion::new_sync(|_| {}), FileSyncType::Fsync)?;
                self.io.wait_for_completion(c)?;
            }
        }
        std::fs::remove_file(&self.journal_path)
            .map_err(|err| crate::error::io_error(err, "delete journal"))?;
        tracing::info!("rolled back the hot journal {}", self.journal_path);
        Ok(())
    }

    /// Rebuilds the WAL state from the WAL file, after another process (or
    /// this one, before a crash) left frames in it.
    pub(crate) fn reload_wal(&self) -> Result<()> {
        let Some(shared_wal) = self.shared_wal.read().clone() else {
            return Ok(());
        };
        let reloaded =
            WalFileShared::open_shared_if_exists(&self.io, &self.wal_path, self.wal_flags)?;
        let reloaded = Arc::try_unwrap(reloaded).map_err(|_| {
            LimboError::InternalError("reloaded WAL state unexpectedly shared".to_string())
        })?;
        shared_wal
            .write()
            .replace_after_external_restore(reloaded.into_inner());
        Ok(())
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let buffer = Arc::new(Buffer::new_temporary(len));
        let read = Arc::new(crate::sync::atomic::AtomicUsize::new(0));
        let completion = {
            let read = read.clone();
            Completion::new_read(buffer.clone(), move |result| {
                if let Ok((_, n)) = result {
                    read.store(n.max(0) as usize, crate::sync::atomic::Ordering::Release);
                }
                None
            })
        };
        let c = self.file.pread(offset, completion)?;
        self.io.wait_for_completion(c)?;
        let n = read.load(crate::sync::atomic::Ordering::Acquire).min(len);
        Ok(buffer.as_slice()[..n].to_vec())
    }
}

/// What a hot journal restores.
struct Playback<'a> {
    page_size: u32,
    /// Database size in pages before the failed transaction.
    original_pages: u32,
    /// Original page contents, in journal order.
    pages: Vec<(u32, &'a [u8])>,
    super_journal: Option<String>,
}

impl Playback<'_> {
    /// A journal that names a super-journal belongs to a multi-database
    /// transaction, which committed if its super-journal is gone.
    fn super_journal_committed(&self) -> bool {
        self.super_journal
            .as_ref()
            .is_some_and(|path| !std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0))
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes(slice.try_into().expect("four bytes")))
}

/// Parses a hot journal the way SQLite plays it back. `None` when it holds
/// nothing to restore.
fn parse_journal(journal: &[u8]) -> Result<Option<Playback<'_>>> {
    if journal.len() < 28 || journal[..8] != JOURNAL_MAGIC {
        return Ok(None);
    }
    let sector_size = read_u32(journal, 20).expect("length checked");
    let page_size = read_u32(journal, 24).expect("length checked");
    if !(512..=65536).contains(&page_size)
        || !page_size.is_power_of_two()
        || !(32..=65536).contains(&sector_size)
        || !sector_size.is_power_of_two()
    {
        return Err(LimboError::Corrupt(format!(
            "hot journal has an invalid page size {page_size} or sector size {sector_size}"
        )));
    }
    let header_size = sector_size as usize;
    let record_size = page_size as usize + 8;
    let super_journal_page = (PENDING_BYTE / u64::from(page_size)) as u32 + 1;
    let mut playback = Playback {
        page_size,
        original_pages: 0,
        pages: Vec::new(),
        super_journal: read_super_journal(journal),
    };

    let mut offset = 0usize;
    let mut first = true;
    'headers: loop {
        // Each header starts on a sector boundary.
        offset = offset.div_ceil(header_size) * header_size;
        if offset + header_size > journal.len() || journal[offset..offset + 8] != JOURNAL_MAGIC {
            break;
        }
        let mut records = read_u32(journal, offset + 8).expect("header in bounds");
        let checksum_init = read_u32(journal, offset + 12).expect("header in bounds");
        let db_pages = read_u32(journal, offset + 16).expect("header in bounds");
        offset += header_size;
        if records == u32::MAX {
            records = ((journal.len() - offset) / record_size) as u32;
        }
        if first {
            playback.original_pages = db_pages;
            first = false;
        }
        for _ in 0..records {
            let Some(record) = journal.get(offset..offset + record_size) else {
                break 'headers;
            };
            offset += record_size;
            let page_no = read_u32(record, 0).expect("record in bounds");
            if page_no == 0 || page_no == super_journal_page {
                break 'headers;
            }
            let data = &record[4..record_size - 4];
            if checksum(checksum_init, data) != read_u32(record, record_size - 4).expect("record") {
                break 'headers;
            }
            if page_no <= db_pages {
                playback.pages.push((page_no, data));
            }
        }
        if records == 0 {
            break;
        }
    }
    if first || (playback.pages.is_empty() && playback.original_pages == 0) {
        return Ok(None);
    }
    Ok(Some(playback))
}

/// pager.c `pager_cksum`.
fn checksum(init: u32, data: &[u8]) -> u32 {
    let mut sum = init;
    let mut i = data.len() as isize - 200;
    while i > 0 {
        sum = sum.wrapping_add(u32::from(data[i as usize]));
        i -= 200;
    }
    sum
}

/// The super-journal name recorded at the end of a journal (pager.c
/// `readSuperJournal`).
fn read_super_journal(journal: &[u8]) -> Option<String> {
    let len = journal.len();
    if len < 16 || journal[len - 8..] != JOURNAL_MAGIC {
        return None;
    }
    let name_len = read_u32(journal, len - 16)? as usize;
    let mut checksum = read_u32(journal, len - 12)?;
    if name_len == 0 || name_len > len - 16 {
        return None;
    }
    let name = &journal[len - 16 - name_len..len - 16];
    for byte in name {
        checksum = checksum.wrapping_sub(u32::from(*byte));
    }
    if checksum != 0 {
        return None;
    }
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    Some(String::from_utf8_lossy(&name[..end]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal_with(page_size: u32, original_pages: u32, pages: &[(u32, u8)]) -> Vec<u8> {
        let sector = 512usize;
        let nonce = 0x1234_5678u32;
        let mut journal = vec![0u8; sector];
        journal[..8].copy_from_slice(&JOURNAL_MAGIC);
        journal[8..12].copy_from_slice(&(pages.len() as u32).to_be_bytes());
        journal[12..16].copy_from_slice(&nonce.to_be_bytes());
        journal[16..20].copy_from_slice(&original_pages.to_be_bytes());
        journal[20..24].copy_from_slice(&(sector as u32).to_be_bytes());
        journal[24..28].copy_from_slice(&page_size.to_be_bytes());
        for (page_no, fill) in pages {
            let data = vec![*fill; page_size as usize];
            journal.extend_from_slice(&page_no.to_be_bytes());
            journal.extend_from_slice(&data);
            journal.extend_from_slice(&checksum(nonce, &data).to_be_bytes());
        }
        journal
    }

    #[test]
    fn parses_the_pages_of_a_journal() {
        let journal = journal_with(4096, 3, &[(1, 0xaa), (3, 0xbb), (5, 0xcc)]);
        let playback = parse_journal(&journal).unwrap().unwrap();
        assert_eq!(playback.page_size, 4096);
        assert_eq!(playback.original_pages, 3);
        // Page 5 is past the original size and is dropped by the truncation.
        let pages: Vec<_> = playback.pages.iter().map(|(n, d)| (*n, d[0])).collect();
        assert_eq!(pages, vec![(1, 0xaa), (3, 0xbb)]);
        assert!(!playback.super_journal_committed());
    }

    #[test]
    fn stops_at_a_torn_record() {
        let mut journal = journal_with(1024, 4, &[(1, 1), (2, 2), (3, 3)]);
        // Damage a byte of the second record that its checksum covers.
        let second = 512 + (1024 + 8) + 4 + 224;
        journal[second] ^= 0xff;
        let playback = parse_journal(&journal).unwrap().unwrap();
        assert_eq!(playback.pages.len(), 1);
    }

    #[test]
    fn ignores_a_journal_without_a_header() {
        assert!(parse_journal(&[0u8; 1024]).unwrap().is_none());
        let mut journal = journal_with(1024, 2, &[(1, 1)]);
        journal[0] = 0;
        assert!(parse_journal(&journal).unwrap().is_none());
    }
}
