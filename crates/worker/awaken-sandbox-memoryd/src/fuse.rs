//! The fuser-backed write-through memory filesystem (ADR-0053, D2/D3).
//!
//! A faithful port of awaken-next's `memoryd` FUSE, over an in-process
//! [`MemoryFs`] instead of an HTTP client, and reporting the store's real
//! create/update timestamps in `getattr` (awaken-next reported `now()`).

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use awaken_memory_store::{MemErr, Memory, MemoryFs};
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use libc::{EAGAIN, EEXIST, EINVAL, EIO, ENOENT, ENOSYS, ENOTEMPTY, O_TRUNC};
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

use crate::invalidate::Invalidation;
use crate::{FuseError, immediate_children, splice_bytes};

/// Poll cadence for the cross-host invalidation listener (ADR-0053 D5). An
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

/// A background thread draining cross-host invalidations into a mount's cache
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
                    cache.remove_path(&path);
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
    pub fn unmount(mut self) {
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
    fs: Arc<dyn MemoryFs>,
    store_id: String,
    mountpoint: PathBuf,
) -> Result<MemoryMountHandle, FuseError> {
    spawn_mount_inner(fs, store_id, mountpoint, None)
}

/// Like [`spawn_mount`], but the mount also drains `invalidations` — a cross-host
/// invalidation feed (ADR-0053 D5) — dropping stale paths from its cache so a write
/// on another node is reflected here. Pass a [`LocalInvalidator`](crate::LocalInvalidator)
/// subscription (or a NATS/pg-notify bridge over the same broadcast).
pub fn spawn_mount_with_invalidations(
    fs: Arc<dyn MemoryFs>,
    store_id: String,
    mountpoint: PathBuf,
    invalidations: broadcast::Receiver<Invalidation>,
) -> Result<MemoryMountHandle, FuseError> {
    spawn_mount_inner(fs, store_id, mountpoint, Some(invalidations))
}

fn spawn_mount_inner(
    fs: Arc<dyn MemoryFs>,
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

/// The FUSE filesystem projecting one memory `store` over a [`MemoryFs`].
pub struct MemoryFuse {
    fs: Arc<dyn MemoryFs>,
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
    pub fn new(fs: Arc<dyn MemoryFs>, store_id: String) -> Result<Self, FuseError> {
        Self::new_with_state(fs, store_id, Arc::new(MountState::default()))
    }

    fn new_with_state(
        fs: Arc<dyn MemoryFs>,
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

    /// A shared handle to the content cache, so a cross-host invalidation listener
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
mod tests {
    use super::*;
    use awaken_memory_store::{InMemoryFs, sha256_hex};

    fn test_fs() -> MemoryFuse {
        MemoryFuse::new(Arc::new(InMemoryFs::new()), "memstore_test".into()).unwrap()
    }

    fn memory(path: &str, content: &str) -> Memory {
        Memory {
            id: format!("id-{path}"),
            path: path.to_string(),
            content_sha256: sha256_hex(content),
            content_size: content.len() as u64,
            version: 1,
            created_unix_nanos: 100,
            updated_unix_nanos: 200,
            content: Some(content.to_string()),
        }
    }

    #[test]
    fn invalidation_listener_drops_matching_paths_only() {
        use crate::LocalInvalidator;
        use crate::invalidate::Invalidator;

        let cache = Arc::new(Mutex::new(ContentLruCache::new(8)));
        cache.lock().unwrap().put(memory("/a.md", "one"));
        cache.lock().unwrap().put(memory("/b.md", "two"));

        let bus = LocalInvalidator::new(16);
        let rx = bus.subscribe();
        let stop = Arc::new(AtomicBool::new(false));
        let listener = {
            let (cache, stop) = (cache.clone(), stop.clone());
            std::thread::spawn(move || run_invalidation_listener(cache, "s".into(), rx, stop))
        };

        // A write on another node for our store drops that path here…
        bus.publish("s", "/a.md");
        // …while a write for a different store is ignored.
        bus.publish("other", "/b.md");

        // Wait for the listener to observe the invalidation (poll cadence is 20ms).
        let mut waited = Duration::ZERO;
        while cache.lock().unwrap().get("/a.md").is_some() && waited < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
            waited += Duration::from_millis(10);
        }
        assert!(
            cache.lock().unwrap().get("/a.md").is_none(),
            "matching path dropped"
        );
        assert!(
            cache.lock().unwrap().get("/b.md").is_some(),
            "other store untouched"
        );

        stop.store(true, Ordering::SeqCst);
        listener.join().unwrap();
    }

    #[test]
    fn fuse_constructs_with_a_seeded_root() {
        let fs = test_fs();
        let root = fs.node(ROOT_INO).expect("root node");
        assert_eq!(root.kind, FileType::Directory);
    }

    #[test]
    fn attr_reports_the_records_timestamps_not_now() {
        let fs = test_fs();
        let (ino, node) = fs.intern_memory(&memory("/a.md", "hi"));
        let attr = MemoryFuse::attr(ino, &node);
        assert_eq!(
            attr.crtime,
            to_systime(100),
            "crtime is the record's created"
        );
        assert_eq!(attr.mtime, to_systime(200), "mtime is the record's updated");
    }

    #[test]
    fn content_lru_cache_evicts_oldest_and_refreshes_on_get() {
        let mut cache = ContentLruCache::new(2);
        cache.put(memory("/a.md", "a"));
        cache.put(memory("/b.md", "b"));
        assert!(cache.get("/a.md").is_some());
        cache.put(memory("/c.md", "c"));
        assert!(cache.get("/a.md").is_some());
        assert!(
            cache.get("/b.md").is_none(),
            "the least-recently-used entry evicted"
        );
        assert!(cache.get("/c.md").is_some());
    }

    #[test]
    fn open_fd_count_tracks_allocated_handles() {
        let fs = test_fs();
        let a = fs.allocate_fh(OpenFile {
            memory_id: "m1".into(),
            base_sha256: sha256_hex("one"),
            buffer: b"one".to_vec(),
            dirty: false,
        });
        let b = fs.allocate_fh(OpenFile {
            memory_id: "m2".into(),
            base_sha256: sha256_hex("two"),
            buffer: b"two".to_vec(),
            dirty: false,
        });
        assert_eq!(fs.mount_state.open_fds.load(Ordering::SeqCst), 2);
        fs.remove_fh(a);
        assert_eq!(fs.mount_state.open_fds.load(Ordering::SeqCst), 1);
        fs.remove_fh(b);
        assert_eq!(fs.mount_state.open_fds.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dirty_fd_budget_fails_closed_at_the_limit() {
        let fs = test_fs();
        for index in 0..MAX_DIRTY_OPEN_FILES {
            let fh = fs.allocate_fh(OpenFile {
                memory_id: format!("m{index}"),
                base_sha256: sha256_hex("clean"),
                buffer: Vec::new(),
                dirty: false,
            });
            fs.write_fh_bytes(fh, 0, b"x").expect("dirty fd admitted");
        }
        let blocked = fs.allocate_fh(OpenFile {
            memory_id: "blocked".into(),
            base_sha256: sha256_hex("blocked"),
            buffer: Vec::new(),
            dirty: false,
        });
        assert!(
            matches!(
                fs.write_fh_bytes(blocked, 0, b"x"),
                Err(FuseError::DirtyFileLimitExceeded)
            ),
            "a new dirty fd past the budget fails closed"
        );
    }

    // These drive the real open/write/flush/rename code paths over a live
    // `InMemoryFs` (no kernel needed). The `MemoryFuse` owns its own runtime and
    // its methods block on it internally, so the test stays synchronous and uses a
    // separate runtime only for external store setup — the two never nest.
    fn setup_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn a_stale_open_fd_conflicts_and_does_not_clobber_a_newer_write() {
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        let created = rt.block_on(backend.create(store, "/c.md", "v1")).unwrap();

        let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
        // Open captures base sha = sha("v1").
        let fh = fuse.open_file_for_path("/c.md", 0).unwrap();

        // Another writer advances the store out from under the open fd.
        rt.block_on(backend.update(store, &created.id, "server-wins", &created.content_sha256))
            .unwrap();

        // The stale fd writes and flushes: CAS on the now-stale base sha conflicts,
        // the buffer is kept, and the store's newer content is NOT clobbered.
        fuse.write_fh_bytes(fh, 0, b"stale-write").unwrap();
        assert!(
            matches!(
                fuse.flush_fh(fh),
                Err(FuseError::Mem(MemErr::Conflict { .. }))
            ),
            "the stale flush conflicts"
        );
        let live = rt
            .block_on(backend.get_by_path(store, "/c.md"))
            .unwrap()
            .unwrap();
        assert_eq!(
            live.content.as_deref(),
            Some("server-wins"),
            "the newer write survives; the stale fd did not clobber it"
        );
    }

    #[test]
    fn errno_maps_every_fault_class() {
        use awaken_memory_store::Memory;
        let mem = || {
            Box::new(Memory {
                id: "i".into(),
                path: "/p".into(),
                content_sha256: String::new(),
                content_size: 0,
                version: 1,
                created_unix_nanos: 0,
                updated_unix_nanos: 0,
                content: None,
            })
        };
        assert_eq!(MemoryFuse::errno(FuseError::NotFound("x".into())), ENOENT);
        assert_eq!(
            MemoryFuse::errno(FuseError::Mem(MemErr::NotFound("x".into()))),
            ENOENT
        );
        assert_eq!(
            MemoryFuse::errno(FuseError::Mem(MemErr::PathConflict("x".into()))),
            EEXIST
        );
        assert_eq!(
            MemoryFuse::errno(FuseError::Mem(MemErr::InvalidPath("x".into()))),
            EINVAL
        );
        assert_eq!(MemoryFuse::errno(FuseError::DirtyFileLimitExceeded), EAGAIN);
        assert_eq!(
            MemoryFuse::errno(FuseError::Mem(MemErr::Conflict { current: mem() })),
            EAGAIN
        );
        assert_eq!(MemoryFuse::errno(FuseError::TooLarge), EIO);
        assert_eq!(MemoryFuse::errno(FuseError::Internal("x".into())), EIO);
        assert_eq!(
            MemoryFuse::errno(FuseError::Mem(MemErr::Storage("x".into()))),
            EIO
        );
    }

    #[test]
    fn lookup_path_resolves_files_synthetic_dirs_and_missing() {
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        rt.block_on(backend.create(store, "/notes/a.md", "a"))
            .unwrap();

        let fuse = MemoryFuse::new(backend, store.into()).unwrap();
        // root
        let (ino, node) = fuse.lookup_path("/").unwrap();
        assert_eq!(ino, ROOT_INO);
        assert_eq!(node.kind, FileType::Directory);
        // a real file
        let (_, f) = fuse.lookup_path("/notes/a.md").unwrap();
        assert_eq!(f.kind, FileType::RegularFile);
        // a synthetic directory (something lives under it)
        let (_, d) = fuse.lookup_path("/notes").unwrap();
        assert_eq!(d.kind, FileType::Directory);
        // nothing there
        assert!(matches!(
            fuse.lookup_path("/ghost"),
            Err(FuseError::NotFound(_))
        ));
        assert!(matches!(
            fuse.lookup_path("/notes/ghost"),
            Err(FuseError::NotFound(_))
        ));
    }

    #[test]
    fn open_create_flush_read_inner_branches() {
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        rt.block_on(backend.create(store, "/f.md", "orig")).unwrap();
        let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();

        // open a missing path → NotFound.
        assert!(matches!(
            fuse.open_file_for_path("/ghost", 0),
            Err(FuseError::NotFound(_))
        ));

        // open with O_TRUNC clears the buffer and marks it dirty.
        let fh = fuse.open_file_for_path("/f.md", O_TRUNC).unwrap();
        assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"");

        // read on an unknown fh → NotFound.
        assert!(matches!(
            fuse.read_fh_bytes(9999, 0, 1),
            Err(FuseError::NotFound(_))
        ));

        // flush the truncated fd → the store now holds empty content.
        fuse.flush_fh(fh).unwrap();
        assert_eq!(
            rt.block_on(backend.get_by_path(store, "/f.md"))
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some(""),
            "O_TRUNC open flushed empty"
        );
        // flushing a clean fd is a no-op (dirty was cleared on the prior flush).
        fuse.flush_fh(fh).unwrap();

        // create over an existing path → PathConflict (→ EEXIST).
        assert!(matches!(
            fuse.create_file_for_path("/f.md"),
            Err(FuseError::Mem(MemErr::PathConflict(_)))
        ));

        // truncate_open_writers retargets an open writer (true), else no-op (false).
        let w = fuse.open_file_for_path("/f.md", 0).unwrap();
        assert!(fuse.truncate_open_writers("/f.md", 0).unwrap());
        assert!(!fuse.truncate_open_writers("/absent", 0).unwrap());
        fuse.remove_fh(w);
    }

    #[test]
    fn readdir_entries_lists_children_and_rejects_non_directories() {
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        rt.block_on(backend.create(store, "/a.md", "a")).unwrap();
        rt.block_on(backend.create(store, "/sub/b.md", "b"))
            .unwrap();
        let fuse = MemoryFuse::new(backend, store.into()).unwrap();

        let root: Vec<String> = fuse
            .readdir_entries(ROOT_INO)
            .unwrap()
            .into_iter()
            .map(|(_, _, n)| n)
            .collect();
        assert!(root.contains(&"a.md".to_string()) && root.contains(&"sub".to_string()));
        assert!(root.contains(&".".to_string()) && root.contains(&"..".to_string()));

        // an unknown inode and a FILE inode both reject as not-a-directory.
        assert!(matches!(
            fuse.readdir_entries(9999),
            Err(FuseError::NotFound(_))
        ));
        let (file_ino, _) = fuse.lookup_path("/a.md").unwrap();
        assert!(matches!(
            fuse.readdir_entries(file_ino),
            Err(FuseError::NotFound(_))
        ));

        // the synthetic subdirectory lists its child.
        let (dir_ino, _) = fuse.lookup_path("/sub").unwrap();
        let sub: Vec<String> = fuse
            .readdir_entries(dir_ino)
            .unwrap()
            .into_iter()
            .map(|(_, _, n)| n)
            .collect();
        assert!(sub.contains(&"b.md".to_string()));
    }

    #[test]
    fn apply_setattr_handles_metadata_fd_and_store_truncation() {
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        rt.block_on(backend.create(store, "/f.md", "hello"))
            .unwrap();
        let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
        let (ino, _) = fuse.lookup_path("/f.md").unwrap();

        // size = None → metadata-only, returns the current node unchanged.
        assert_eq!(fuse.apply_setattr(ino, None, None).unwrap().size, 5);

        // no fh, no open writer → truncate the store directly.
        assert_eq!(fuse.apply_setattr(ino, Some(2), None).unwrap().size, 2);
        assert_eq!(
            rt.block_on(backend.get_by_path(store, "/f.md"))
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("he")
        );

        // with an open fh → resize that fd's buffer.
        let fh = fuse.open_file_for_path("/f.md", 0).unwrap();
        assert_eq!(fuse.apply_setattr(ino, Some(1), Some(fh)).unwrap().size, 1);
        fuse.remove_fh(fh);

        // an unknown inode → NotFound.
        assert!(matches!(
            fuse.apply_setattr(9999, Some(0), None),
            Err(FuseError::NotFound(_))
        ));
    }

    #[test]
    fn an_open_fd_survives_rename_and_flushes_to_the_renamed_memory() {
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        rt.block_on(backend.create(store, "/old.md", "snapshot"))
            .unwrap();

        let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
        let fh = fuse.open_file_for_path("/old.md", 0).unwrap();

        // Rename the memory while the fd is open (through the fs, as the FUSE rename
        // does); the fd keeps its buffer and id.
        rt.block_on(backend.rename(store, "/old.md", "/new.md"))
            .unwrap();
        assert_eq!(
            fuse.read_fh_bytes(fh, 0, 64).unwrap(),
            b"snapshot",
            "the open fd still reads its captured content after rename"
        );

        // A write + close-flush on that fd lands on the renamed memory (same id,
        // unchanged sha, so the CAS succeeds).
        fuse.write_fh_bytes(fh, 0, b"after-rename").unwrap();
        fuse.flush_fh(fh).expect("flush after rename succeeds");
        let moved = rt
            .block_on(backend.get_by_path(store, "/new.md"))
            .unwrap()
            .unwrap();
        assert_eq!(moved.content.as_deref(), Some("after-rename"));
        assert!(
            rt.block_on(backend.get_by_path(store, "/old.md"))
                .unwrap()
                .is_none(),
            "the old path is gone"
        );
    }

    #[test]
    fn child_path_joins_valid_names_and_rejects_traversal() {
        // Path safety: a child name must be a single, non-empty segment. `.`/`..`/an
        // embedded slash are rejected so a lookup/create/rename cannot escape the store.
        assert_eq!(
            MemoryFuse::child_path("/", OsStr::new("a.md")).as_deref(),
            Some("/a.md")
        );
        assert_eq!(
            MemoryFuse::child_path("/notes", OsStr::new("a.md")).as_deref(),
            Some("/notes/a.md")
        );
        // A trailing slash on the parent does not double up.
        assert_eq!(
            MemoryFuse::child_path("/notes/", OsStr::new("a.md")).as_deref(),
            Some("/notes/a.md")
        );
        for bad in ["", "a/b", ".", ".."] {
            assert_eq!(
                MemoryFuse::child_path("/", OsStr::new(bad)),
                None,
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn write_and_read_at_offsets_splice_and_clamp_the_fd_buffer() {
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        rt.block_on(backend.create(store, "/f.md", "abcdef"))
            .unwrap();
        let fuse = MemoryFuse::new(backend, store.into()).unwrap();
        let fh = fuse.open_file_for_path("/f.md", 0).unwrap();

        // An overwrite in the middle of the buffer.
        fuse.write_fh_bytes(fh, 2, b"XY").unwrap();
        assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"abXYef");
        // A write past the end zero-fills the gap.
        fuse.write_fh_bytes(fh, 8, b"Z").unwrap();
        assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"abXYef\0\0Z");
        // A read clamps: an offset past the end yields nothing…
        assert_eq!(fuse.read_fh_bytes(fh, 100, 10).unwrap(), b"");
        // …and a size that overruns the end is truncated to what exists.
        assert_eq!(fuse.read_fh_bytes(fh, 6, 100).unwrap(), b"\0\0Z");
        fuse.remove_fh(fh);
    }

    #[test]
    fn a_flush_conflict_keeps_the_buffer_dirty_for_a_re_drive() {
        // On a CAS conflict the flush returns EAGAIN but must NOT drop the buffer or
        // mark the fd clean — the agent's write is preserved for a re-drive, and a
        // second flush conflicts again (proving the fd was never silently cleared).
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        let created = rt.block_on(backend.create(store, "/c.md", "v1")).unwrap();
        let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
        let fh = fuse.open_file_for_path("/c.md", 0).unwrap();

        // Advance the store out from under the open fd, then write + flush the stale fd.
        rt.block_on(backend.update(store, &created.id, "server", &created.content_sha256))
            .unwrap();
        fuse.write_fh_bytes(fh, 0, b"mine").unwrap();
        assert!(matches!(
            fuse.flush_fh(fh),
            Err(FuseError::Mem(MemErr::Conflict { .. }))
        ));
        // The buffer survives the conflict…
        assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"mine");
        // …and the fd is still dirty, so re-flushing conflicts again (not a clean no-op).
        assert!(matches!(
            fuse.flush_fh(fh),
            Err(FuseError::Mem(MemErr::Conflict { .. }))
        ));
        fuse.remove_fh(fh);
    }

    #[test]
    fn apply_setattr_grow_zero_fills_through_the_store() {
        // Truncation that GROWS a file (no open fd) zero-fills to the new size and
        // writes it through the store — the mirror of the shrink case.
        let store = "s";
        let backend = Arc::new(InMemoryFs::new());
        let rt = setup_rt();
        rt.block_on(backend.create(store, "/g.md", "hi")).unwrap();
        let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
        let (ino, _) = fuse.lookup_path("/g.md").unwrap();

        assert_eq!(fuse.apply_setattr(ino, Some(5), None).unwrap().size, 5);
        assert_eq!(
            rt.block_on(backend.get_by_path(store, "/g.md"))
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("hi\0\0\0"),
            "grow zero-fills to the requested length"
        );
    }

    #[test]
    fn content_lru_handles_zero_capacity_prefix_clear_and_removal() {
        // A zero-capacity cache never stores anything (defensive: a misconfigured cap
        // must not panic on eviction).
        let mut zero = ContentLruCache::new(0);
        zero.put(memory("/a.md", "a"));
        assert!(zero.get("/a.md").is_none());

        let mut cache = ContentLruCache::new(8);
        cache.put(memory("/notes/a.md", "a"));
        cache.put(memory("/notes/deep/b.md", "b"));
        cache.put(memory("/other.md", "o"));
        // `clear_path_prefix` (used by rmdir) drops only the matching subtree.
        cache.clear_path_prefix("/notes/");
        assert!(cache.get("/notes/a.md").is_none());
        assert!(cache.get("/notes/deep/b.md").is_none());
        assert!(cache.get("/other.md").is_some(), "a sibling survives");
        // `remove_path` drops one entry; `clear` drops all.
        cache.remove_path("/other.md");
        assert!(cache.get("/other.md").is_none());
        cache.put(memory("/x.md", "x"));
        cache.clear();
        assert!(cache.get("/x.md").is_none());
    }

    #[test]
    fn a_lagged_listener_clears_the_whole_cache() {
        use crate::LocalInvalidator;
        use crate::invalidate::Invalidator;
        // A listener that fell behind by more than the bus capacity has missed
        // invalidations, so it cannot know which paths are stale — it clears the
        // whole cache rather than risk serving a stale entry.
        let cache = Arc::new(Mutex::new(ContentLruCache::new(8)));
        cache.lock().unwrap().put(memory("/a.md", "a"));
        cache.lock().unwrap().put(memory("/b.md", "b"));

        // Capacity-2 bus; publish far more than that BEFORE the listener drains, so
        // its first `try_recv` observes `Lagged`.
        let bus = LocalInvalidator::new(2);
        let rx = bus.subscribe();
        for i in 0..20 {
            bus.publish("s", &format!("/p{i}.md"));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let listener = {
            let (cache, stop) = (cache.clone(), stop.clone());
            std::thread::spawn(move || run_invalidation_listener(cache, "s".into(), rx, stop))
        };

        let mut waited = Duration::ZERO;
        while cache.lock().unwrap().get("/a.md").is_some() && waited < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
            waited += Duration::from_millis(10);
        }
        assert!(
            cache.lock().unwrap().get("/a.md").is_none(),
            "a lagged listener clears the cache"
        );
        assert!(cache.lock().unwrap().get("/b.md").is_none());

        stop.store(true, Ordering::SeqCst);
        listener.join().unwrap();
    }

    #[test]
    fn a_closed_bus_stops_the_listener_without_the_stop_flag() {
        use crate::LocalInvalidator;
        // Dropping the sender closes the bus; the listener observes `Closed` and exits
        // on its own (the drop-without-explicit-unmount path relies on this).
        let cache = Arc::new(Mutex::new(ContentLruCache::new(4)));
        let bus = LocalInvalidator::new(4);
        let rx = bus.subscribe();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (cache, stop) = (cache.clone(), stop.clone());
            std::thread::spawn(move || run_invalidation_listener(cache, "s".into(), rx, stop))
        };

        drop(bus);
        let start = std::time::Instant::now();
        handle.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "a closed bus exits the listener promptly"
        );
        assert!(
            !stop.load(Ordering::SeqCst),
            "it exited via Closed, not the stop flag"
        );
    }
}
