//! The fuser-backed write-through memory filesystem (ADR-0053, D2/D3).
//!
//! A faithful port of awaken-next's `memoryd` FUSE, over an in-process
//! [`MemoryRepository`] instead of an HTTP client, and reporting the store's real
//! create/update timestamps in `getattr` (awaken-next reported `now()`).

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use awaken_resource_contract::{MemErr, Memory, MemoryRepository};
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use libc::{EAGAIN, EEXIST, EINVAL, EIO, ENOENT, ENOSPC, ENOSYS, ENOTEMPTY, O_TRUNC};
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

use crate::invalidate::Invalidation;
use crate::{FuseError, immediate_children, splice_bytes};

/// Poll cadence for the peer-projection invalidation listener (ADR-0053 D5). An
/// invalidation only prompts a cache refetch, so sub-frame latency is unnecessary;
/// polling (rather than a blocking recv) lets the listener honour its stop flag
/// promptly at unmount without a second wakeup channel.
const INVALIDATION_POLL: Duration = Duration::from_millis(20);

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;
const CONTENT_CACHE_CAPACITY: usize = 256;
const CONTENT_CACHE_TTL: Duration = Duration::from_secs(1);
const MAX_DIRTY_OPEN_FILES: usize = 128;
const DEFAULT_UNMOUNT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn to_systime(nanos: u128) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// A live mount: gracefully unmount via [`unmount`](Self::unmount) (draining open
/// fds up to a bounded timeout); dropping the handle also unmounts.
pub struct MemoryMountHandle {
    // `Option` so `unmount` can take the session by value (`join`) without moving out
    // of a `Drop` type; a still-`Some` session at drop unmounts via its own `Drop`.
    session: Option<BackgroundSession>,
    state: Arc<MountState>,
    drain_timeout: Duration,
    listener: Option<InvalidationListener>,
}

/// A background thread draining peer-projection invalidations into a mount's cache
/// (ADR-0053 D5). Stops when its flag is set (at unmount) or the bus closes.
struct InvalidationListener {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl InvalidationListener {
    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

/// Drain `rx`, dropping any invalidated path for `store_id` from `cache`, until
/// `stop` is set or the bus closes. Non-matching stores and lag are ignored (a
/// lagged listener has missed invalidations, so it clears the whole cache to be
/// safe rather than serve a possibly-stale entry).
fn run_invalidation_listener(
    cache: Arc<Mutex<ContentLruCache>>,
    store_id: String,
    mut rx: broadcast::Receiver<Invalidation>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        match rx.try_recv() {
            Ok((store, path)) => {
                if store == store_id
                    && let Ok(mut cache) = cache.lock()
                {
                    if path == "/" {
                        cache.clear();
                    } else {
                        cache.remove_path(&path);
                    }
                }
            }
            Err(broadcast::error::TryRecvError::Empty) => std::thread::sleep(INVALIDATION_POLL),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                if let Ok(mut cache) = cache.lock() {
                    cache.clear();
                }
            }
            Err(broadcast::error::TryRecvError::Closed) => break,
        }
    }
}

#[derive(Default)]
struct MountState {
    open_fds: AtomicUsize,
}

impl MemoryMountHandle {
    /// Unmount and wait for the background session to exit. Drains open fds up to
    /// the drain timeout so a close-flush in flight is not cut off; warns (never
    /// hangs) if fds remain.
    pub fn unmount(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
        let deadline = Instant::now() + self.drain_timeout;
        while self.state.open_fds.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let remaining = self.state.open_fds.load(Ordering::SeqCst);
        if remaining != 0 {
            tracing::warn!(
                open_fds = remaining,
                drain_timeout_ms = self.drain_timeout.as_millis(),
                "memory FUSE unmount drain timeout elapsed with open file descriptors"
            );
        }
        if let Some(session) = self.session.take() {
            session.join();
        }
    }

    #[must_use]
    pub fn open_fd_count(&self) -> usize {
        self.state.open_fds.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn with_drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }
}

impl Drop for MemoryMountHandle {
    fn drop(&mut self) {
        // Dropping the handle (without an explicit unmount) still stops the listener
        // thread rather than leaving it polling a detached bus.
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
    }
}

/// Spawn a background mount of `store` at `mountpoint`, returning its handle.
pub fn spawn_mount(
    fs: Arc<dyn MemoryRepository>,
    store_id: String,
    mountpoint: PathBuf,
) -> Result<MemoryMountHandle, FuseError> {
    spawn_mount_inner(fs, store_id, mountpoint, None)
}

/// Like [`spawn_mount`], but the mount also drains `invalidations` and drops stale
/// paths from its cache so a write through a peer projection is reflected here.
/// Pass a [`LocalInvalidator`](crate::LocalInvalidator) subscription for same-process
/// coherence; a distributed adapter can feed the same broadcast receiver.
pub fn spawn_mount_with_invalidations(
    fs: Arc<dyn MemoryRepository>,
    store_id: String,
    mountpoint: PathBuf,
    invalidations: broadcast::Receiver<Invalidation>,
) -> Result<MemoryMountHandle, FuseError> {
    spawn_mount_inner(fs, store_id, mountpoint, Some(invalidations))
}

fn spawn_mount_inner(
    fs: Arc<dyn MemoryRepository>,
    store_id: String,
    mountpoint: PathBuf,
    invalidations: Option<broadcast::Receiver<Invalidation>>,
) -> Result<MemoryMountHandle, FuseError> {
    let state = Arc::new(MountState::default());
    let fuse = MemoryFuse::new_with_state(fs, store_id.clone(), state.clone())?;
    // Capture a cache handle before `fuse` is moved into the kernel session.
    let listener = invalidations.map(|rx| {
        let cache = fuse.cache_handle();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            run_invalidation_listener(cache, store_id, rx, stop_thread);
        });
        InvalidationListener { stop, handle }
    });
    let session = fuser::spawn_mount2(fuse, mountpoint, &mount_options())
        .map_err(|e| FuseError::Internal(e.to_string()))?;
    Ok(MemoryMountHandle {
        session: Some(session),
        state,
        drain_timeout: DEFAULT_UNMOUNT_DRAIN_TIMEOUT,
        listener,
    })
}

fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("awaken-memory".into()),
        MountOption::NoExec,
        MountOption::NoSuid,
        MountOption::NoDev,
    ]
}

/// The FUSE filesystem projecting one memory `store` over a [`MemoryRepository`].
pub struct MemoryFuse {
    fs: Arc<dyn MemoryRepository>,
    store_id: String,
    runtime: Runtime,
    mount_time: u128,
    inodes: Mutex<InodeTable>,
    content_cache: Arc<Mutex<ContentLruCache>>,
    mount_state: Arc<MountState>,
}

#[derive(Default)]
struct InodeTable {
    next: u64,
    next_fh: u64,
    by_path: HashMap<String, u64>,
    by_ino: HashMap<u64, Node>,
    open_files: HashMap<u64, OpenFile>,
    dirty_order: VecDeque<u64>,
}

#[derive(Clone)]
struct Node {
    path: String,
    kind: FileType,
    size: u64,
    created: u128,
    updated: u128,
}

#[derive(Clone)]
struct OpenFile {
    memory_id: String,
    base_sha256: String,
    buffer: Vec<u8>,
    dirty: bool,
}

struct ContentLruCache {
    capacity: usize,
    entries: HashMap<String, CachedMemory>,
    order: VecDeque<String>,
}

struct CachedMemory {
    memory: Memory,
    inserted_at: Instant,
}

impl ContentLruCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, path: &str) -> Option<Memory> {
        let entry = self.entries.get(path)?;
        if entry.inserted_at.elapsed() > CONTENT_CACHE_TTL {
            self.remove_path(path);
            return None;
        }
        let value = entry.memory.clone();
        self.touch(path);
        Some(value)
    }

    fn put(&mut self, memory: Memory) {
        if self.capacity == 0 {
            return;
        }
        let path = memory.path.clone();
        self.entries.insert(
            path.clone(),
            CachedMemory {
                memory,
                inserted_at: Instant::now(),
            },
        );
        self.touch(&path);
        self.evict_over_capacity();
    }

    fn remove_path(&mut self, path: &str) {
        self.entries.remove(path);
        self.order.retain(|c| c != path);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn clear_path_prefix(&mut self, prefix: &str) {
        self.entries.retain(|path, _| !path.starts_with(prefix));
        self.order.retain(|path| self.entries.contains_key(path));
    }

    fn touch(&mut self, path: &str) {
        self.order.retain(|c| c != path);
        self.order.push_back(path.to_string());
    }

    fn evict_over_capacity(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(path) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&path);
        }
    }
}

impl MemoryFuse {
    /// Build a filesystem over `fs` for one `store_id` (no mount yet).
    pub fn new(fs: Arc<dyn MemoryRepository>, store_id: String) -> Result<Self, FuseError> {
        Self::new_with_state(fs, store_id, Arc::new(MountState::default()))
    }

    fn new_with_state(
        fs: Arc<dyn MemoryRepository>,
        store_id: String,
        mount_state: Arc<MountState>,
    ) -> Result<Self, FuseError> {
        let mount_time = now_nanos();
        let mut table = InodeTable {
            next: ROOT_INO + 1,
            next_fh: 1,
            ..InodeTable::default()
        };
        table.by_path.insert("/".into(), ROOT_INO);
        table.by_ino.insert(
            ROOT_INO,
            Node {
                path: "/".into(),
                kind: FileType::Directory,
                size: 0,
                created: mount_time,
                updated: mount_time,
            },
        );
        Ok(Self {
            fs,
            store_id,
            runtime: Runtime::new().map_err(|e| FuseError::Internal(e.to_string()))?,
            mount_time,
            inodes: Mutex::new(table),
            content_cache: Arc::new(Mutex::new(ContentLruCache::new(CONTENT_CACHE_CAPACITY))),
            mount_state,
        })
    }

    fn attr(ino: u64, node: &Node) -> FileAttr {
        let mtime = to_systime(node.updated);
        FileAttr {
            ino,
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: to_systime(node.created),
            kind: node.kind,
            perm: if node.kind == FileType::Directory {
                0o755
            } else {
                0o644
            },
            nlink: if node.kind == FileType::Directory {
                2
            } else {
                1
            },
            uid: 0,
            gid: 0,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn intern(
        &self,
        path: String,
        kind: FileType,
        size: u64,
        created: u128,
        updated: u128,
    ) -> (u64, Node) {
        let mut table = self.inodes.lock().expect("inode table mutex poisoned");
        if let Some(ino) = table.by_path.get(&path).copied() {
            let node = table.by_ino.get_mut(&ino).expect("inode node");
            node.kind = kind;
            node.size = size;
            node.created = created;
            node.updated = updated;
            return (ino, node.clone());
        }
        let ino = table.next;
        table.next += 1;
        let node = Node {
            path: path.clone(),
            kind,
            size,
            created,
            updated,
        };
        table.by_path.insert(path, ino);
        table.by_ino.insert(ino, node.clone());
        (ino, node)
    }

    fn intern_memory(&self, memory: &Memory) -> (u64, Node) {
        self.intern(
            memory.path.clone(),
            FileType::RegularFile,
            memory.content_size,
            memory.created_unix_nanos,
            memory.updated_unix_nanos,
        )
    }

    fn intern_dir(&self, path: &str) -> (u64, Node) {
        self.intern(
            path.to_string(),
            FileType::Directory,
            0,
            self.mount_time,
            self.mount_time,
        )
    }

    fn node(&self, ino: u64) -> Option<Node> {
        self.inodes
            .lock()
            .expect("inode table mutex poisoned")
            .by_ino
            .get(&ino)
            .cloned()
    }

    fn child_path(parent: &str, name: &OsStr) -> Option<String> {
        let name = name.to_str()?;
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return None;
        }
        Some(if parent == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", parent.trim_end_matches('/'))
        })
    }

    fn lookup_path(&self, path: &str) -> Result<(u64, Node), FuseError> {
        if path == "/" {
            return Ok((ROOT_INO, self.node(ROOT_INO).expect("root node")));
        }
        if let Some(memory) = self.cache_get(path) {
            return Ok(self.intern_memory(&memory));
        }
        match self
            .runtime
            .block_on(self.fs.get_by_path(&self.store_id, path))
        {
            Ok(Some(memory)) => {
                self.cache_put(memory.clone());
                Ok(self.intern_memory(&memory))
            }
            Ok(None) => {
                // Not a file — a synthetic directory iff something lives under it.
                let prefix = format!("{}/", path.trim_end_matches('/'));
                let children = self
                    .runtime
                    .block_on(self.fs.list(&self.store_id, &prefix))?;
                if children.is_empty() {
                    Err(FuseError::NotFound(path.to_string()))
                } else {
                    Ok(self.intern_dir(path))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn path_for_ino(&self, ino: u64) -> Result<String, FuseError> {
        self.node(ino)
            .map(|node| node.path)
            .ok_or_else(|| FuseError::NotFound(format!("inode {ino}")))
    }

    fn allocate_fh(&self, open: OpenFile) -> u64 {
        let mut table = self.inodes.lock().expect("inode table mutex poisoned");
        let fh = table.next_fh;
        table.next_fh += 1;
        table.open_files.insert(fh, open);
        self.mount_state.open_fds.fetch_add(1, Ordering::SeqCst);
        fh
    }

    /// A shared handle to the content cache, so a peer-projection invalidation listener
    /// can drop stale paths after the fuse is moved into the kernel session.
    fn cache_handle(&self) -> Arc<Mutex<ContentLruCache>> {
        self.content_cache.clone()
    }

    fn cache_get(&self, path: &str) -> Option<Memory> {
        self.content_cache
            .lock()
            .expect("content cache mutex poisoned")
            .get(path)
    }

    fn cache_put(&self, memory: Memory) {
        self.content_cache
            .lock()
            .expect("content cache mutex poisoned")
            .put(memory);
    }

    fn cache_remove_path(&self, path: &str) {
        self.content_cache
            .lock()
            .expect("content cache mutex poisoned")
            .remove_path(path);
    }

    fn cache_clear_prefix(&self, prefix: &str) {
        self.content_cache
            .lock()
            .expect("content cache mutex poisoned")
            .clear_path_prefix(prefix);
    }

    fn open_file_for_path(&self, path: &str, flags: i32) -> Result<u64, FuseError> {
        let memory = match self.cache_get(path) {
            Some(memory) => memory,
            None => match self
                .runtime
                .block_on(self.fs.get_by_path(&self.store_id, path))?
            {
                Some(memory) => {
                    self.cache_put(memory.clone());
                    memory
                }
                None => return Err(FuseError::NotFound(path.to_string())),
            },
        };
        let mut buffer = memory.content.clone().unwrap_or_default().into_bytes();
        let truncate = flags & O_TRUNC != 0;
        if truncate {
            buffer.clear();
        }
        let fh = self.allocate_fh(OpenFile {
            memory_id: memory.id.clone(),
            base_sha256: memory.content_sha256.clone(),
            buffer,
            dirty: false,
        });
        if truncate && let Err(error) = self.mark_fh_dirty(fh) {
            self.remove_fh(fh);
            return Err(error);
        }
        Ok(fh)
    }

    fn create_file_for_path(&self, path: &str) -> Result<(u64, u64, Node), FuseError> {
        let memory = self
            .runtime
            .block_on(self.fs.create(&self.store_id, path, ""))?;
        let (ino, node) = self.intern_memory(&memory);
        let fh = self.allocate_fh(OpenFile {
            memory_id: memory.id.clone(),
            base_sha256: memory.content_sha256.clone(),
            buffer: Vec::new(),
            dirty: false,
        });
        self.cache_put(memory);
        Ok((fh, ino, node))
    }

    fn mark_fh_dirty(&self, fh: u64) -> Result<(), FuseError> {
        let mut table = self.inodes.lock().expect("inode table mutex poisoned");
        Self::dirty_open_file_mut(&mut table, fh).map(|_| ())
    }

    /// Resize the buffer of every write fd open for the memory at `path` to `size`,
    /// marking each dirty so the truncation rides that fd's flush rather than racing
    /// an independent store write (which would advance the sha out from under the
    /// open fd). Returns `true` if at least one fd was retargeted.
    fn truncate_open_writers(&self, path: &str, size: usize) -> Result<bool, FuseError> {
        let memory_id = self.cache_get(path).map(|memory| memory.id);
        let Some(memory_id) = memory_id else {
            return Ok(false);
        };
        let mut table = self.inodes.lock().expect("inode table mutex poisoned");
        let fhs: Vec<u64> = table
            .open_files
            .iter()
            .filter(|(_, open)| open.memory_id == memory_id)
            .map(|(fh, _)| *fh)
            .collect();
        if fhs.is_empty() {
            return Ok(false);
        }
        for fh in fhs {
            let open = Self::dirty_open_file_mut(&mut table, fh)?;
            open.buffer.resize(size, 0);
        }
        Ok(true)
    }

    /// The directory entries for `ino` (`.`, `..`, then each immediate child),
    /// interning each child. `NotFound` if `ino` is unknown or not a directory.
    fn readdir_entries(&self, ino: u64) -> Result<Vec<(u64, FileType, String)>, FuseError> {
        let node = self
            .node(ino)
            .filter(|n| n.kind == FileType::Directory)
            .ok_or_else(|| FuseError::NotFound(format!("dir inode {ino}")))?;
        let list_prefix = if node.path == "/" {
            "/".to_string()
        } else {
            format!("{}/", node.path.trim_end_matches('/'))
        };
        let memories = self
            .runtime
            .block_on(self.fs.list(&self.store_id, &list_prefix))?;
        let mut entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ROOT_INO, FileType::Directory, "..".to_string()),
        ];
        for (name, is_dir) in immediate_children(&memories, &node.path) {
            let path = if node.path == "/" {
                format!("/{name}")
            } else {
                format!("{}/{name}", node.path.trim_end_matches('/'))
            };
            let (child_ino, child) = if is_dir {
                self.intern_dir(&path)
            } else {
                let mem = memories.iter().find(|m| m.path == path);
                let size = mem.map(|m| m.content_size).unwrap_or(0);
                let updated = mem.map(|m| m.updated_unix_nanos).unwrap_or(self.mount_time);
                self.intern(path, FileType::RegularFile, size, updated, updated)
            };
            entries.push((child_ino, child.kind, name));
        }
        Ok(entries)
    }

    /// Apply a `setattr` (only `size` is honored — see ADR-0053 D4). `size == None`
    /// is a metadata-only setattr (returns the current attr). With a size and an open
    /// `fh`, the truncation resizes that fd's buffer (rides its flush). With a size and
    /// no fh, it retargets any open writer, else updates the store directly. Returns
    /// the resulting node for the reply.
    fn apply_setattr(
        &self,
        ino: u64,
        size: Option<usize>,
        fh: Option<u64>,
    ) -> Result<Node, FuseError> {
        let node = self
            .node(ino)
            .ok_or_else(|| FuseError::NotFound(format!("inode {ino}")))?;
        let Some(size) = size else {
            return Ok(node);
        };
        if let Some(fh) = fh {
            let mut table = self.inodes.lock().expect("inode table mutex poisoned");
            Self::dirty_open_file_mut(&mut table, fh)?
                .buffer
                .resize(size, 0);
            let mut node = table
                .by_ino
                .get(&ino)
                .cloned()
                .ok_or_else(|| FuseError::NotFound(format!("inode {ino}")))?;
            node.size = size as u64;
            return Ok(node);
        }
        // No fh (a `truncate(path)` syscall): if a writer is open, the truncation
        // rides its buffer/flush; otherwise update the store directly.
        if self.truncate_open_writers(&node.path, size)? {
            let mut node = node;
            node.size = size as u64;
            return Ok(node);
        }
        let memory = self.runtime.block_on(async {
            let current = self
                .fs
                .get_by_path(&self.store_id, &node.path)
                .await?
                .ok_or_else(|| MemErr::NotFound(node.path.clone()))?;
            let mut bytes = current.content.clone().unwrap_or_default().into_bytes();
            bytes.resize(size, 0);
            let content = String::from_utf8(bytes).map_err(|e| MemErr::Storage(e.to_string()))?;
            self.fs
                .update(
                    &self.store_id,
                    &current.id,
                    &content,
                    &current.content_sha256,
                )
                .await
        })?;
        self.cache_put(memory.clone());
        Ok(self.intern_memory(&memory).1)
    }

    fn dirty_open_file_mut(table: &mut InodeTable, fh: u64) -> Result<&mut OpenFile, FuseError> {
        let already_dirty = table
            .open_files
            .get(&fh)
            .ok_or_else(|| FuseError::NotFound(format!("fh {fh}")))?
            .dirty;
        if !already_dirty && table.dirty_order.len() >= MAX_DIRTY_OPEN_FILES {
            return Err(FuseError::DirtyFileLimitExceeded);
        }
        if already_dirty {
            table.dirty_order.retain(|c| *c != fh);
        }
        table.dirty_order.push_back(fh);
        let open = table
            .open_files
            .get_mut(&fh)
            .ok_or_else(|| FuseError::NotFound(format!("fh {fh}")))?;
        open.dirty = true;
        Ok(open)
    }

    fn mark_fh_clean(table: &mut InodeTable, fh: u64) {
        table.dirty_order.retain(|c| *c != fh);
    }

    fn read_fh_bytes(&self, fh: u64, offset: i64, size: u32) -> Result<Vec<u8>, FuseError> {
        let table = self.inodes.lock().expect("inode table mutex poisoned");
        let open = table
            .open_files
            .get(&fh)
            .ok_or_else(|| FuseError::NotFound(format!("fh {fh}")))?;
        let start = usize::try_from(offset.max(0))
            .unwrap_or(0)
            .min(open.buffer.len());
        let end = start.saturating_add(size as usize).min(open.buffer.len());
        Ok(open.buffer[start..end].to_vec())
    }

    fn write_fh_bytes(&self, fh: u64, offset: i64, data: &[u8]) -> Result<(), FuseError> {
        let mut table = self.inodes.lock().expect("inode table mutex poisoned");
        let open = Self::dirty_open_file_mut(&mut table, fh)?;
        let content = String::from_utf8(open.buffer.clone())
            .map_err(|e| FuseError::Internal(e.to_string()))?;
        open.buffer = splice_bytes(&content, offset, data)?.into_bytes();
        Ok(())
    }

    fn flush_fh(&self, fh: u64) -> Result<(), FuseError> {
        let open = {
            let table = self.inodes.lock().expect("inode table mutex poisoned");
            table
                .open_files
                .get(&fh)
                .cloned()
                .ok_or_else(|| FuseError::NotFound(format!("fh {fh}")))?
        };
        if !open.dirty {
            return Ok(());
        }
        let content = String::from_utf8(open.buffer.clone())
            .map_err(|e| FuseError::Internal(e.to_string()))?;
        let updated = match self.runtime.block_on(self.fs.update(
            &self.store_id,
            &open.memory_id,
            &content,
            &open.base_sha256,
        )) {
            Ok(updated) => updated,
            Err(MemErr::Conflict { current }) => {
                tracing::warn!(
                    memory_id = %open.memory_id,
                    expected_sha256 = %open.base_sha256,
                    current_sha256 = %current.content_sha256,
                    current_content_size = current.content_size,
                    "memory FUSE fd flush conflict — buffer kept, EAGAIN surfaced"
                );
                return Err(FuseError::Mem(MemErr::Conflict { current }));
            }
            Err(error) => return Err(error.into()),
        };
        {
            let mut table = self.inodes.lock().expect("inode table mutex poisoned");
            if let Some(current) = table.open_files.get_mut(&fh) {
                current.base_sha256 = updated.content_sha256.clone();
                current.dirty = false;
            }
            Self::mark_fh_clean(&mut table, fh);
        }
        self.cache_put(updated.clone());
        self.intern_memory(&updated);
        Ok(())
    }

    fn remove_fh(&self, fh: u64) {
        let removed = {
            let mut table = self.inodes.lock().expect("inode table mutex poisoned");
            Self::mark_fh_clean(&mut table, fh);
            table.open_files.remove(&fh)
        };
        if removed.is_some() {
            self.mount_state.open_fds.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn errno(error: FuseError) -> i32 {
        match error {
            FuseError::NotFound(_) | FuseError::Mem(MemErr::NotFound(_)) => ENOENT,
            FuseError::Mem(MemErr::PathConflict(_)) => EEXIST,
            FuseError::Mem(MemErr::InvalidPath(_)) => EINVAL,
            FuseError::Mem(MemErr::AtCapacity) => ENOSPC,
            FuseError::DirtyFileLimitExceeded | FuseError::Mem(MemErr::Conflict { .. }) => EAGAIN,
            _ => EIO,
        }
    }
}

impl Filesystem for MemoryFuse {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_node) = self.node(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(path) = Self::child_path(&parent_node.path, name) else {
            reply.error(ENOENT);
            return;
        };
        match self.lookup_path(&path) {
            Ok((ino, node)) => reply.entry(&TTL, &Self::attr(ino, &node), 0),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        if let Some(fh) = fh {
            let table = self.inodes.lock().expect("inode table mutex poisoned");
            if let (Some(node), Some(open)) = (table.by_ino.get(&ino), table.open_files.get(&fh)) {
                let mut node = node.clone();
                node.size = open.buffer.len() as u64;
                reply.attr(&TTL, &Self::attr(ino, &node));
                return;
            }
        }
        if let Some(node) = self.node(ino) {
            reply.attr(&TTL, &Self::attr(ino, &node));
        } else {
            reply.error(ENOENT);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        match self.apply_setattr(ino, size.map(|s| s as usize), fh) {
            Ok(node) => reply.attr(&TTL, &Self::attr(ino, &node)),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let path = match self.path_for_ino(ino) {
            Ok(path) => path,
            Err(error) => {
                reply.error(Self::errno(error));
                return;
            }
        };
        match self.open_file_for_path(&path, flags) {
            Ok(fh) => reply.opened(fh, 0),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.readdir_entries(ino) {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(Self::errno(error));
                return;
            }
        };
        for (idx, (entry_ino, kind, name)) in
            entries.into_iter().enumerate().skip(offset.max(0) as usize)
        {
            if reply.add(entry_ino, (idx + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // A read always follows an `open` that handed back a nonzero fh, so we serve
        // from that fd's buffer; an unknown fh surfaces `ENOENT`.
        match self.read_fh_bytes(fh, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_node) = self.node(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(path) = Self::child_path(&parent_node.path, name) else {
            reply.error(ENOENT);
            return;
        };
        match self.create_file_for_path(&path) {
            Ok((fh, ino, node)) => reply.created(&TTL, &Self::attr(ino, &node), 0, fh, 0),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        // A write always follows an `open` that handed back a nonzero fh; it buffers
        // into that fd (flushed as one CAS write on close). An unknown fh → `ENOENT`.
        match self.write_fh_bytes(fh, offset, data) {
            Ok(()) => reply.written(data.len() as u32),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        match self.flush_fh(fh) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.flush_fh(fh) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.flush_fh(fh);
        self.remove_fh(fh);
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(Self::errno(error)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags != 0 {
            reply.error(ENOSYS);
            return;
        }
        let (Some(parent_node), Some(newparent_node)) = (self.node(parent), self.node(newparent))
        else {
            reply.error(ENOENT);
            return;
        };
        let (Some(old_path), Some(new_path)) = (
            Self::child_path(&parent_node.path, name),
            Self::child_path(&newparent_node.path, newname),
        ) else {
            reply.error(ENOENT);
            return;
        };
        match self
            .runtime
            .block_on(self.fs.rename(&self.store_id, &old_path, &new_path))
        {
            Ok(memory) => {
                self.cache_remove_path(&old_path);
                self.cache_put(memory.clone());
                // Remap the inode in place so already-open fds keep their id/buffer.
                let mut table = self.inodes.lock().expect("inode table mutex poisoned");
                if let Some(ino) = table.by_path.remove(&old_path) {
                    table.by_path.insert(memory.path.clone(), ino);
                    if let Some(node) = table.by_ino.get_mut(&ino) {
                        node.path = memory.path;
                        node.size = memory.content_size;
                        node.updated = memory.updated_unix_nanos;
                    }
                }
                reply.ok();
            }
            Err(error) => reply.error(Self::errno(error.into())),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_node) = self.node(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(path) = Self::child_path(&parent_node.path, name) else {
            reply.error(ENOENT);
            return;
        };
        match self
            .runtime
            .block_on(self.fs.delete_by_path(&self.store_id, &path))
        {
            Ok(()) => {
                self.cache_remove_path(&path);
                reply.ok();
            }
            Err(error) => reply.error(Self::errno(error.into())),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_node) = self.node(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(path) = Self::child_path(&parent_node.path, name) else {
            reply.error(ENOENT);
            return;
        };
        // Directories are synthetic (implied by memory paths); no store write.
        let (ino, node) = self.intern_dir(&path);
        reply.entry(&TTL, &Self::attr(ino, &node), 0);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_node) = self.node(parent) else {
            reply.error(ENOENT);
            return;
        };
        let Some(path) = Self::child_path(&parent_node.path, name) else {
            reply.error(ENOENT);
            return;
        };
        let prefix = format!("{}/", path.trim_end_matches('/'));
        match self.runtime.block_on(self.fs.list(&self.store_id, &prefix)) {
            Ok(children) if children.is_empty() => {
                self.cache_clear_prefix(&prefix);
                reply.ok();
            }
            Ok(_) => reply.error(ENOTEMPTY),
            Err(error) => reply.error(Self::errno(error.into())),
        }
    }
}

#[cfg(test)]
#[path = "fuse/tests.rs"]
mod tests;
