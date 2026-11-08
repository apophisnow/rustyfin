//! Follows the mount table, so a library path whose mounts change is set up again. inotify doesn't
//! see into a filesystem mounted over a watched folder, and a path's method is picked only once,
//! when its monitor starts.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::task::JoinHandle;

use super::Inner;
use super::mounts::{MountChange, mount_changes, restarts_monitor};

/// The mount table of this process. The kernel wakes a poll of it once per change.
const MOUNT_TABLE: &str = "/proc/self/mountinfo";

/// Follows the mount table in a task, which marks the monitors that a mount change restarts. The
/// table is read before this returns, so a monitor set up after it sees a table at least as new.
/// None, after a warning, when the table cannot be opened, registered or read.
pub(super) fn follow(inner: &Arc<Inner>) -> Option<JoinHandle<()>> {
    let registered = File::open(MOUNT_TABLE).and_then(|file| {
        // SAFETY: `file` moves into the `AsyncFd`, which owns it for its whole life. Nothing else
        // closes or replaces its file descriptor.
        unsafe { AsyncFd::register_with_interest(file, Interest::PRIORITY) }
            .map_err(io::Error::from)
    });
    let fd = match registered {
        Ok(fd) => fd,
        Err(err) => {
            warn_cannot_follow(&err);
            return None;
        }
    };
    let table = match read_table(fd.get_ref()) {
        Ok(table) => table,
        Err(err) => {
            warn_cannot_follow(&err);
            return None;
        }
    };
    Some(tokio::spawn(follow_table(Arc::clone(inner), fd, table)))
}

/// Waits for the kernel to wake the poll of the table, then reads the table and marks the monitors
/// that its changes restart. Ends, after a warning, when a wait or a read fails.
async fn follow_table(inner: Arc<Inner>, fd: AsyncFd<File>, mut table: String) {
    loop {
        let mut ready = match fd.ready(Interest::PRIORITY).await {
            Ok(ready) => ready,
            Err(err) => {
                warn_cannot_follow(&err);
                return;
            }
        };
        ready.clear_ready();
        let new = match read_table(ready.get_inner()) {
            Ok(new) => new,
            Err(err) => {
                warn_cannot_follow(&err);
                return;
            }
        };
        let changes = mount_changes(&table, &new);
        table = new;
        if !changes.is_empty() {
            mark_restarts(&inner, &changes);
        }
    }
}

/// Marks each monitor that a change restarts, by the first such change in `changes`, then wakes
/// the manager once when any monitor was marked. Each marked monitor is logged once.
fn mark_restarts(inner: &Inner, changes: &[(PathBuf, MountChange)]) {
    let any = {
        let mut state = inner.lock();
        let mut any = false;
        for monitor in state.monitors.values_mut() {
            let path = monitor
                .canonical
                .as_deref()
                .unwrap_or(monitor.info.path.as_path());
            let Some((point, change)) = changes
                .iter()
                .find(|(point, change)| restarts_monitor(path, point, *change))
            else {
                continue;
            };
            monitor.restart = true;
            any = true;
            let verb = match change {
                MountChange::Mounted => "mounted",
                MountChange::Unmounted => "unmounted",
            };
            tracing::info!(
                "library monitor: '{}' was {verb}; setting up '{}' of '{}' again",
                point.display(),
                monitor.info.path.display(),
                monitor.info.library_name
            );
        }
        any
    };
    if any {
        inner.reload.notify_one();
    }
}

/// Reads the whole mount table through `file`, from its start, so each read is a new reading.
fn read_table(mut file: &File) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut table = String::new();
    file.read_to_string(&mut table)?;
    Ok(table)
}

/// Logs that the mount table cannot be followed.
fn warn_cannot_follow(err: &io::Error) {
    tracing::warn!(
        "library monitor: cannot follow the mount table ({err}); a share mounted under a library path from now on is seen after a restart"
    );
}
