//! Auto-vacuum: pointer-map upkeep, page relocation and file truncation.
//!
//! This is a port of the auto-vacuum parts of SQLite's btree.c
//! (`ptrmapPut`/`ptrmapGet`, `relocatePage`, `incrVacuumStep`,
//! `autoVacuumCommit`, `allocateBtreePage` in `BTALLOC_EXACT`/`BTALLOC_LE`
//! mode, and the root-page moves of `btreeCreateTable` and `btreeDropTable`).
//!
//! The type of a page's pointer-map entry is written where the page changes
//! role: `allocate_page`, `allocate_overflow_page`, `free_page` and
//! `btree_create`. The parent half of the entries for b-tree children and
//! first overflow pages is recomputed from the changed b-tree pages by
//! [`Pager::ptrmap_sync_dirty_pages`] before anything reads the pointer map,
//! so the b-tree balancing code does not have to know about pointer maps.
//! Both kinds of writes go to pointer-map pages, which the subjournal and the
//! WAL roll back like any other page.
//!
//! Each step that changes pages first reads and pins every page it needs,
//! and only then changes them, without yielding in between. A step that
//! yields while reading is simply run again.

use super::ptrmap::{PtrmapEntry, PtrmapGeometry, PtrmapType, PTRMAP_ENTRY_SIZE};
use super::{AutoVacuumMode, HeaderRef, HeaderRefMut, PageRef, Pager, RegisteredCursor};
use crate::storage::btree::offset::BTREE_RIGHTMOST_PTR;
use crate::storage::btree::{btree_init_page, PinGuard};
use crate::storage::page_cache::PageCacheKey;
use crate::storage::sqlite3_ondisk::{
    DatabaseHeader, PageType, FREELIST_LEAF_PTR_SIZE, FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR,
    FREELIST_TRUNK_OFFSET_LEAF_COUNT, FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR,
};
use crate::types::{IOCompletions, IOResult, IOResultOr};
use crate::{
    bail_corrupt_error, io_yield_one, return_if_io, turso_assert, turso_assert_eq, Result,
};

pub(super) fn auto_vacuum_mode_of(header: &DatabaseHeader) -> AutoVacuumMode {
    if header.vacuum_mode_largest_root_page.get() == 0 {
        AutoVacuumMode::None
    } else if header.incremental_vacuum_enabled.get() != 0 {
        AutoVacuumMode::Incremental
    } else {
        AutoVacuumMode::Full
    }
}

/// How [`Pager::freelist_take`] picks the page it removes from the freelist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreelistTake {
    /// The first page SQLite's `allocateBtreePage` would hand out.
    Any,
    /// This page, which must be on the freelist.
    Exact(u32),
    /// Any page whose number is at most this one.
    AtMost(u32),
}

/// Position of a freelist walk: the trunk page being searched and the trunk
/// page that links to it (0 when the database header does).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct FreelistSearch {
    prev_trunk: u32,
    trunk: u32,
}

#[derive(Debug, Clone, Copy)]
enum IncrVacuumStepState {
    Start,
    RemoveFromFreelist {
        search: FreelistSearch,
    },
    FindNewHome {
        entry: PtrmapEntry,
        search: FreelistSearch,
    },
    Relocate {
        entry: PtrmapEntry,
        new_page: u32,
    },
}

#[derive(Debug, Clone, Copy)]
enum CommitState {
    Start,
    SyncPtrmap,
    SaveCursors { final_size: u32 },
    MovePages { final_size: u32, last_page: u32 },
    Done,
}

#[derive(Debug, Clone, Copy)]
enum CreateRootState {
    Start,
    SyncPtrmap,
    TakeExactPage { root: u32, search: FreelistSearch },
    TakeAnyPage { root: u32 },
    SaveCursors { root: u32, moved_to: u32 },
    MovePage { root: u32, moved_to: u32 },
    InitRoot { root: u32 },
}

#[derive(Debug, Clone, Copy)]
enum DropRootState {
    Start,
    FreeRoot {
        largest_root: u32,
    },
    MoveLargestRoot {
        largest_root: u32,
    },
    FreeLargestRoot {
        largest_root: u32,
    },
    UpdateLargestRoot {
        largest_root: u32,
        moved: Option<u32>,
    },
}

pub(super) struct AutoVacuumState {
    sync: Option<(Vec<u32>, usize)>,
    cursor_save: Option<(Vec<RegisteredCursor>, usize)>,
    incr_step: IncrVacuumStepState,
    commit: CommitState,
    create_root: CreateRootState,
    drop_root: DropRootState,
}

impl AutoVacuumState {
    pub(super) fn new() -> Self {
        Self {
            sync: None,
            cursor_save: None,
            incr_step: IncrVacuumStepState::Start,
            commit: CommitState::Start,
            create_root: CreateRootState::Start,
            drop_root: DropRootState::Start,
        }
    }
}

/// A pointer-map entry that a page's content says must exist.
#[derive(Debug, Clone, Copy)]
struct PtrmapLink {
    page: u32,
    entry: PtrmapEntry,
}

impl Pager {
    pub(super) fn ptrmap_geometry(&self) -> PtrmapGeometry {
        let pending_byte_page = self.pending_byte_page_id().unwrap_or(0);
        PtrmapGeometry::new(self.usable_space() as u32, pending_byte_page)
    }

    /// Reads a page and waits for it to be loaded. Calling it again for the
    /// same page after a yield is safe.
    fn read_loaded_page(&self, page_no: u32) -> IOResultOr<PageRef> {
        let (page, completion) = return_if_io!(self.read_page(page_no as i64));
        if let Some(completion) = completion {
            if !completion.succeeded() {
                io_yield_one!(completion);
            }
        }
        Ok(IOResult::Done(page))
    }

    fn read_pinned_page(&self, page_no: u32) -> IOResultOr<PinGuard> {
        let page = return_if_io!(self.read_loaded_page(page_no));
        turso_assert!(page.is_loaded(), "page must be loaded", { "page_id": page_no });
        Ok(IOResult::Done(PinGuard::new(page)))
    }

    fn read_ptrmap_page_for(&self, page_no: u32) -> IOResultOr<PinGuard> {
        let geometry = self.ptrmap_geometry();
        geometry.check_has_entry(page_no)?;
        self.read_pinned_page(geometry.ptrmap_page_no(page_no))
    }

    /// Returns the pointer-map entry of `page_no`, or `None` if the entry
    /// holds no valid type.
    pub fn ptrmap_get(&self, page_no: u32) -> IOResultOr<Option<PtrmapEntry>> {
        let ptrmap_page = return_if_io!(self.read_ptrmap_page_for(page_no));
        Ok(IOResult::Done(self.ptrmap_entry_in(&ptrmap_page, page_no)))
    }

    /// Sets the pointer-map entry of `page_no`. The pointer-map page is only
    /// changed if the entry differs.
    pub fn ptrmap_put(
        &self,
        page_no: u32,
        entry_type: PtrmapType,
        parent_page_no: u32,
    ) -> IOResultOr<()> {
        let ptrmap_page = return_if_io!(self.read_ptrmap_page_for(page_no));
        self.ptrmap_set_in(
            &ptrmap_page,
            page_no,
            PtrmapEntry {
                entry_type,
                parent_page_no,
            },
        )?;
        Ok(IOResult::Done(()))
    }

    fn ptrmap_entry_in(&self, ptrmap_page: &PageRef, page_no: u32) -> Option<PtrmapEntry> {
        let offset = self.ptrmap_geometry().entry_offset(page_no);
        let buf = ptrmap_page.get_contents().as_ptr();
        PtrmapEntry::deserialize(&buf[offset..offset + PTRMAP_ENTRY_SIZE])
    }

    fn ptrmap_set_in(&self, ptrmap_page: &PageRef, page_no: u32, entry: PtrmapEntry) -> Result<()> {
        let geometry = self.ptrmap_geometry();
        turso_assert_eq!(
            ptrmap_page.get().id() as u32,
            geometry.ptrmap_page_no(page_no),
            "pointer map page has an unexpected number"
        );
        let offset = geometry.entry_offset(page_no);
        let mut serialized = [0u8; PTRMAP_ENTRY_SIZE];
        entry.serialize(&mut serialized)?;
        let buf = ptrmap_page.get_contents().as_ptr();
        if buf[offset..offset + PTRMAP_ENTRY_SIZE] != serialized {
            self.add_dirty(ptrmap_page)?;
            buf[offset..offset + PTRMAP_ENTRY_SIZE].copy_from_slice(&serialized);
        }
        Ok(())
    }

    /// Reads the pointer-map pages for `links` and writes the entries.
    fn write_ptrmap_links(&self, links: &[PtrmapLink]) -> IOResultOr<()> {
        let mut ptrmap_pages = Vec::with_capacity(links.len());
        for link in links {
            ptrmap_pages.push(return_if_io!(self.read_ptrmap_page_for(link.page)));
        }
        for (link, ptrmap_page) in links.iter().zip(&ptrmap_pages) {
            self.ptrmap_set_in(ptrmap_page, link.page, link.entry)?;
        }
        Ok(IOResult::Done(()))
    }

    /// Recomputes the pointer-map entries of the children and first overflow
    /// pages of every changed b-tree page.
    ///
    /// Pages change parents only when a page that points to them changes, so
    /// after this runs every entry in the pointer map is exact, as SQLite
    /// keeps it.
    pub(crate) fn ptrmap_sync_dirty_pages(&self) -> IOResultOr<()> {
        loop {
            let progress = self.vacuum_state.write().auto_vacuum.sync.take();
            let (pages, idx) = match progress {
                Some(progress) => progress,
                None => {
                    let database_size =
                        return_if_io!(self.with_header(|header| header.database_size.get()));
                    let geometry = self.ptrmap_geometry();
                    let pages: Vec<u32> = self
                        .dirty_pages
                        .read()
                        .iter()
                        .filter(|&page| page <= database_size && !geometry.is_unmapped_page(page))
                        .collect();
                    (pages, 0)
                }
            };
            if idx == pages.len() {
                return Ok(IOResult::Done(()));
            }
            let page_no = pages[idx];
            let result = self.ptrmap_sync_page(page_no);
            self.vacuum_state.write().auto_vacuum.sync = Some((pages, idx));
            return_if_io!(result);
            let mut state = self.vacuum_state.write();
            let (_, idx) = state
                .auto_vacuum
                .sync
                .as_mut()
                .expect("sync progress was just stored");
            *idx += 1;
        }
    }

    fn ptrmap_sync_page(&self, page_no: u32) -> IOResultOr<()> {
        if page_no != 1 {
            let Some(entry) = return_if_io!(self.ptrmap_get(page_no)) else {
                bail_corrupt_error!("page {page_no} was changed but has no pointer map entry");
            };
            if !matches!(
                entry.entry_type,
                PtrmapType::BTreeNode | PtrmapType::RootPage
            ) {
                return Ok(IOResult::Done(()));
            }
        }
        let page = return_if_io!(self.read_pinned_page(page_no));
        let links = self.child_ptrmap_links(&page, page_no)?;
        self.write_ptrmap_links(&links)
    }

    /// The pointer-map entries that the cells of b-tree page `page` require
    /// if the page is stored as page `page_no` (SQLite's `setChildPtrmaps`).
    fn child_ptrmap_links(&self, page: &PageRef, page_no: u32) -> Result<Vec<PtrmapLink>> {
        let contents = page.get_contents();
        let usable_space = self.usable_space();
        let is_leaf = contents.is_leaf();
        let cell_count = contents.cell_count();
        let mut links = Vec::with_capacity(cell_count + 1);
        let link = |child: u32, entry_type: PtrmapType| PtrmapLink {
            page: child,
            entry: PtrmapEntry {
                entry_type,
                parent_page_no: page_no,
            },
        };
        for idx in 0..cell_count {
            if !is_leaf {
                links.push(link(
                    contents.cell_interior_read_left_child_page(idx)?,
                    PtrmapType::BTreeNode,
                ));
            }
            let (_, _, first_overflow_page) =
                contents.cell_get_raw_region_and_overflow(idx, usable_space)?;
            if let Some(first_overflow_page) = first_overflow_page {
                links.push(link(first_overflow_page, PtrmapType::Overflow1));
            }
        }
        if let Some(rightmost) = contents.rightmost_pointer()? {
            links.push(link(rightmost, PtrmapType::BTreeNode));
        }
        let geometry = self.ptrmap_geometry();
        for link in &links {
            if link.page < 2 || geometry.is_unmapped_page(link.page) {
                bail_corrupt_error!(
                    "page {page_no} points to page {}, which cannot be a b-tree or overflow page",
                    link.page
                );
            }
        }
        Ok(links)
    }

    /// Saves the position of every open cursor and drops their page stacks,
    /// because pages are about to move (SQLite's `saveAllCursors`).
    fn save_all_cursor_positions(&self) -> IOResultOr<()> {
        let progress = self.vacuum_state.write().auto_vacuum.cursor_save.take();
        let (cursors, mut idx) = progress.unwrap_or_else(|| {
            let registry = self.cursor_registry.lock();
            let cursors = registry.values().flat_map(|b| b.iter().copied()).collect();
            (cursors, 0)
        });
        while idx < cursors.len() {
            // SAFETY: see RegisteredCursor's invariant.
            let cursor = unsafe { cursors[idx].as_mut() };
            match cursor.try_save_position_for_external_balance() {
                Ok(IOResult::Done(_)) => {}
                Ok(IOResult::IO(io)) => {
                    self.vacuum_state.write().auto_vacuum.cursor_save = Some((cursors, idx));
                    return Ok(IOResult::IO(io));
                }
                Err(err) => return Err(err),
            }
            cursor.invalidate_btree_cache();
            idx += 1;
        }
        Ok(IOResult::Done(()))
    }

    /// Removes a page from the freelist and returns its number (the freelist
    /// part of SQLite's `allocateBtreePage`). The page's content is left as
    /// it is.
    fn freelist_take(&self, search: &mut FreelistSearch, take: FreelistTake) -> IOResultOr<u32> {
        loop {
            let header_page = return_if_io!(self.read_pinned_page(DatabaseHeader::PAGE_ID as u32));
            let (freelist_pages, first_trunk) = {
                let header = HeaderRef(header_page.to_page());
                let header = header.borrow();
                (
                    header.freelist_pages.get(),
                    header.freelist_trunk_page.get(),
                )
            };
            if freelist_pages == 0 {
                bail_corrupt_error!("the freelist is empty but a page must be taken from it");
            }
            if search.trunk == 0 {
                *search = FreelistSearch {
                    prev_trunk: 0,
                    trunk: first_trunk,
                };
            }
            if search.trunk == 0 {
                bail_corrupt_error!("freelist has {freelist_pages} pages but no trunk page");
            }
            let trunk = return_if_io!(self.read_pinned_page(search.trunk));
            let prev_trunk = if search.prev_trunk == 0 {
                None
            } else {
                Some(return_if_io!(self.read_pinned_page(search.prev_trunk)))
            };
            let trunk_contents = trunk.get_contents();
            let next_trunk =
                trunk_contents.read_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR);
            let leaf_count =
                trunk_contents.read_u32_no_offset(FREELIST_TRUNK_OFFSET_LEAF_COUNT) as usize;
            if leaf_count > self.usable_space() / FREELIST_LEAF_PTR_SIZE - 2 {
                bail_corrupt_error!(
                    "freelist trunk page {} has {leaf_count} leaves",
                    search.trunk
                );
            }
            let leaf_at = |idx: usize| {
                trunk_contents.read_u32_no_offset(
                    FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR + idx * FREELIST_LEAF_PTR_SIZE,
                )
            };
            let take_trunk = match take {
                FreelistTake::Any => leaf_count == 0,
                FreelistTake::Exact(page) => search.trunk == page,
                FreelistTake::AtMost(page) => search.trunk <= page,
            };
            let taken = if take_trunk {
                let replacement = if leaf_count == 0 {
                    next_trunk
                } else {
                    let new_trunk_no = leaf_at(0);
                    let new_trunk = return_if_io!(self.read_pinned_page(new_trunk_no));
                    self.add_dirty(&new_trunk)?;
                    let new_contents = new_trunk.get_contents();
                    new_contents
                        .write_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR, next_trunk);
                    new_contents.write_u32_no_offset(
                        FREELIST_TRUNK_OFFSET_LEAF_COUNT,
                        leaf_count as u32 - 1,
                    );
                    let leaves_start = FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR;
                    let leaves_len = (leaf_count - 1) * FREELIST_LEAF_PTR_SIZE;
                    new_contents.as_ptr()[leaves_start..leaves_start + leaves_len].copy_from_slice(
                        &trunk_contents.as_ptr()[leaves_start + FREELIST_LEAF_PTR_SIZE
                            ..leaves_start + FREELIST_LEAF_PTR_SIZE + leaves_len],
                    );
                    new_trunk_no
                };
                match &prev_trunk {
                    Some(prev_trunk) => {
                        self.add_dirty(prev_trunk)?;
                        prev_trunk
                            .get_contents()
                            .write_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR, replacement);
                    }
                    None => {
                        self.add_dirty(&header_page)?;
                        HeaderRefMut(header_page.to_page())
                            .borrow_mut()
                            .freelist_trunk_page = replacement.into();
                    }
                }
                Some(search.trunk)
            } else if leaf_count > 0 {
                let leaf_idx = match take {
                    FreelistTake::Any => Some(0),
                    FreelistTake::Exact(page) => (0..leaf_count).find(|&idx| leaf_at(idx) == page),
                    FreelistTake::AtMost(page) => (0..leaf_count).find(|&idx| leaf_at(idx) <= page),
                };
                match leaf_idx {
                    Some(leaf_idx) => {
                        let leaf = leaf_at(leaf_idx);
                        self.add_dirty(&trunk)?;
                        let last_leaf = leaf_at(leaf_count - 1);
                        trunk_contents.write_u32_no_offset(
                            FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR
                                + leaf_idx * FREELIST_LEAF_PTR_SIZE,
                            last_leaf,
                        );
                        trunk_contents.write_u32_no_offset(
                            FREELIST_TRUNK_OFFSET_LEAF_COUNT,
                            leaf_count as u32 - 1,
                        );
                        Some(leaf)
                    }
                    None => None,
                }
            } else {
                None
            };
            match taken {
                Some(page) => {
                    self.add_dirty(&header_page)?;
                    let header = HeaderRefMut(header_page.to_page());
                    let header = header.borrow_mut();
                    header.freelist_pages = (header.freelist_pages.get() - 1).into();
                    *search = FreelistSearch::default();
                    return Ok(IOResult::Done(page));
                }
                None => {
                    if next_trunk == 0 {
                        bail_corrupt_error!("page not found on the freelist ({take:?})");
                    }
                    *search = FreelistSearch {
                        prev_trunk: search.trunk,
                        trunk: next_trunk,
                    };
                }
            }
        }
    }

    fn freelist_take_with_state(
        &self,
        search: FreelistSearch,
        take: FreelistTake,
        store: impl Fn(&mut AutoVacuumState, FreelistSearch),
    ) -> IOResultOr<u32> {
        let mut search = search;
        let result = self.freelist_take(&mut search, take);
        store(&mut self.vacuum_state.write().auto_vacuum, search);
        result
    }

    /// Moves page `from` to page `to`, which must be unused, and fixes the
    /// pointer that leads to it and the pointer-map entries of the pages it
    /// points to (SQLite's `relocatePage`).
    fn relocate_page(&self, from: u32, entry: PtrmapEntry, to: u32) -> IOResultOr<()> {
        if from < 3 {
            bail_corrupt_error!("page {from} cannot be moved");
        }
        let from_page = return_if_io!(self.read_pinned_page(from));
        let to_page = return_if_io!(self.read_pinned_page(to));
        let mut links = match entry.entry_type {
            PtrmapType::BTreeNode | PtrmapType::RootPage => {
                self.child_ptrmap_links(&from_page, to)?
            }
            PtrmapType::Overflow1 | PtrmapType::Overflow2 => {
                let next_overflow = from_page.get_contents().read_u32_no_offset(0);
                if next_overflow == 0 {
                    Vec::new()
                } else {
                    vec![PtrmapLink {
                        page: next_overflow,
                        entry: PtrmapEntry {
                            entry_type: PtrmapType::Overflow2,
                            parent_page_no: to,
                        },
                    }]
                }
            }
            PtrmapType::FreePage => {
                bail_corrupt_error!("free page {from} cannot be moved");
            }
        };
        let parent = if entry.entry_type == PtrmapType::RootPage {
            None
        } else {
            let parent_page = return_if_io!(self.read_pinned_page(entry.parent_page_no));
            let pointer_offset = pointer_offset_in(&parent_page, from, entry, self.usable_space())?;
            links.push(PtrmapLink { page: to, entry });
            Some((parent_page, pointer_offset))
        };
        let mut ptrmap_pages = Vec::with_capacity(links.len());
        for link in &links {
            ptrmap_pages.push(return_if_io!(self.read_ptrmap_page_for(link.page)));
        }

        tracing::debug!("relocate_page(from={from}, to={to}, entry={entry:?})");
        turso_assert!(
            from_page.get_contents().overflow_cells.is_empty(),
            "a page with overflow cells cannot be moved",
            { "page_id": from }
        );
        self.add_dirty(&to_page)?;
        to_page.get_contents().overflow_cells.clear();
        to_page
            .get_contents()
            .as_ptr()
            .copy_from_slice(from_page.get_contents().as_ptr());
        if let Some((parent_page, pointer_offset)) = &parent {
            self.add_dirty(parent_page)?;
            parent_page.get_contents().as_ptr()[*pointer_offset..*pointer_offset + 4]
                .copy_from_slice(&to.to_be_bytes());
        }
        for (link, ptrmap_page) in links.iter().zip(&ptrmap_pages) {
            self.ptrmap_set_in(ptrmap_page, link.page, link.entry)?;
        }
        Ok(IOResult::Done(()))
    }

    /// Moves the last page of the database file into a free page, or removes
    /// it from the freelist (SQLite's `incrVacuumStep`). Returns `false` when
    /// there was nothing to do because the freelist is empty.
    fn incr_vacuum_step(
        &self,
        final_size: u32,
        last_page: u32,
        is_commit: bool,
    ) -> IOResultOr<bool> {
        let geometry = self.ptrmap_geometry();
        loop {
            let state = self.vacuum_state.read().auto_vacuum.incr_step;
            match state {
                IncrVacuumStepState::Start => {
                    if !geometry.is_unmapped_page(last_page) {
                        let freelist_pages =
                            return_if_io!(self.with_header(|header| header.freelist_pages.get()));
                        if freelist_pages == 0 {
                            return Ok(IOResult::Done(false));
                        }
                        let Some(entry) = return_if_io!(self.ptrmap_get(last_page)) else {
                            bail_corrupt_error!("page {last_page} has no pointer map entry");
                        };
                        let next = match entry.entry_type {
                            PtrmapType::RootPage => {
                                bail_corrupt_error!(
                                    "root page {last_page} is past the end of the vacuumed file"
                                );
                            }
                            PtrmapType::FreePage if is_commit => None,
                            PtrmapType::FreePage => Some(IncrVacuumStepState::RemoveFromFreelist {
                                search: FreelistSearch::default(),
                            }),
                            _ => Some(IncrVacuumStepState::FindNewHome {
                                entry,
                                search: FreelistSearch::default(),
                            }),
                        };
                        if let Some(next) = next {
                            self.vacuum_state.write().auto_vacuum.incr_step = next;
                            continue;
                        }
                    }
                    return_if_io!(self.finish_incr_vacuum_step(last_page, is_commit));
                    return Ok(IOResult::Done(true));
                }
                IncrVacuumStepState::RemoveFromFreelist { search } => {
                    return_if_io!(self.freelist_take_with_state(
                        search,
                        FreelistTake::Exact(last_page),
                        |state, search| {
                            state.incr_step = IncrVacuumStepState::RemoveFromFreelist { search }
                        },
                    ));
                    self.vacuum_state.write().auto_vacuum.incr_step = IncrVacuumStepState::Start;
                    return_if_io!(self.finish_incr_vacuum_step(last_page, is_commit));
                    return Ok(IOResult::Done(true));
                }
                IncrVacuumStepState::FindNewHome { entry, search } => {
                    let take = if is_commit {
                        FreelistTake::Any
                    } else {
                        FreelistTake::AtMost(final_size)
                    };
                    let new_page = return_if_io!(self.freelist_take_with_state(
                        search,
                        take,
                        |state, search| {
                            state.incr_step = IncrVacuumStepState::FindNewHome { entry, search }
                        },
                    ));
                    let database_size =
                        return_if_io!(self.with_header(|header| header.database_size.get()));
                    if new_page > database_size {
                        bail_corrupt_error!(
                            "freelist page {new_page} is past the end of the database ({database_size} pages)"
                        );
                    }
                    self.vacuum_state.write().auto_vacuum.incr_step = if new_page > final_size {
                        turso_assert!(
                            is_commit,
                            "a page past the final size was taken for an incremental vacuum"
                        );
                        IncrVacuumStepState::FindNewHome {
                            entry,
                            search: FreelistSearch::default(),
                        }
                    } else {
                        IncrVacuumStepState::Relocate { entry, new_page }
                    };
                }
                IncrVacuumStepState::Relocate { entry, new_page } => {
                    return_if_io!(self.relocate_page(last_page, entry, new_page));
                    self.vacuum_state.write().auto_vacuum.incr_step = IncrVacuumStepState::Start;
                    return_if_io!(self.finish_incr_vacuum_step(last_page, is_commit));
                    return Ok(IOResult::Done(true));
                }
            }
        }
    }

    fn finish_incr_vacuum_step(&self, last_page: u32, is_commit: bool) -> IOResultOr<()> {
        if is_commit {
            return Ok(IOResult::Done(()));
        }
        let geometry = self.ptrmap_geometry();
        let mut new_size = last_page - 1;
        while geometry.is_unmapped_page(new_size) {
            new_size -= 1;
        }
        return_if_io!(self.with_header_mut(|header| header.database_size = new_size.into()));
        Ok(IOResult::Done(()))
    }

    /// Moves pages so that the freelist becomes empty and truncates the
    /// database to the pages in use, before a transaction of an
    /// `auto_vacuum=FULL` database commits (SQLite's `autoVacuumCommit`).
    /// For `auto_vacuum=INCREMENTAL` only the pointer map is brought up to
    /// date.
    pub(crate) fn auto_vacuum_commit(&self) -> IOResultOr<()> {
        let result = self.auto_vacuum_commit_steps();
        if result.is_err() {
            self.reset_auto_vacuum_state();
        }
        result
    }

    fn auto_vacuum_commit_steps(&self) -> IOResultOr<()> {
        loop {
            let state = self.vacuum_state.read().auto_vacuum.commit;
            match state {
                CommitState::Start => {
                    if self.dirty_pages.read().is_empty() {
                        self.vacuum_state.write().auto_vacuum.commit = CommitState::Done;
                        continue;
                    }
                    let mode = return_if_io!(self.with_header(auto_vacuum_mode_of));
                    self.vacuum_state.write().auto_vacuum.commit = match mode {
                        AutoVacuumMode::None => CommitState::Done,
                        AutoVacuumMode::Full | AutoVacuumMode::Incremental => {
                            CommitState::SyncPtrmap
                        }
                    };
                }
                CommitState::SyncPtrmap => {
                    return_if_io!(self.ptrmap_sync_dirty_pages());
                    let (mode, database_size, freelist_pages) =
                        return_if_io!(self.with_header(|header| (
                            auto_vacuum_mode_of(header),
                            header.database_size.get(),
                            header.freelist_pages.get()
                        )));
                    let next = if mode == AutoVacuumMode::Full && freelist_pages > 0 {
                        let geometry = self.ptrmap_geometry();
                        if geometry.is_unmapped_page(database_size) {
                            bail_corrupt_error!(
                                "the last page of the database ({database_size}) is a pointer map or pending-byte page"
                            );
                        }
                        let final_size = geometry.final_db_size(database_size, freelist_pages);
                        if final_size > database_size {
                            bail_corrupt_error!(
                                "freelist count {freelist_pages} does not fit a database of {database_size} pages"
                            );
                        }
                        CommitState::SaveCursors { final_size }
                    } else {
                        CommitState::Done
                    };
                    self.vacuum_state.write().auto_vacuum.commit = next;
                }
                CommitState::SaveCursors { final_size } => {
                    return_if_io!(self.save_all_cursor_positions());
                    let database_size =
                        return_if_io!(self.with_header(|header| header.database_size.get()));
                    self.vacuum_state.write().auto_vacuum.commit = CommitState::MovePages {
                        final_size,
                        last_page: database_size,
                    };
                }
                CommitState::MovePages {
                    final_size,
                    last_page,
                } => {
                    if last_page > final_size {
                        return_if_io!(self.incr_vacuum_step(final_size, last_page, true));
                        self.vacuum_state.write().auto_vacuum.commit = CommitState::MovePages {
                            final_size,
                            last_page: last_page - 1,
                        };
                        continue;
                    }
                    return_if_io!(self.with_header_mut(|header| {
                        header.freelist_trunk_page = 0.into();
                        header.freelist_pages = 0.into();
                        header.database_size = final_size.into();
                    }));
                    self.vacuum_state.write().auto_vacuum.commit = CommitState::Done;
                }
                CommitState::Done => {
                    let database_size =
                        return_if_io!(self.with_header(|header| header.database_size.get()));
                    self.discard_pages_after(database_size)?;
                    self.vacuum_state.write().auto_vacuum.commit = CommitState::Start;
                    return Ok(IOResult::Done(()));
                }
            }
        }
    }

    /// Allocates the root page of a new b-tree in an auto-vacuum database.
    /// Root pages sit right after the largest existing root page; the page
    /// that is there now moves elsewhere (SQLite's `btreeCreateTable`).
    pub(super) fn create_root_page(&self, page_type: PageType) -> IOResultOr<u32> {
        let result = self.create_root_page_steps(page_type);
        if result.is_err() {
            self.reset_auto_vacuum_state();
        }
        result
    }

    fn create_root_page_steps(&self, page_type: PageType) -> IOResultOr<u32> {
        let geometry = self.ptrmap_geometry();
        loop {
            let state = self.vacuum_state.read().auto_vacuum.create_root;
            match state {
                CreateRootState::Start => {
                    self.vacuum_state.write().auto_vacuum.create_root = CreateRootState::SyncPtrmap;
                }
                CreateRootState::SyncPtrmap => {
                    return_if_io!(self.ptrmap_sync_dirty_pages());
                    let (largest_root, database_size) = return_if_io!(self.with_header(|header| (
                        header.vacuum_mode_largest_root_page.get(),
                        header.database_size.get()
                    )));
                    if largest_root > database_size {
                        bail_corrupt_error!(
                            "largest root page {largest_root} is past the end of the database ({database_size} pages)"
                        );
                    }
                    let mut root = largest_root + 1;
                    while geometry.is_unmapped_page(root) {
                        root += 1;
                    }
                    let take_exact = root <= database_size
                        && matches!(
                            return_if_io!(self.ptrmap_get(root)),
                            Some(PtrmapEntry {
                                entry_type: PtrmapType::FreePage,
                                ..
                            })
                        );
                    self.vacuum_state.write().auto_vacuum.create_root = if take_exact {
                        CreateRootState::TakeExactPage {
                            root,
                            search: FreelistSearch::default(),
                        }
                    } else {
                        CreateRootState::TakeAnyPage { root }
                    };
                }
                CreateRootState::TakeExactPage { root, search } => {
                    return_if_io!(self.freelist_take_with_state(
                        search,
                        FreelistTake::Exact(root),
                        |state, search| {
                            state.create_root = CreateRootState::TakeExactPage { root, search }
                        },
                    ));
                    self.vacuum_state.write().auto_vacuum.create_root =
                        CreateRootState::InitRoot { root };
                }
                CreateRootState::TakeAnyPage { root } => {
                    let page = return_if_io!(self.allocate_page());
                    let moved_to = page.get().id() as u32;
                    self.vacuum_state.write().auto_vacuum.create_root = if moved_to == root {
                        CreateRootState::InitRoot { root }
                    } else {
                        CreateRootState::SaveCursors { root, moved_to }
                    };
                }
                CreateRootState::SaveCursors { root, moved_to } => {
                    return_if_io!(self.save_all_cursor_positions());
                    self.vacuum_state.write().auto_vacuum.create_root =
                        CreateRootState::MovePage { root, moved_to };
                }
                CreateRootState::MovePage { root, moved_to } => {
                    let Some(entry) = return_if_io!(self.ptrmap_get(root)) else {
                        bail_corrupt_error!("page {root} has no pointer map entry");
                    };
                    if matches!(
                        entry.entry_type,
                        PtrmapType::RootPage | PtrmapType::FreePage
                    ) {
                        bail_corrupt_error!(
                            "page {root} after the largest root page is a {:?} page",
                            entry.entry_type
                        );
                    }
                    return_if_io!(self.relocate_page(root, entry, moved_to));
                    self.vacuum_state.write().auto_vacuum.create_root =
                        CreateRootState::InitRoot { root };
                }
                CreateRootState::InitRoot { root } => {
                    let page = return_if_io!(self.read_pinned_page(root));
                    let ptrmap_page = return_if_io!(self.read_ptrmap_page_for(root));
                    let header = return_if_io!(HeaderRefMut::from_pager(self));
                    self.add_dirty(&page)?;
                    page.get_contents().as_ptr().fill(0);
                    btree_init_page(&page, page_type, 0, self.usable_space());
                    self.ptrmap_set_in(
                        &ptrmap_page,
                        root,
                        PtrmapEntry {
                            entry_type: PtrmapType::RootPage,
                            parent_page_no: 0,
                        },
                    )?;
                    header.borrow_mut().vacuum_mode_largest_root_page = root.into();
                    self.vacuum_state.write().auto_vacuum.create_root = CreateRootState::Start;
                    return Ok(IOResult::Done(root));
                }
            }
        }
    }

    /// Frees the root page `root` of a b-tree whose other pages were already
    /// freed. In an auto-vacuum database the root page with the largest
    /// number moves into the freed slot so that root pages stay at the start
    /// of the file; its old number is returned (SQLite's `btreeDropTable`).
    pub(crate) fn free_root_page(&self, root: u32) -> IOResultOr<Option<u32>> {
        let result = self.free_root_page_steps(root);
        if result.is_err() {
            self.reset_auto_vacuum_state();
        }
        result
    }

    fn free_root_page_steps(&self, root: u32) -> IOResultOr<Option<u32>> {
        let geometry = self.ptrmap_geometry();
        loop {
            let state = self.vacuum_state.read().auto_vacuum.drop_root;
            match state {
                DropRootState::Start => {
                    let (mode, largest_root) = return_if_io!(self.with_header(|header| (
                        auto_vacuum_mode_of(header),
                        header.vacuum_mode_largest_root_page.get()
                    )));
                    if mode == AutoVacuumMode::None {
                        return_if_io!(self.free_page(None, root as usize));
                        return Ok(IOResult::Done(None));
                    }
                    self.vacuum_state.write().auto_vacuum.drop_root = if root == largest_root {
                        DropRootState::FreeRoot { largest_root }
                    } else {
                        DropRootState::MoveLargestRoot { largest_root }
                    };
                }
                DropRootState::FreeRoot { largest_root } => {
                    return_if_io!(self.free_page(None, root as usize));
                    self.vacuum_state.write().auto_vacuum.drop_root =
                        DropRootState::UpdateLargestRoot {
                            largest_root,
                            moved: None,
                        };
                }
                DropRootState::MoveLargestRoot { largest_root } => {
                    let entry = return_if_io!(self.ptrmap_get(largest_root));
                    if !matches!(
                        entry,
                        Some(PtrmapEntry {
                            entry_type: PtrmapType::RootPage,
                            ..
                        })
                    ) {
                        bail_corrupt_error!(
                            "largest root page {largest_root} is not a root page ({entry:?})"
                        );
                    }
                    return_if_io!(self.relocate_page(
                        largest_root,
                        PtrmapEntry {
                            entry_type: PtrmapType::RootPage,
                            parent_page_no: 0,
                        },
                        root,
                    ));
                    self.invalidate_all_cursors();
                    self.vacuum_state.write().auto_vacuum.drop_root =
                        DropRootState::FreeLargestRoot { largest_root };
                }
                DropRootState::FreeLargestRoot { largest_root } => {
                    return_if_io!(self.free_page(None, largest_root as usize));
                    self.vacuum_state.write().auto_vacuum.drop_root =
                        DropRootState::UpdateLargestRoot {
                            largest_root,
                            moved: Some(largest_root),
                        };
                }
                DropRootState::UpdateLargestRoot {
                    largest_root,
                    moved,
                } => {
                    let mut new_largest_root = largest_root - 1;
                    while geometry.is_unmapped_page(new_largest_root) {
                        new_largest_root -= 1;
                    }
                    return_if_io!(self.with_header_mut(|header| {
                        header.vacuum_mode_largest_root_page = new_largest_root.into()
                    }));
                    self.vacuum_state.write().auto_vacuum.drop_root = DropRootState::Start;
                    return Ok(IOResult::Done(moved));
                }
            }
        }
    }

    /// Forgets changed pages past `database_size`: the file is truncated
    /// there, so they must not be written.
    pub(super) fn discard_pages_after(&self, database_size: u32) -> Result<()> {
        let mut dirty_pages = self.dirty_pages.write();
        let mut cache = self.page_cache.write();
        for page_id in dirty_pages.iter().filter(|&id| id > database_size) {
            if let Some(page) = cache.get(&PageCacheKey::new(page_id as usize))? {
                page.clear_dirty();
                page.try_unpin();
            }
        }
        dirty_pages.remove_range((database_size + 1)..);
        cache.truncate(database_size as usize)?;
        Ok(())
    }

    pub(super) fn reset_auto_vacuum_state(&self) {
        self.vacuum_state.write().auto_vacuum = AutoVacuumState::new();
    }
}

/// Byte offset, within `parent`, of the pointer to page `child` (the lookup
/// part of SQLite's `modifyPagePointer`).
fn pointer_offset_in(
    parent: &PageRef,
    child: u32,
    entry: PtrmapEntry,
    usable_space: usize,
) -> Result<usize> {
    let contents = parent.get_contents();
    let parent_no = parent.get().id();
    if entry.entry_type == PtrmapType::Overflow2 {
        if contents.read_u32_no_offset(0) != child {
            bail_corrupt_error!("overflow page {parent_no} does not point to page {child}");
        }
        return Ok(0);
    }
    let cell_count = contents.cell_count();
    let is_leaf = contents.is_leaf();
    for idx in 0..cell_count {
        if entry.entry_type == PtrmapType::Overflow1 {
            let (start, len, first_overflow_page) =
                contents.cell_get_raw_region_and_overflow(idx, usable_space)?;
            if first_overflow_page == Some(child) {
                return Ok(start + len - 4);
            }
        } else if !is_leaf && contents.cell_interior_read_left_child_page(idx)? == child {
            return Ok(contents.cell_get_raw_start_offset(idx));
        }
    }
    if entry.entry_type == PtrmapType::BTreeNode && contents.rightmost_pointer()? == Some(child) {
        return Ok(contents.offset() + BTREE_RIGHTMOST_PTR);
    }
    bail_corrupt_error!(
        "page {parent_no} has no {:?} pointer to page {child}",
        entry.entry_type
    )
}
