//! Choosing between watching and polling a library path, by the filesystem it is on.

mod mounts;
mod poll;

pub use mounts::{fs_type_from_mountinfo, fs_type_of, is_local_fs_type};
pub use poll::FolderPoller;
