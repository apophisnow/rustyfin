//! Watching a library path with the kernel's change notifications (inotify, Linux). Every folder
//! under the path has a watch. Events are batched by unit and reported to the change queue. A path
//! that cannot be watched is handed to the poller instead.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{Inner, MonitorKey};
use crate::library_monitor::unit_for;
use crate::naming::is_ignored_dir;
use crate::scanner::ScanScope;

/// The events a folder's watch reports. Plain writes, reads and attribute changes raise nothing.
const WATCH_MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF)
    .union(WatchMask::ONLYDIR);

/// How long a batch collects events before it is reported.
const BATCH_DELAY: Duration = Duration::from_millis(500);

/// Bytes read from the kernel at a time. An event takes a few hundred bytes at most.
const READ_BUFFER: usize = 16 * 1024;

/// A folder's identity: device and inode. Two paths with one identity are one folder.
type Id = (u64, u64);

/// A watched folder.
struct Folder {
    wd: WatchDescriptor,
    id: Id,
}

/// The folders under a library path, each with its watch. The monitor moves it into the blocking
/// threads that walk folders, and takes it back.
struct Tree {
    root: PathBuf,
    watches: Watches,
    /// The watched folders by path.
    folders: BTreeMap<PathBuf, Folder>,
    /// The folder of each watch.
    paths: HashMap<WatchDescriptor, PathBuf>,
    /// The identities of the watched folders and of the data dir. A folder with one of them is
    /// not watched again.
    ids: HashSet<Id>,
}

/// What a read of the kernel's queue gives: an event copied out of the read buffer, or the
/// overflow of the queue. A `fake_overflow` gives one too.
enum Seen {
    Overflow,
    Event {
        wd: WatchDescriptor,
        mask: EventMask,
        name: Option<OsString>,
    },
}

/// Watches the library path of the monitor `key` until the monitor has to poll it instead, and
/// returns the reason, which the poller logs. Returns None when the monitor is no longer listed,
/// or when its task failed, which is logged. The next reload then replaces the monitor.
pub(super) async fn watch(
    inner: &Inner,
    key: &MonitorKey,
    library_name: &str,
    fs_type: &str,
    id: u64,
    overflow: &Notify,
) -> Option<String> {
    let root = key.1.clone();
    let setup_root = root.clone();
    let data_dir = inner.data_dir.clone();
    let setup = tokio::task::spawn_blocking(move || setup(&setup_root, &data_dir)).await;
    let (inotify, mut tree, took) = match setup {
        Ok(Ok(setup)) => setup,
        Ok(Err(err)) => return Some(format!("kernel watching failed: {err}")),
        Err(err) => {
            tracing::warn!(
                "library monitor: could not set up '{}': {err}; restarting it at the next library change",
                root.display()
            );
            return None;
        }
    };
    let folders = tree.folders.len();
    inner.with_monitor(key, id, |monitor| {
        monitor.info.folders = folders;
        monitor.info.watches = folders;
        monitor.info.ready = true;
    })?;
    tracing::info!(
        "library monitor: watching '{}' of '{library_name}' ({fs_type}): {folders} folders, {folders} watches, ready in {} ms",
        root.display(),
        took.as_millis()
    );

    // SAFETY: `inotify` moves into the `AsyncFd`, which owns it for its whole life. Nothing else
    // closes or replaces its file descriptor.
    let mut fd = match unsafe { AsyncFd::register(inotify) } {
        Ok(fd) => fd,
        Err(err) => {
            let (_, cause) = err.into_parts();
            return Some(format!("kernel watching failed: {cause}"));
        }
    };
    let library_paths = [root.to_string_lossy().into_owned()];
    let mut batch: BTreeMap<ScanScope, PathBuf> = BTreeMap::new();
    // When the batch is reported. Set only while the batch is not empty.
    let mut due: Option<Instant> = None;
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        let flush = async move {
            match due {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        let seen = tokio::select! {
            read = read_ready(&mut fd, &mut buf) => match read {
                Ok(seen) => seen,
                Err(err) => {
                    report(inner, &mut batch).await;
                    return Some(format!("kernel watching failed: {err}"));
                }
            },
            () = overflow.notified() => vec![Seen::Overflow],
            () = flush => {
                report(inner, &mut batch).await;
                due = None;
                Vec::new()
            }
        };
        if seen.is_empty() {
            continue;
        }
        if seen.iter().any(|event| matches!(event, Seen::Overflow)) {
            tracing::warn!(
                "library monitor: the event queue of '{}' overflowed; rescanning '{library_name}'",
                root.display()
            );
        }
        let (returned, reported) = match apply_on_thread(tree, seen).await {
            Ok(done) => done,
            Err(err) => {
                inner.with_monitor(key, id, |monitor| monitor.info.ready = false);
                tracing::warn!(
                    "library monitor: watching '{}' failed: {err}; restarting it at the next library change",
                    root.display()
                );
                return None;
            }
        };
        tree = returned;
        for path in reported {
            let Some(unit) = unit_for(&library_paths, &path) else {
                continue;
            };
            if batch.is_empty() {
                due = Some(Instant::now() + BATCH_DELAY);
            }
            batch.entry(unit).or_insert(path);
        }
        let folders = tree.folders.len();
        inner.with_monitor(key, id, |monitor| {
            monitor.info.folders = folders;
            monitor.info.watches = folders;
        })?;
    }
}

/// Starts an inotify instance and watches the library path, with the folders under it. Runs on a
/// blocking thread. Returns the instance, the tree and how long the setup took.
fn setup(root: &Path, data_dir: &Path) -> io::Result<(Inotify, Tree, Duration)> {
    let started = std::time::Instant::now();
    let inotify = Inotify::init()?;
    let meta = fs::metadata(root)?;
    let mut tree = Tree {
        root: root.to_path_buf(),
        watches: inotify.watches(),
        folders: BTreeMap::new(),
        paths: HashMap::new(),
        // The data dir is left out of every walk, from the start.
        ids: fs::metadata(data_dir)
            .ok()
            .map(|meta| (meta.dev(), meta.ino()))
            .into_iter()
            .collect(),
    };
    tree.watch_folder(root, (meta.dev(), meta.ino()))?;
    Ok((inotify, tree, started.elapsed()))
}

/// Waits until the kernel has events, then reads all of them. Copies them out of the buffer.
async fn read_ready(fd: &mut AsyncFd<Inotify>, buf: &mut [u8]) -> io::Result<Vec<Seen>> {
    let mut ready = fd.readable_mut().await?;
    let mut seen = Vec::new();
    loop {
        match ready.get_inner_mut().read_events(buf) {
            Ok(events) => {
                for event in events {
                    seen.push(if event.mask.contains(EventMask::Q_OVERFLOW) {
                        Seen::Overflow
                    } else {
                        Seen::Event {
                            wd: event.wd,
                            mask: event.mask,
                            name: event.name.map(OsStr::to_os_string),
                        }
                    });
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                ready.clear_ready();
                return Ok(seen);
            }
            Err(err) => return Err(err),
        }
    }
}

/// Applies the events to the tree on a blocking thread, since a walk of a new folder reads the
/// disk. Returns the tree and the paths to report.
async fn apply_on_thread(
    mut tree: Tree,
    seen: Vec<Seen>,
) -> Result<(Tree, Vec<PathBuf>), tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        let mut reported = Vec::new();
        for event in seen {
            tree.apply(event, &mut reported);
        }
        (tree, reported)
    })
    .await
}

/// Reports the paths of the batch, and empties it.
async fn report(inner: &Inner, batch: &mut BTreeMap<ScanScope, PathBuf>) {
    let paths: Vec<PathBuf> = std::mem::take(batch).into_values().collect();
    inner.changes.report_all(&paths).await;
}

impl Tree {
    /// Applies one event, or the overflow of the queue, and collects the paths to report.
    fn apply(&mut self, seen: Seen, reported: &mut Vec<PathBuf>) {
        match seen {
            Seen::Overflow => {
                // Events were lost, so the whole library is rescanned. Folders that appeared
                // meanwhile get their watches.
                reported.push(self.root.clone());
                self.overflow_walk();
            }
            Seen::Event { wd, mask, name } => self.event(&wd, mask, name, reported),
        }
    }

    /// Applies one event of a folder's watch to the tree, and collects the path it reports.
    fn event(
        &mut self,
        wd: &WatchDescriptor,
        mask: EventMask,
        name: Option<OsString>,
        reported: &mut Vec<PathBuf>,
    ) {
        // An event of a watch that is no longer in the tree has nothing to say.
        let Some(dir) = self.paths.get(wd).cloned() else {
            return;
        };
        let Some(name) = name else {
            // Without a name, the event is about the folder itself. Only the kernel dropping its
            // watch changes the tree. The library path's own watch is left alone.
            if mask.contains(EventMask::IGNORED) && dir != self.root {
                self.forget(&dir, false);
            }
            return;
        };
        if name.to_str().is_some_and(is_ignored_dir) {
            return;
        }
        let path = dir.join(&name);
        // A symlink to a folder is created without ISDIR, so its target is checked.
        let creates_folder = if mask.contains(EventMask::ISDIR) {
            mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
        } else {
            mask.contains(EventMask::CREATE) && folder_id(&path).is_some()
        };
        if creates_folder {
            // Reported only when it is watched now: not when it is gone, the data dir, or watched
            // under another name already.
            if self.watch_new(&path) {
                reported.push(path);
            }
            return;
        }
        if mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
            // The name is gone, so a folder watched under it, and what is under it, are
            // forgotten. A removed symlink to a folder raises no IGNORED of its own.
            self.forget_subtree(&path);
        }
        reported.push(path);
    }

    /// Watches the folder at `dir`, of identity `id`, then the folders under it. The watch comes
    /// before the listing, so a subfolder made meanwhile is listed or raises an event. Errors when
    /// the watch on `dir` itself fails.
    fn watch_folder(&mut self, dir: &Path, id: Id) -> io::Result<()> {
        let wd = self.watches.add(dir, WATCH_MASK)?;
        self.record(dir, wd, id);
        self.walk_children(dir);
        Ok(())
    }

    /// Records the watch `wd` of the folder at `dir`. An entry that `dir` had before (the folder
    /// was replaced, and the events of its removal were lost) is forgotten.
    fn record(&mut self, dir: &Path, wd: WatchDescriptor, id: Id) {
        if let Some(stale) = self
            .folders
            .insert(dir.to_path_buf(), Folder { wd: wd.clone(), id })
        {
            self.paths.remove(&stale.wd);
            self.ids.remove(&stale.id);
            if stale.wd != wd {
                let _ = self.watches.remove(stale.wd);
            }
        }
        self.paths.insert(wd, dir.to_path_buf());
        self.ids.insert(id);
    }

    /// Watches the new folders under `dir`, in name order.
    fn walk_children(&mut self, dir: &Path) {
        for name in subfolders(dir) {
            self.watch_new(&dir.join(name));
        }
    }

    /// Watches `dir`, a folder that appeared, and the folders under it. Returns whether `dir` is
    /// watched now. It is not when it is gone, when it is the data dir or watched under another
    /// name, or when its watch fails.
    fn watch_new(&mut self, dir: &Path) -> bool {
        let Some(id) = folder_id(dir) else {
            tracing::debug!(
                "library monitor: '{}' is gone or not a folder; not watched",
                dir.display()
            );
            return false;
        };
        if self.ids.contains(&id) {
            return false;
        }
        match self.watch_folder(dir, id) {
            Ok(()) => true,
            Err(err) => {
                tracing::debug!("library monitor: not watching '{}': {err}", dir.display());
                false
            }
        }
    }

    /// Forgets the folder at `dir`, and removes its watch when `unwatch` is set. The kernel has
    /// dropped the watch already on IGNORED, and removing it again fails, which is fine.
    fn forget(&mut self, dir: &Path, unwatch: bool) {
        let Some(folder) = self.folders.remove(dir) else {
            return;
        };
        self.paths.remove(&folder.wd);
        self.ids.remove(&folder.id);
        if unwatch {
            let _ = self.watches.remove(folder.wd);
        }
    }

    /// Forgets the folder at `dir` and the folders under it, with their watches.
    fn forget_subtree(&mut self, dir: &Path) {
        let under: Vec<PathBuf> = self
            .folders
            .range(dir.to_path_buf()..)
            .take_while(|(path, _)| path.starts_with(dir))
            .map(|(path, _)| path.clone())
            .collect();
        for path in under {
            self.forget(&path, true);
        }
    }

    /// The walk after an overflow. Folders that are gone, or have another identity, are forgotten
    /// first, since their events may have been lost. The walk then goes down through the watched
    /// folders under the library path and watches the new ones, adding no second watch.
    fn overflow_walk(&mut self) {
        let known: Vec<(PathBuf, Id)> = self
            .folders
            .iter()
            .map(|(path, folder)| (path.clone(), folder.id))
            .collect();
        for (path, id) in known {
            // Dropped already with an ancestor, or the library path, which is kept.
            if path == self.root || !self.folders.contains_key(&path) {
                continue;
            }
            if folder_id(&path) != Some(id) {
                self.forget_subtree(&path);
            }
        }
        let root = self.root.clone();
        self.overflow_dir(&root);
    }

    /// Goes down through the watched folders under `dir`, and watches the new ones.
    fn overflow_dir(&mut self, dir: &Path) {
        for name in subfolders(dir) {
            let child = dir.join(name);
            let Some(id) = folder_id(&child) else {
                continue;
            };
            if self
                .folders
                .get(&child)
                .is_some_and(|folder| folder.id == id)
            {
                self.overflow_dir(&child);
            } else {
                // New, or watched under another name, or the data dir: `watch_new` tells.
                self.watch_new(&child);
            }
        }
    }
}

/// The identity of the folder at `path`, following a symlink. None when there is no folder there.
fn folder_id(path: &Path) -> Option<Id> {
    fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_dir())
        .map(|meta| (meta.dev(), meta.ino()))
}

/// The names of the folders in `dir` that the scanner does not skip, in name order. A symlink is
/// followed to see whether it is a folder.
fn subfolders(dir: &Path) -> BTreeSet<OsString> {
    let Ok(entries) = fs::read_dir(dir) else {
        tracing::debug!("library monitor: '{}' cannot be listed", dir.display());
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            let is_dir = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => {
                    fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir())
                }
                Ok(kind) => kind.is_dir(),
                Err(_) => false,
            };
            is_dir && !entry.file_name().to_str().is_some_and(is_ignored_dir)
        })
        .map(|entry| entry.file_name())
        .collect()
}
