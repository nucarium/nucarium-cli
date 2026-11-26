//! Where files live, and how private ones are written.

use anyhow::{anyhow, Context, Result};
use std::env;
use std::fs;
use std::io::Write;
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

/// Look at a file without opening it, and refuse it unless it is a regular
/// file: a pipe, a device, a socket or a folder is never opened. With
/// `follow`, a link is looked through and what it names must be a regular
/// file; without it, a link is refused.
pub fn look_regular(path: &Path, follow: bool) -> std::io::Result<Looked> {
    use std::os::unix::fs::MetadataExt;
    let meta = match follow {
        true => fs::metadata(path)?,
        false => fs::symlink_metadata(path)?,
    };
    if !meta.is_file() {
        return Err(std::io::Error::other("not a plain file"));
    }
    Ok(Looked {
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
    })
}

/// Open, for reading, the file a [`look_regular`] looked at, and refuse it
/// if what opened is not that same file: something swapped in between the
/// look and the open is never read. Opening does not wait on anything.
pub fn open_looked(path: &Path, follow: bool, looked: &Looked) -> std::io::Result<fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nofollow | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
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
    use std::io::Read;
    let looked = look_regular(path, false)?;
    if looked.len > max {
        return Err(std::io::Error::other("too large"));
    }
    let file = open_looked(path, false, &looked)?;
    if file.metadata()?.len() > max {
        return Err(std::io::Error::other("too large"));
    }
    let mut text = String::new();
    file.take(max).read_to_string(&mut text)?;
    Ok(text)
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
        for follow in [false, true] {
            assert!(look_regular(&pipe, follow).is_err());
            assert!(read_plain(&pipe, 64).is_err());
        }
        // Through a link only when asked, and then never to a device.
        assert!(look_regular(&device, false).is_err());
        assert!(look_regular(&device, true).is_err());
        assert!(read_plain(&device, 64).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        // A plain file reads, and a link to one only when following.
        let plain = dir.path().join("plain");
        fs::write(&plain, "text").unwrap();
        let link = dir.path().join("link");
        symlink(&plain, &link).unwrap();
        assert_eq!(read_plain(&plain, 64).unwrap(), "text");
        assert!(read_plain(&link, 64).is_err());
        assert_eq!(look_regular(&link, true).unwrap().len, 4);
    }

    #[test]
    fn a_file_swapped_in_between_the_look_and_the_open_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pointer");
        fs::write(&path, "first").unwrap();
        let looked = look_regular(&path, false).unwrap();
        // Another file renamed into its place after the look.
        let other = dir.path().join("other");
        fs::write(&other, "second").unwrap();
        fs::rename(&other, &path).unwrap();
        assert!(open_looked(&path, false, &looked).is_err());
        assert!(open_looked(&path, true, &looked).is_err());
        // A pipe swapped in is refused too, without waiting for a writer.
        fs::remove_file(&path).unwrap();
        fifo(&path);
        let started = Instant::now();
        assert!(open_looked(&path, false, &looked).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        // The file that was looked at opens.
        fs::remove_file(&path).unwrap();
        fs::write(&path, "third").unwrap();
        let looked = look_regular(&path, false).unwrap();
        assert!(open_looked(&path, false, &looked).is_ok());
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
