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
    use super::write_private;

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
