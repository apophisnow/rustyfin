// B18 part 2: watching library folders for changes. These tests cover choosing the method (the
// filesystem type of a library path) and polling folder timestamps (`FolderPoller`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustyfin_core::library_watch::{
    FolderPoller, fs_type_from_mountinfo, fs_type_of, is_local_fs_type,
};
use uuid::Uuid;

// ── Helpers ──

/// A temporary folder holding the library path `root`, removed when dropped.
struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("rustyfin_test_watch_{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("root")).unwrap();
        Self(dir)
    }

    /// The library path.
    fn root(&self) -> PathBuf {
        self.0.join("root")
    }

    /// `rel` inside the library path.
    fn at(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    /// `rel` next to the library path, outside the library.
    fn outside(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Creates the file `path` with `len` bytes, and the folders it is in.
fn write(path: &Path, len: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![b'x'; len]).unwrap();
}

/// Appends `len` bytes to the file `path`, as a copy in progress does.
fn append(path: &Path, len: usize) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&vec![b'x'; len]).unwrap();
}

/// Waits until a change gets a later modified time than the change before, also where the
/// kernel's clock for timestamps is coarse.
fn tick() {
    std::thread::sleep(Duration::from_millis(60));
}

fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths
}

/// Everything any test in this binary logs from now on, at every level.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = buf.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || LogWriter(sink.clone()))
            .try_init()
            .expect("no other global subscriber in this test binary");
        buf
    })
    .clone()
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The captured log lines that contain every one of `needles`.
fn log_lines(needles: &[&str]) -> Vec<String> {
    let logs = captured_logs();
    let text = String::from_utf8_lossy(&logs.lock().unwrap()).to_string();
    text.lines()
        .filter(|line| needles.iter().all(|needle| line.contains(needle)))
        .map(str::to_string)
        .collect()
}

// ── Filesystem types ──

/// A /proc/self/mountinfo with zero, one and two optional fields, escaped mount points, two mounts
/// on one mount point and a line that doesn't parse.
const MOUNTINFO: &str = r"22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/sda2 rw,ssd
25 22 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
30 22 0:25 / /tmp rw,nosuid,nodev shared:5 master:2 - tmpfs tmpfs rw,size=8g
40 22 0:30 /media /mnt/media ro,relatime master:3 - zfs tank/media ro,xattr
41 40 0:31 / /mnt/media/Film rw - tmpfs tmpfs rw
50 22 0:40 / /mnt/nas/Movies rw,relatime shared:9 - nfs4 nas:/export/movies rw,vers=4.2
51 22 8:17 / /mnt/nas/local\040disk rw,relatime - ext4 /dev/sdb1 rw
60 22 8:33 / /srv rw - xfs /dev/sdc1 rw
61 60 8:49 / /srv rw - ext4 /dev/sdd1 rw
70 22 0:50 / /mnt/tab\011name rw - vfat /dev/sde1 rw
71 22 0:51 / /mnt/back\134slash rw - cifs //nas/share rw
72 22 0:52 / /mnt/rclone rw,nosuid,nodev - fuse.rclone remote: rw
this line does not parse
73 22 0:53 / /mnt/sshfs rw - fuse.sshfs user@host:/ rw
";

fn fs_of(path: &str) -> Option<String> {
    fs_type_from_mountinfo(MOUNTINFO, Path::new(path))
}

#[test]
fn mountinfo_gives_the_type_of_the_longest_mount_point_that_holds_the_path() {
    assert_eq!(
        fs_of("/mnt/media/Film/Heat (1995)").as_deref(),
        Some("tmpfs")
    );
    assert_eq!(fs_of("/mnt/media/Film").as_deref(), Some("tmpfs"));
    assert_eq!(
        fs_of("/mnt/media/Filmography").as_deref(),
        Some("zfs"),
        "mount points hold paths by whole components"
    );
    assert_eq!(fs_of("/mnt/media").as_deref(), Some("zfs"));
    assert_eq!(
        fs_of("/mnt/nas/Movies/Heat (1995)/Heat (1995).mkv").as_deref(),
        Some("nfs4")
    );
    assert_eq!(fs_of("/mnt/nas/Moviesque").as_deref(), Some("btrfs"));
    assert_eq!(fs_of("/home/me/Music").as_deref(), Some("btrfs"));
    assert_eq!(fs_of("/proc/self").as_deref(), Some("proc"));
    assert_eq!(
        fs_of("/mnt/sshfs/films").as_deref(),
        Some("fuse.sshfs"),
        "a line that doesn't parse is skipped"
    );
}

#[test]
fn mountinfo_mount_points_are_unescaped_and_the_later_of_two_mounts_wins() {
    assert_eq!(fs_of("/mnt/nas/local disk/Movies").as_deref(), Some("ext4"));
    assert_eq!(
        fs_of(r"/mnt/nas/local\040disk/Movies").as_deref(),
        Some("btrfs"),
        "the escape is not part of the name"
    );
    assert_eq!(fs_of("/mnt/tab\tname/Movies").as_deref(), Some("vfat"));
    assert_eq!(fs_of(r"/mnt/back\slash/Movies").as_deref(), Some("cifs"));
    assert_eq!(fs_of("/mnt/rclone/Movies").as_deref(), Some("fuse.rclone"));
    assert_eq!(
        fs_of("/srv/media").as_deref(),
        Some("ext4"),
        "mounted over the xfs one"
    );
    assert_eq!(fs_type_from_mountinfo("", Path::new("/mnt/media")), None);
    assert_eq!(
        fs_type_from_mountinfo("not mountinfo\n", Path::new("/")),
        None
    );
}

#[test]
fn local_filesystems_are_watched_and_all_others_polled() {
    for local in [
        "ext4", "ext3", "xfs", "btrfs", "zfs", "f2fs", "bcachefs", "overlay", "tmpfs", "vfat",
        "exfat", "ntfs", "ntfs3", "fuseblk", "apfs", "hfsplus", "squashfs", "iso9660",
    ] {
        assert!(is_local_fs_type(local), "{local} is local");
    }
    for polled in [
        "nfs",
        "nfs4",
        "cifs",
        "smb3",
        "smbfs",
        "9p",
        "ceph",
        "glusterfs",
        "afs",
        "davfs",
        "fuse",
        "fuse.sshfs",
        "fuse.rclone",
        "fuse.mergerfs",
        "proc",
        "",
        "somethingnew",
    ] {
        assert!(!is_local_fs_type(polled), "{polled} is polled");
    }
}

#[test]
fn fs_type_of_reads_the_mounts_of_this_machine() {
    let temp = std::env::temp_dir();
    if cfg!(target_os = "linux") {
        assert_eq!(fs_type_of(Path::new("/proc/self")).as_deref(), Some("proc"));
    }
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        let fs = fs_type_of(&temp).expect("the temp dir is on a mount");
        assert!(is_local_fs_type(&fs), "the temp dir is on {fs}");
    } else {
        assert_eq!(fs_type_of(&temp), None);
    }
}

// ── Polling ──

#[test]
fn the_first_poll_only_records_the_baseline() {
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    write(&tree.at("Show/Season 1/S01E01.mkv"), 10);
    write(&tree.at("Loose (2001).mkv"), 10);
    std::fs::create_dir_all(tree.at(".hidden/inner")).unwrap();
    std::fs::create_dir_all(tree.at("@eaDir/Heat (1995)")).unwrap();
    std::fs::create_dir_all(tree.at("Heat (1995)/Heat (1995).trickplay")).unwrap();
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(!poller.has_baseline());

    assert_eq!(poller.poll(), Vec::<PathBuf>::new());
    assert!(poller.has_baseline());
    assert!(poller.available());
    // The library path, Heat (1995), Show and Season 1; ignored folders are left out.
    assert_eq!(poller.folders(), 4);
    assert_eq!(poller.settling(), 0);

    tick();
    assert_eq!(poller.poll(), Vec::<PathBuf>::new(), "nothing changed");
}

#[test]
fn a_poll_reports_new_changed_and_removed_folders() {
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    write(&tree.at("Alien (1979)/Alien (1979).mkv"), 10);
    write(&tree.at("Show/Season 1/S01E01.mkv"), 10);
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());

    tick();
    write(&tree.at("Heat (1995)/Heat (1995).srt"), 10);
    std::fs::remove_dir_all(tree.at("Alien (1979)")).unwrap();
    write(&tree.at("Up (2009)/Up (2009).mkv"), 10);
    write(&tree.at("Up (2009)/extras/Making of.mkv"), 10);
    write(&tree.at("Show/Season 1/S01E02.mkv"), 10);
    assert_eq!(
        poller.poll(),
        sorted(vec![
            tree.at("Alien (1979)"),
            tree.at("Heat (1995)"),
            tree.at("Show/Season 1"),
            tree.at("Up (2009)"),
        ]),
        "a new folder is reported without the folders inside it"
    );
    // The library path, Heat (1995), Show, Season 1, Up (2009) and its extras.
    assert_eq!(poller.folders(), 6);
    assert!(poller.settling() > 0, "changed folders settle");

    tick();
    write(&tree.at("Up (2009)/extras/Trailer.mkv"), 10);
    assert_eq!(
        poller.poll(),
        vec![tree.at("Up (2009)/extras")],
        "the folders inside a new folder are polled"
    );

    tick();
    assert_eq!(poller.poll(), Vec::<PathBuf>::new());
    assert_eq!(poller.settling(), 0, "every folder held still");
}

#[test]
fn a_growing_file_is_reported_again_once_it_holds_still() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.at("Heat (1995)")).unwrap();
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());

    tick();
    let film = tree.at("Heat (1995)/Heat (1995).mkv");
    write(&film, 1000);
    assert_eq!(poller.poll(), vec![tree.at("Heat (1995)")], "the new file");
    for _ in 0..2 {
        tick();
        append(&film, 1000);
        assert_eq!(poller.poll(), Vec::<PathBuf>::new(), "still growing");
        assert_eq!(poller.settling(), 1);
    }

    tick();
    assert_eq!(
        poller.poll(),
        vec![tree.at("Heat (1995)")],
        "held still since the last poll"
    );
    assert_eq!(poller.settling(), 0);
    tick();
    assert_eq!(poller.poll(), Vec::<PathBuf>::new());
}

#[test]
fn a_copy_that_ended_before_the_next_poll_is_reported_once() {
    let tree = Tree::new();
    std::fs::create_dir_all(tree.at("Heat (1995)")).unwrap();
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());

    tick();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 1000);
    assert_eq!(poller.poll(), vec![tree.at("Heat (1995)")]);
    tick();
    assert_eq!(
        poller.poll(),
        Vec::<PathBuf>::new(),
        "unchanged since it was reported"
    );
    assert_eq!(poller.settling(), 0);
}

#[test]
fn files_in_the_library_path_are_reported_by_their_own_paths() {
    let tree = Tree::new();
    write(&tree.at("Heat (1995).mkv"), 10);
    write(&tree.at("Alien (1979).mkv"), 10);
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());

    tick();
    std::fs::remove_file(tree.at("Alien (1979).mkv")).unwrap();
    let up = tree.at("Up (2009).mkv");
    write(&up, 1000);
    assert_eq!(
        poller.poll(),
        sorted(vec![tree.at("Alien (1979).mkv"), up.clone()]),
        "never the library path itself"
    );
    tick();
    append(&up, 1000);
    assert_eq!(poller.poll(), Vec::<PathBuf>::new(), "still growing");
    tick();
    assert_eq!(
        poller.poll(),
        vec![up],
        "held still: only the file that changed"
    );
    tick();
    assert_eq!(poller.poll(), Vec::<PathBuf>::new());
}

#[test]
fn a_renamed_or_moved_folder_is_reported_under_both_names_and_polled_under_the_new_one() {
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    write(&tree.at("Show/Season 1/S01E01.mkv"), 10);
    write(&tree.at("Show/Season 2/S02E01.mkv"), 10);
    write(&tree.at("Other/S01E01.nfo"), 10);
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());
    assert_eq!(poller.folders(), 6);

    tick();
    std::fs::rename(tree.at("Heat (1995)"), tree.at("Heat (1995) [1080p]")).unwrap();
    std::fs::rename(tree.at("Show/Season 1"), tree.at("Show/Season 01")).unwrap();
    // "Other" sorts before "Show": the move is seen from both sides in one poll.
    std::fs::rename(tree.at("Show/Season 2"), tree.at("Other/Season 2")).unwrap();
    assert_eq!(
        poller.poll(),
        sorted(vec![
            tree.at("Heat (1995)"),
            tree.at("Heat (1995) [1080p]"),
            tree.at("Other"),
            tree.at("Other/Season 2"),
            tree.at("Show"),
            tree.at("Show/Season 01"),
            tree.at("Show/Season 1"),
            tree.at("Show/Season 2"),
        ])
    );
    assert_eq!(poller.folders(), 6);

    tick();
    write(&tree.at("Show/Season 01/S01E02.mkv"), 10);
    write(&tree.at("Other/Season 2/S02E02.mkv"), 10);
    write(&tree.at("Heat (1995) [1080p]/Heat (1995).srt"), 10);
    assert_eq!(
        poller.poll(),
        sorted(vec![
            tree.at("Heat (1995) [1080p]"),
            tree.at("Other/Season 2"),
            tree.at("Show/Season 01"),
        ]),
        "polled under the new names"
    );
}

#[test]
fn a_missing_library_path_keeps_its_baseline_until_it_is_back() {
    captured_logs();
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    write(&tree.at("Alien (1979)/Alien (1979).mkv"), 10);
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());
    let root = tree.root().display().to_string();
    let missing = format!("library monitor: '{root}' is missing or unreadable");
    let back = format!("library monitor: '{root}' is back");

    let away = tree.outside("away");
    std::fs::rename(tree.root(), &away).unwrap();
    for _ in 0..3 {
        tick();
        assert_eq!(poller.poll(), Vec::<PathBuf>::new());
        assert!(!poller.available());
        assert!(poller.has_baseline());
    }
    let warnings = log_lines(&[&missing]);
    assert_eq!(warnings.len(), 1, "one warning per outage: {warnings:?}");
    assert!(warnings[0].contains("WARN"), "{warnings:?}");

    // Changed while it was away.
    std::fs::remove_dir_all(away.join("Alien (1979)")).unwrap();
    write(&away.join("Up (2009)/Up (2009).mkv"), 10);
    std::fs::rename(&away, tree.root()).unwrap();
    tick();
    assert_eq!(
        poller.poll(),
        sorted(vec![tree.at("Alien (1979)"), tree.at("Up (2009)")])
    );
    assert!(poller.available());
    let lines = log_lines(&[&back]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("INFO"), "{lines:?}");

    std::fs::rename(tree.root(), &away).unwrap();
    tick();
    assert_eq!(poller.poll(), Vec::<PathBuf>::new());
    assert_eq!(log_lines(&[&missing]).len(), 2, "a new outage warns again");
}

#[test]
fn an_emptied_library_path_counts_as_unavailable() {
    captured_logs();
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    write(&tree.at("Alien (1979)/Alien (1979).mkv"), 10);
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());
    let root = tree.root().display().to_string();

    // An unmounted share leaves its mount point empty.
    let away = tree.outside("away");
    std::fs::create_dir(&away).unwrap();
    for name in ["Heat (1995)", "Alien (1979)"] {
        std::fs::rename(tree.at(name), away.join(name)).unwrap();
    }
    tick();
    assert_eq!(poller.poll(), Vec::<PathBuf>::new());
    assert!(!poller.available());
    let warnings = log_lines(&[&format!("'{root}' is missing or unreadable")]);
    assert_eq!(warnings.len(), 1, "{warnings:?}");

    // Back with other folders: the kept baseline tells what changed.
    write(&tree.at("Up (2009)/Up (2009).mkv"), 10);
    tick();
    assert_eq!(
        poller.poll(),
        sorted(vec![
            tree.at("Alien (1979)"),
            tree.at("Heat (1995)"),
            tree.at("Up (2009)"),
        ])
    );
    assert!(poller.available());
}

#[test]
fn a_library_path_missing_at_the_first_poll_gets_its_baseline_when_it_appears() {
    captured_logs();
    let tree = Tree::new();
    let root = tree.outside("later");
    let mut poller = FolderPoller::new(root.clone(), None);
    for _ in 0..2 {
        assert_eq!(poller.poll(), Vec::<PathBuf>::new());
        assert!(!poller.has_baseline());
        assert!(!poller.available());
    }
    let shown = root.display().to_string();
    let warnings = log_lines(&[&format!("'{shown}' is missing or unreadable")]);
    assert_eq!(warnings.len(), 1, "{warnings:?}");

    write(&root.join("Heat (1995)/Heat (1995).mkv"), 10);
    assert_eq!(
        poller.poll(),
        Vec::<PathBuf>::new(),
        "the first poll that works records the baseline"
    );
    assert!(poller.has_baseline());
    assert!(poller.available());
    assert_eq!(poller.folders(), 2);

    tick();
    write(&root.join("Up (2009)/Up (2009).mkv"), 10);
    assert_eq!(poller.poll(), vec![root.join("Up (2009)")]);
}

#[test]
fn ignored_folders_and_the_data_dir_are_not_polled() {
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    std::fs::create_dir_all(tree.at(".hidden")).unwrap();
    std::fs::create_dir_all(tree.at("@eaDir/Heat (1995)")).unwrap();
    std::fs::create_dir_all(tree.at("data/images")).unwrap();
    let mut poller = FolderPoller::new(tree.root(), Some(tree.at("data")));
    assert!(poller.poll().is_empty());
    assert_eq!(poller.folders(), 2, "the library path and Heat (1995)");

    tick();
    write(&tree.at(".hidden/Heat (1995).mkv"), 10);
    write(&tree.at("@eaDir/Heat (1995)/SYNO_EA.jpg"), 10);
    write(&tree.at("data/images/poster.jpg"), 10);
    std::fs::create_dir_all(tree.at(".new hidden")).unwrap();
    assert_eq!(poller.poll(), Vec::<PathBuf>::new());

    // A library inside the data dir is polled; only the data dir itself is left out.
    let mut inside = FolderPoller::new(tree.root(), Some(tree.0.clone()));
    assert!(inside.poll().is_empty());
    assert_eq!(
        inside.folders(),
        4,
        "the library path, Heat (1995), data and data/images"
    );
}

#[cfg(unix)]
#[test]
fn the_data_dir_is_left_out_whatever_form_its_path_has() {
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    std::fs::create_dir_all(tree.at("data/images")).unwrap();
    let alias = tree.outside("alias");
    std::os::unix::fs::symlink(tree.root(), &alias).unwrap();
    let forms = [
        (tree.root(), tree.at("Heat (1995)/../data")),
        (tree.root(), alias.join("data")),
        (alias.clone(), tree.at("data")),
    ];
    for (n, (root, data_dir)) in forms.into_iter().enumerate() {
        let mut poller = FolderPoller::new(root.clone(), Some(data_dir.clone()));
        assert!(poller.poll().is_empty());
        assert_eq!(
            poller.folders(),
            2,
            "{data_dir:?}: the library path and Heat (1995)"
        );

        tick();
        write(&tree.at(&format!("data/images/poster {n}.jpg")), 10);
        write(&tree.at(&format!("data/cache {n}/subtitle.srt")), 10);
        assert_eq!(poller.poll(), Vec::<PathBuf>::new(), "{data_dir:?}");

        tick();
        write(&tree.at("Heat (1995)/Heat (1995).srt"), 10);
        assert_eq!(
            poller.poll(),
            vec![root.join("Heat (1995)")],
            "{data_dir:?}"
        );
        std::fs::remove_file(tree.at("Heat (1995)/Heat (1995).srt")).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn a_symlinked_folder_is_polled_and_reported_by_its_path_in_the_library() {
    let tree = Tree::new();
    let heat = tree.outside("elsewhere/Heat (1995)");
    write(&heat.join("Heat (1995).mkv"), 10);
    std::os::unix::fs::symlink(&heat, tree.at("Heat (1995)")).unwrap();
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());
    assert_eq!(poller.folders(), 2);

    tick();
    write(&heat.join("Heat (1995).srt"), 10);
    assert_eq!(poller.poll(), vec![tree.at("Heat (1995)")]);

    tick();
    let alien = tree.outside("elsewhere/Alien (1979)");
    write(&alien.join("Alien (1979).mkv"), 10);
    std::os::unix::fs::symlink(&alien, tree.at("Alien (1979)")).unwrap();
    assert_eq!(poller.poll(), vec![tree.at("Alien (1979)")]);
    assert_eq!(poller.folders(), 3);
}

#[cfg(unix)]
#[test]
fn a_library_path_that_is_a_symlink_reports_paths_under_the_link() {
    let tree = Tree::new();
    write(&tree.at("Heat (1995)/Heat (1995).mkv"), 10);
    let link = tree.outside("films");
    std::os::unix::fs::symlink(tree.root(), &link).unwrap();
    let mut poller = FolderPoller::new(link.clone(), None);
    assert!(poller.poll().is_empty());

    tick();
    write(&tree.at("Up (2009)/Up (2009).mkv"), 10);
    write(&tree.at("Heat (1995)/Heat (1995).srt"), 10);
    assert_eq!(
        poller.poll(),
        sorted(vec![link.join("Heat (1995)"), link.join("Up (2009)")])
    );
}

#[cfg(unix)]
#[test]
fn symlink_loops_and_two_links_to_one_folder_are_walked_once() {
    let tree = Tree::new();
    write(&tree.at("Show/Season 1/S01E01.mkv"), 10);
    std::os::unix::fs::symlink(tree.root(), tree.at("Show/Season 1/loop")).unwrap();
    let heat = tree.outside("elsewhere/Heat (1995)");
    write(&heat.join("Heat (1995).mkv"), 10);
    std::os::unix::fs::symlink(&heat, tree.at("Heat A")).unwrap();
    std::os::unix::fs::symlink(&heat, tree.at("Heat B")).unwrap();
    let mut poller = FolderPoller::new(tree.root(), None);
    assert!(poller.poll().is_empty());
    // The library path, Show, Season 1 and one of the two links.
    assert_eq!(poller.folders(), 4);

    tick();
    write(&heat.join("Heat (1995).srt"), 10);
    let reported = poller.poll();
    assert!(
        reported == vec![tree.at("Heat A")] || reported == vec![tree.at("Heat B")],
        "{reported:?}"
    );

    // Skipped folders are known by name, so they are never new.
    tick();
    write(&tree.at("Show/Season 1/S01E02.mkv"), 10);
    assert_eq!(poller.poll(), vec![tree.at("Show/Season 1")]);
    tick();
    std::fs::create_dir(tree.at("Up (2009)")).unwrap();
    assert_eq!(poller.poll(), vec![tree.at("Up (2009)")]);
}

/// Measures polling a real library, which it only reads:
/// `RUSTYFIN_MEASURE_PATH=/mnt/media cargo test -p rustyfin-core --test library_watch_tests
/// measure -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement; needs RUSTYFIN_MEASURE_PATH"]
fn measure_polling_a_library() {
    let Some(path) = std::env::var_os("RUSTYFIN_MEASURE_PATH") else {
        return;
    };
    let rss_before = rss_kib();
    let mut poller = FolderPoller::new(PathBuf::from(path), None);
    let started = Instant::now();
    assert!(poller.poll().is_empty());
    let baseline = started.elapsed();
    let mut polls = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let reported = poller.poll().len();
        polls.push((started.elapsed(), reported));
    }
    let rss_growth = rss_kib()
        .zip(rss_before)
        .map(|(after, before)| after - before);
    println!(
        "measure: {} folders, baseline in {baseline:?}, polls (time, reported): {polls:?}, \
         resident memory grew by {rss_growth:?} KiB",
        poller.folders()
    );
}

/// Resident memory of this process in KiB, from `/proc/self/status`.
fn rss_kib() -> Option<i64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}
