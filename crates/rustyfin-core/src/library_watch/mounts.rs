//! Filesystem type of a path, from the mount table, to choose between watching and polling.

use std::path::{Path, PathBuf};

/// Types whose changes reach the kernel, so a folder on them can be watched.
const LOCAL_FS_TYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "jfs", "reiserfs", "bcachefs", "nilfs2",
    "overlay", "tmpfs", "ramfs", "vfat", "msdos", "exfat", "ntfs", "ntfs3", "fuseblk", "hfs",
    "hfsplus", "apfs", "udf", "iso9660", "squashfs", "erofs",
];

/// Filesystem type of the mount that holds `path`, read from mountinfo text.
/// The mount point with the most components wins; a later line wins a tie.
pub fn fs_type_from_mountinfo(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        // Optional fields come after field 5 and end with a lone "-"; the type follows it.
        let Some(dash) = fields
            .iter()
            .skip(6)
            .position(|field| *field == "-")
            .map(|index| index + 6)
        else {
            continue;
        };
        let (Some(mount), Some(fs_type)) = (fields.get(4), fields.get(dash + 1)) else {
            continue;
        };
        let mount = PathBuf::from(unescape(mount));
        if !path.starts_with(&mount) {
            continue;
        }
        let depth = mount.components().count();
        if best
            .as_ref()
            .is_none_or(|(best_depth, _)| depth >= *best_depth)
        {
            best = Some((depth, fs_type.to_string()));
        }
    }
    best.map(|(_, fs_type)| fs_type)
}

/// Whether a filesystem of this type reports its changes, so the folder can be watched.
pub fn is_local_fs_type(fs_type: &str) -> bool {
    LOCAL_FS_TYPES.contains(&fs_type)
}

/// Filesystem type of the mount that holds `path`, from `/proc/self/mountinfo`.
#[cfg(target_os = "linux")]
pub fn fs_type_of(path: &Path) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    fs_type_from_mountinfo(&mountinfo, path)
}

/// Filesystem type of the mount that holds `path`, from `statfs`.
#[cfg(target_os = "macos")]
pub fn fs_type_of(path: &Path) -> Option<String> {
    use std::ffi::{CStr, CString};
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c_path` is NUL-terminated and `buf` is a valid out-pointer.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statfs returned 0, so it has initialised `buf`.
    let buf = unsafe { buf.assume_init() };
    // SAFETY: statfs wrote a NUL-terminated name into `f_fstypename`.
    let name = unsafe { CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    name.to_str().ok().map(str::to_owned)
}

/// Filesystem types are not read on this OS.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn fs_type_of(_path: &Path) -> Option<String> {
    None
}

/// Undoes the octal escapes the kernel writes for space, tab, newline and backslash.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match octal_escape(&bytes[i..]) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte that a `\ooo` escape at the start of `bytes` stands for.
fn octal_escape(bytes: &[u8]) -> Option<u8> {
    let [b'\\', a, b, c, ..] = bytes else {
        return None;
    };
    let digits = [*a, *b, *c];
    if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
        return None;
    }
    let value = digits
        .iter()
        .fold(0u16, |acc, d| acc * 8 + u16::from(d - b'0'));
    u8::try_from(value).ok()
}
