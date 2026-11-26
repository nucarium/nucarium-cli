//! Where files live, and how private ones are written.

use anyhow::{anyhow, Context, Result};
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The agent user's home folder, where an adapter looks for a third party's
/// files when the config names no other place.
pub fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn xdg(var: &str, fallback: &[&str]) -> Result<PathBuf> {
    if let Some(dir) = env::var_os(var).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let mut path = home().ok_or_else(|| anyhow!("neither {var} nor HOME is set"))?;
    path.extend(fallback);
    Ok(path)
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
pub fn config_home() -> Result<PathBuf> {
    xdg("XDG_CONFIG_HOME", &[".config"])
}

/// The daemon's config folder: `~/.config/nucleated`.
pub fn daemon_config_dir() -> Result<PathBuf> {
    Ok(config_home()?.join("nucleated"))
}

/// The daemon's state folder: `~/.local/state/nucleated`.
pub fn state_dir() -> Result<PathBuf> {
    Ok(xdg("XDG_STATE_HOME", &[".local", "state"])?.join("nucleated"))
}

/// The daemon's cache folder: `~/.cache/nucleated`. What is kept here may
/// hold content, a clone of the owner's config repository say, and so it is
/// outside the state folder, which holds none.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(xdg("XDG_CACHE_HOME", &[".cache"])?.join("nucleated"))
}

/// Create a folder only its owner can enter. An existing folder is left as it is.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Each write's own number, so that two threads writing the same file never
/// share a temporary file.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Write a file only its owner can read, by writing a temporary file beside it
/// and renaming, so a reader never sees half of it. The temporary file is
/// this write's alone: two writers of one file, in two processes or two
/// threads of one, each rename a whole file of their own.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent folder", path.display()))?;
    create_private_dir(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| -> Result<()> {
        let mut file = options.open(&tmp)?;
        // The mode above only applies to a file this call creates.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// What a first look at a file found, before it is opened: enough to tell,
/// once it is open, that the open reached that same file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Looked {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
}

/// Look at a file without opening it and without following a link, and
/// refuse it unless it is a regular file: a link, a pipe, a device, a
/// socket or a folder is never opened.
pub fn look_regular(path: &Path) -> std::io::Result<Looked> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a plain file"));
    }
    Ok(Looked {
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
    })
}

/// Open, for reading, the file a [`look_regular`] looked at, never through
/// a link, and refuse it if what opened is not that same file: something
/// swapped in between the look and the open is never read. Opening does not
/// wait on anything.
pub fn open_looked(path: &Path, looked: &Looked) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    same_file(file, looked)
}

/// The open file, if it is the regular file that was looked at.
fn same_file(file: fs::File, looked: &Looked) -> std::io::Result<fs::File> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    if (meta.dev(), meta.ino()) != (looked.dev, looked.ino) || !meta.is_file() {
        return Err(std::io::Error::other("not the file that was looked at"));
    }
    Ok(file)
}

/// The text of a small plain file that something else may have written: not
/// through a link, not a pipe or a device, and not past `max` bytes. Any of
/// those is an error, and nothing of it is read; a pipe or a device is not
/// even opened.
pub fn read_plain(path: &Path, max: u64) -> std::io::Result<String> {
    let looked = look_regular(path)?;
    if looked.len > max {
        return Err(std::io::Error::other("too large"));
    }
    read_to_cap(open_looked(path, &looked)?, max)
}

/// All of an open file's text, or an error past `max` bytes.
fn read_to_cap(file: fs::File, max: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    file.take(max + 1).read_to_string(&mut text)?;
    if text.len() as u64 > max {
        return Err(std::io::Error::other("too large"));
    }
    Ok(text)
}

/// A folder held open, so that what is done in it is done in that very
/// folder: an entry in it is looked at, opened, made or removed by its name
/// in the folder, and no link is followed to get there. For folders
/// something else can write, a container or a session.
pub struct Dir(OwnedFd);

/// What a look at one entry of a [`Dir`] found, without following a link.
pub struct Entry {
    pub regular: bool,
    pub looked: Looked,
    pub modified: Option<std::time::SystemTime>,
}

fn c_string(bytes: &[u8]) -> std::io::Result<CString> {
    CString::new(bytes).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

fn checked(result: libc::c_int) -> std::io::Result<libc::c_int> {
    match result {
        -1 => Err(std::io::Error::last_os_error()),
        fd => Ok(fd),
    }
}

impl Dir {
    /// Open a folder. Without `follow`, a link in its place is refused. Only
    /// the last part of the path is the folder's own; the folders above it
    /// are opened as they are.
    pub fn open(path: &Path, follow: bool) -> std::io::Result<Dir> {
        let name = c_string(path.as_os_str().as_bytes())?;
        let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
        // SAFETY: `name` is a valid C string for the length of the call.
        let fd = checked(unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | nofollow,
            )
        })?;
        // SAFETY: `open` just returned this descriptor, and nothing else owns it.
        Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// The folder `path` names inside `root`, reached one folder at a time
    /// from `root` held open, never through a link: a folder on the way
    /// swapped for a link is refused rather than followed. `root` itself is
    /// opened as it is; `path` must be in it, without `..`.
    pub fn beneath(root: &Path, path: &Path) -> std::io::Result<Dir> {
        use std::path::Component;
        let inside = path
            .strip_prefix(root)
            .map_err(|_| std::io::Error::other("not inside the folder it is held to"))?;
        let mut dir = Dir::open(root, true)?;
        for part in inside.components() {
            match part {
                Component::Normal(name) => dir = dir.at_bytes(name.as_bytes())?,
                Component::CurDir => {}
                _ => return Err(std::io::Error::other("a path that leaves its folder")),
            }
        }
        Ok(dir)
    }

    /// A folder in this one, never through a link.
    pub fn at(&self, name: &str) -> std::io::Result<Dir> {
        self.at_bytes(name.as_bytes())
    }

    fn at_bytes(&self, name: &[u8]) -> std::io::Result<Dir> {
        let name = c_string(name)?;
        // SAFETY: a descriptor this folder owns, and a valid C string.
        let fd = checked(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        })?;
        // SAFETY: `openat` just returned this descriptor, and nothing else owns it.
        Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Look at an entry without opening it, and without following a link.
    pub fn look(&self, name: &str) -> std::io::Result<Entry> {
        let name = c_string(name.as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: a descriptor this folder owns, a valid C string, and room
        // for one `stat`, which a successful call fills.
        checked(unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        // SAFETY: filled by the successful call above.
        let stat = unsafe { stat.assume_init() };
        let modified = u64::try_from(stat.st_mtime)
            .ok()
            .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
        // `st_dev` and `st_ino` are not the same type on every platform; as
        // the standard library gives them, they are both `u64`.
        #[allow(clippy::unnecessary_cast)]
        Ok(Entry {
            regular: stat.st_mode & libc::S_IFMT == libc::S_IFREG,
            looked: Looked {
                dev: stat.st_dev as u64,
                ino: stat.st_ino as u64,
                len: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
            },
            modified,
        })
    }

    /// Open, for reading, the regular file a [`Dir::look`] looked at, and
    /// refuse it if what opened is not that same file. Never through a
    /// link, and never waiting on anything.
    pub fn open_looked(&self, name: &str, looked: &Looked) -> std::io::Result<fs::File> {
        let c = c_string(name.as_bytes())?;
        // SAFETY: a descriptor this folder owns, and a valid C string.
        let fd = checked(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_NOCTTY
                    | libc::O_CLOEXEC,
            )
        })?;
        // SAFETY: `openat` just returned this descriptor, and nothing else owns it.
        same_file(fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }), looked)
    }

    /// The text of a small regular file in this folder: looked at first, so
    /// a link, a pipe or a device is refused and never opened, then opened
    /// only if it is still that file. An error past `max` bytes.
    pub fn read_plain(&self, name: &str, max: u64) -> std::io::Result<String> {
        let entry = self.look(name)?;
        if !entry.regular {
            return Err(std::io::Error::other("not a plain file"));
        }
        if entry.looked.len > max {
            return Err(std::io::Error::other("too large"));
        }
        read_to_cap(self.open_looked(name, &entry.looked)?, max)
    }

    /// Make a new file of the daemon's own, mode 600, holding `bytes`. An
    /// entry already under that name, of any kind, is left as it is and is
    /// an error. Not synced to disk: for a copy whose loss in a crash costs
    /// nothing.
    pub fn create_new(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.write_new(name, bytes, false)
    }

    fn write_new(&self, name: &str, bytes: &[u8], sync: bool) -> std::io::Result<()> {
        let c = c_string(name.as_bytes())?;
        // SAFETY: a descriptor this folder owns, and a valid C string.
        let fd = checked(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_NOCTTY
                    | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        })?;
        // SAFETY: `openat` just made this file, and nothing else owns it.
        let mut file = fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let written = file
            .write_all(bytes)
            .and_then(|()| if sync { file.sync_all() } else { Ok(()) });
        if written.is_err() {
            let _ = self.unlink(name);
        }
        written
    }

    /// Put a new file of the daemon's own, holding `bytes`, in place of the
    /// entry `name`: written whole under a name of its own first, then
    /// renamed over it, so whatever was there (a link included) is replaced
    /// as an entry and never written through. A folder is not replaced.
    /// Synced to disk before the rename, so a crash leaves the old entry or
    /// the whole new one.
    pub fn replace(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = format!(".{name}.{}.tmp", std::process::id());
        let _ = self.unlink(&tmp);
        self.write_new(&tmp, bytes, true)?;
        let (from, to) = (c_string(tmp.as_bytes())?, c_string(name.as_bytes())?);
        // SAFETY: the descriptor this folder owns, twice, and two valid C
        // strings.
        let renamed = checked(unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                from.as_ptr(),
                self.0.as_raw_fd(),
                to.as_ptr(),
            )
        });
        if renamed.is_err() {
            let _ = self.unlink(&tmp);
        }
        renamed.map(|_| ())
    }

    /// Remove an entry. A link is removed, never what it names; a folder is
    /// not removed.
    pub fn unlink(&self, name: &str) -> std::io::Result<()> {
        let name = c_string(name.as_bytes())?;
        // SAFETY: a descriptor this folder owns, and a valid C string.
        checked(unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) }).map(|_| ())
    }

    /// The names of at most `limit` entries, `.` and `..` aside.
    pub fn names(&self, limit: usize) -> Vec<String> {
        let mut names = Vec::new();
        // A descriptor of its own for the listing, which closes it.
        // SAFETY: `fcntl` duplicates a descriptor this folder owns.
        let Ok(fd) = checked(unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })
        else {
            return names;
        };
        // SAFETY: `fd` is ours; on success the listing owns it.
        let listing = unsafe { libc::fdopendir(fd) };
        if listing.is_null() {
            // SAFETY: `fdopendir` failed, so `fd` is still ours to close.
            unsafe { libc::close(fd) };
            return names;
        }
        // SAFETY: `listing` is a valid listing until `closedir` below; each
        // entry `readdir` returns is valid until the next call.
        unsafe {
            libc::rewinddir(listing);
            while names.len() < limit {
                let entry = libc::readdir(listing);
                if entry.is_null() {
                    break;
                }
                let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr());
                let name = name.to_string_lossy();
                if name != "." && name != ".." {
                    names.push(name.into_owned());
                }
            }
            libc::closedir(listing);
        }
        names
    }
}

/// The permission bits of a file, where the platform has them.
pub fn mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    fn fifo(path: &Path) {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn a_pipe_or_a_device_is_refused_before_it_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let pipe = dir.path().join("pipe");
        fifo(&pipe);
        let device = dir.path().join("device");
        symlink("/dev/zero", &device).unwrap();
        let started = Instant::now();
        assert!(look_regular(&pipe).is_err());
        assert!(read_plain(&pipe, 64).is_err());
        assert!(look_regular(&device).is_err());
        assert!(read_plain(&device, 64).is_err());
        let held = Dir::open(dir.path(), false).unwrap();
        assert!(held.read_plain("pipe", 64).is_err());
        assert!(held.read_plain("device", 64).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        // A plain file reads, and a link to one does not.
        let plain = dir.path().join("plain");
        fs::write(&plain, "text").unwrap();
        let link = dir.path().join("link");
        symlink(&plain, &link).unwrap();
        assert_eq!(read_plain(&plain, 64).unwrap(), "text");
        assert_eq!(held.read_plain("plain", 64).unwrap(), "text");
        assert!(read_plain(&link, 64).is_err());
        assert!(held.read_plain("link", 64).is_err());
        assert!(read_plain(&plain, 3).is_err());
    }

    #[test]
    fn a_file_swapped_in_between_the_look_and_the_open_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pointer");
        fs::write(&path, "first").unwrap();
        let looked = look_regular(&path).unwrap();
        // Another file renamed into its place after the look.
        let other = dir.path().join("other");
        fs::write(&other, "second").unwrap();
        fs::rename(&other, &path).unwrap();
        assert!(open_looked(&path, &looked).is_err());
        // A pipe swapped in is refused too, without waiting for a writer.
        fs::remove_file(&path).unwrap();
        fifo(&path);
        let started = Instant::now();
        assert!(open_looked(&path, &looked).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        // The file that was looked at opens.
        fs::remove_file(&path).unwrap();
        fs::write(&path, "third").unwrap();
        let looked = look_regular(&path).unwrap();
        assert!(open_looked(&path, &looked).is_ok());
    }

    #[test]
    fn a_folder_held_beneath_another_is_never_reached_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/file"), "inside").unwrap();
        let held = Dir::beneath(&root, &root.join("a/b")).unwrap();
        assert_eq!(held.read_plain("file", 64).unwrap(), "inside");
        // A folder on the way that is a link, to a folder outside.
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("b")).unwrap();
        fs::write(outside.path().join("b/file"), "outside").unwrap();
        fs::remove_dir_all(root.join("a")).unwrap();
        symlink(outside.path(), root.join("a")).unwrap();
        assert!(Dir::beneath(&root, &root.join("a/b")).is_err());
        // A path not in the folder, or leaving it.
        assert!(Dir::beneath(&root, outside.path()).is_err());
        assert!(Dir::beneath(&root, &root.join("../x")).is_err());
    }

    #[test]
    fn a_new_file_never_replaces_what_is_under_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let held = Dir::open(dir.path(), false).unwrap();
        held.create_new("made", b"bytes").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("made")).unwrap(),
            "bytes"
        );
        assert!(held.create_new("made", b"other").is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("made")).unwrap(),
            "bytes"
        );
        // Nor writes through a link, or into a folder.
        let target = dir.path().join("target");
        fs::write(&target, "kept").unwrap();
        symlink(&target, dir.path().join("link")).unwrap();
        assert!(held.create_new("link", b"x").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
        fs::create_dir(dir.path().join("folder")).unwrap();
        assert!(held.create_new("folder", b"x").is_err());
        // Replacing puts a file in place of a link, not through it, and
        // leaves a folder.
        held.replace("link", b"new").unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("link")).unwrap(), "new");
        assert!(fs::symlink_metadata(dir.path().join("link"))
            .unwrap()
            .is_file());
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
        assert!(held.replace("folder", b"x").is_err());
        assert!(dir.path().join("folder").is_dir());
        let mut names = held.names(16);
        names.sort();
        assert_eq!(names, ["folder", "link", "made", "target"]);
    }

    #[test]
    fn threads_writing_one_file_at_once_each_write_a_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shared.json");
        let contents: Vec<String> = (0..16).map(|n| format!("{n}").repeat(4_096)).collect();
        std::thread::scope(|scope| {
            for text in &contents {
                let file = &file;
                scope.spawn(move || {
                    for _ in 0..20 {
                        write_private(file, text.as_bytes()).unwrap();
                    }
                });
            }
        });
        let left = std::fs::read_to_string(&file).unwrap();
        assert!(contents.contains(&left), "a file made of two writes");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["shared.json"], "a temporary file was left behind");
    }
}
