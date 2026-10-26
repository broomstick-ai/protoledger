//! Points inside the store's compressed-ledger code at which a test can act:
//! corrupt a file between a write and its check, run a seal while a reader is
//! between two steps, or stop an operation as a crash would. Hooks are per
//! thread, so tests running in parallel do not see each other's.
//!
//! This module exists only with the non-default `test-hooks` feature, which
//! the crate's own tests turn on (a dev-dependency of the crate on itself).
//! Without it there is no module, and every hook point compiles to nothing:
//! not even a thread-local lookup.

use anyhow::Result;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

/// Where a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Point {
    /// A writer has opened its ledger's files and is about to lock them
    /// (SPEC §10.4). The path is the file it will lock.
    WriterLocking,
    /// A seal has written and fsynced its block, before reading it back
    /// (SPEC §10.4 step 2). The path is the block file.
    SealWritten,
    /// A seal has checked its block, before truncating the hot tail (step 3).
    /// The path is the hot tail.
    SealChecked,
    /// A migration has written its temporary file, before checking it
    /// (SPEC §10.10 step 2). The path is the temporary file.
    MigrationWritten,
    /// A migration has renamed its new form into place, before deleting the
    /// old one (between steps 3 and 4). The path is the new file.
    MigrationRenamed,
    /// A recompression has written its temporary file, before checking it
    /// (SPEC §10.9 step 2). The path is the temporary file.
    RecompressWritten,
    /// A recompression has checked its temporary file, before renaming it
    /// over the block file (between steps 2 and 3). The path is the
    /// temporary file.
    RecompressChecked,
    /// A recompression has renamed its new block file into place, before
    /// emptying the hot tail (between steps 3 and 4). The path is the block
    /// file.
    RecompressRenamed,
    /// A seal, migration or recompression that compresses with a dictionary
    /// is about to take the store's dictionary lock and make sure the
    /// dictionary is on disk (SPEC §10.7). The path is the dictionary file.
    DictLocking,
    /// `store config --dictionary` has checked that the dictionary is in the
    /// store, before it writes `store.json` (SPEC §10.10). The path is the
    /// dictionary file.
    DictDefaulting,
    /// A reader has taken `L` from the block file, before reading the hot
    /// tail (SPEC §10.5). The path is the block file.
    ReaderBeforeTail,
    /// A reader has read the hot tail, before re-opening the block file to
    /// check it is unchanged. The path is the hot tail.
    ReaderAfterTail,
}

type Hook = Box<dyn FnMut(&Path) -> Result<()>>;

thread_local! {
    static HOOKS: RefCell<HashMap<Point, Hook>> = RefCell::new(HashMap::new());
}

/// Run `f` at `point` on this thread, until [`clear`]. An error it returns
/// stops the operation there, as a crash would (nothing after the point runs).
pub fn set(point: Point, f: impl FnMut(&Path) -> Result<()> + 'static) {
    HOOKS.with(|h| h.borrow_mut().insert(point, Box::new(f)));
}

/// Remove every hook on this thread.
pub fn clear() {
    HOOKS.with(|h| h.borrow_mut().clear());
}

pub(crate) fn fire(point: Point, path: &Path) -> Result<()> {
    let Some(mut f) = HOOKS.with(|h| h.borrow_mut().remove(&point)) else {
        return Ok(());
    };
    let r = f(path);
    HOOKS.with(|h| {
        h.borrow_mut().entry(point).or_insert(f);
    });
    r
}
