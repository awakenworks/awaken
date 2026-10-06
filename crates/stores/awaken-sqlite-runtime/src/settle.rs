// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: when an embedded SQLite file is at rest after this process dropped a
// @formal reason: store (ADR-0087). Dynamic path: sqlx closes a dropped pool's connection later, on its own
// @formal reason: thread, and that close checkpoints the WAL; settling waits for it so a file inspection
// @formal reason: never races it. Evidence: the pool's settle decision-table test (L1-L4); this is a
// @formal reason: product-requirement classification, not a direct formal-proof claim.
//! Which embedded database files have a dropped pool that is still closing.
//!
//! A pool counts twice per file: once while a handle to it is alive, and once while
//! SQLite still has its connection open. SQLite drops a collation's state only when
//! it closes the connection that registered it, and sqlx shares one collation state
//! between the pool's options and every connection, so a guard kept in that state
//! is released exactly when the pool is gone and its last connection is closed.
//! A file is settled when every connected pool still has a live handle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use crate::clock_hold::ClockHold;

static FILES: LazyLock<Files> = LazyLock::new(Files::default);

#[derive(Default)]
struct Files {
    counts: Mutex<HashMap<PathBuf, Counts>>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct Counts {
    live: usize,
    connected: usize,
}

#[derive(Clone, Copy)]
enum Count {
    Live,
    Connected,
}

impl Files {
    fn add(&self, file: &Path, count: Count) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = counts.entry(file.to_path_buf()).or_default();
        match count {
            Count::Live => entry.live += 1,
            Count::Connected => entry.connected += 1,
        }
    }

    fn remove(&self, file: &Path, count: Count) {
        {
            let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = counts.get_mut(file) {
                match count {
                    Count::Live => entry.live = entry.live.saturating_sub(1),
                    Count::Connected => entry.connected = entry.connected.saturating_sub(1),
                }
                if entry.live == 0 && entry.connected == 0 {
                    counts.remove(file);
                }
            }
        }
        self.changed.notify_waiters();
    }

    fn closing(&self, file: &Path) -> bool {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(file)
            .is_some_and(|entry| entry.connected > entry.live)
    }
}

/// Counted while a pool on the file has a live handle.
pub(crate) struct LiveFile(PathBuf);

impl LiveFile {
    pub(crate) fn new(file: PathBuf) -> Self {
        FILES.add(&file, Count::Live);
        Self(file)
    }
}

impl Drop for LiveFile {
    fn drop(&mut self) {
        FILES.remove(&self.0, Count::Live);
    }
}

/// Counted while SQLite has the pool's connection open; kept in a collation.
pub(crate) struct ConnectedFile(PathBuf);

impl ConnectedFile {
    pub(crate) fn new(file: PathBuf) -> Self {
        FILES.add(&file, Count::Connected);
        Self(file)
    }
}

impl Drop for ConnectedFile {
    fn drop(&mut self) {
        FILES.remove(&self.0, Count::Connected);
    }
}

pub(crate) const COLLATION: &str = "awaken_embedded_connected";

/// One spelling per file, so a relative and an absolute path to the same
/// database count as the same file.
pub(crate) fn file_key(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map_or_else(|_| absolute.clone(), |parent| parent.join(name)),
        _ => absolute,
    }
}

pub(crate) async fn settled(path: &Path) {
    let file = file_key(path);
    // The close runs on sqlx's thread, which a paused clock cannot see.
    let _closing = ClockHold::take();
    loop {
        let mut changed = std::pin::pin!(FILES.changed.notified());
        changed.as_mut().enable();
        if !FILES.closing(&file) {
            return;
        }
        changed.await;
    }
}
