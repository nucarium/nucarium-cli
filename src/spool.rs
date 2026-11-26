//! The spool: one small file per event, between the hook that writes it and
//! the daemon that applies and sends it.
//!
//! A file is written to a temporary name and renamed, so the daemon never
//! reads half of one. File names are event ids, which sort by time, so the
//! spool is replayed in the order things happened.
//!
//! There is more than one. Each container environment writes its own folder,
//! `<state_dir>/spools/<environment>/`, mounted where the shared one would
//! be, so its hook writes it as it always has; the daemon knows which
//! environment a record came from by the folder it is in. Host environments,
//! the daemon itself, and containers started before their own folder existed
//! write the shared one, `<state_dir>/spool`. A folder a container writes is
//! read as hostile: regular files only, never through a link, never more
//! than [`MAX_FILE`] of one, never more than a set number in one pass.

use crate::envelope::{valid_env_key, valid_event_id, Envelope};
use crate::paths;
use crate::process::Identity;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The most bytes a spool file may hold. The largest record that fits the
/// contract is a few kilobytes; anything bigger is no record, and is not read.
pub const MAX_FILE: u64 = 64 * 1024;

/// What a spool file holds: the envelope, and facts that stay on the box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub event: Envelope,
    /// When the hook ran, in Unix milliseconds.
    pub at_ms: u64,
    /// The nucleus session folder the agent session is working in, if any.
    /// Housekeeping uses it to know which folders are finished. Never sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The agent process whose hook wrote this, so the daemon can end the
    /// session if that process goes without saying so. Never sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<Identity>,
}

/// Why a spool file was not read as a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    /// It is not there any more.
    Gone,
    /// A link, a pipe, a device, a folder: anything but a regular file.
    NotAFile,
    /// Bigger than [`MAX_FILE`].
    TooLarge,
    /// A file, and not a record this program writes.
    NotARecord,
}

/// The folder that holds every container environment's spool folder.
pub fn spools_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("spools")
}

/// Where events from a container environment's folder are set aside: beside
/// those folders, in one no container has mounted.
fn set_aside_dir(state_dir: &Path) -> PathBuf {
    spools_dir(state_dir).join(".rejected")
}

/// How many events from container environments' folders are set aside.
pub fn set_aside_from_environments(state_dir: &Path, limit: usize) -> usize {
    let dir = set_aside_dir(state_dir);
    match real_dir(&dir) {
        true => ids_in(&dir, limit).len(),
        false => 0,
    }
}

/// True for a folder that is itself a folder, and not a link to one.
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// The event ids of the files in a folder, sorted, from at most `limit` of
/// its entries.
fn ids_in(dir: &Path, limit: usize) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .take(limit)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(".json")?;
            valid_event_id(id).then(|| id.to_string())
        })
        .collect();
    ids.sort();
    ids
}

/// Open a file for reading only if it is a regular file, and not through a
/// link: a pipe is opened without waiting for a writer, and then refused
/// once its type is read from the open file itself.
fn open_file(path: &Path) -> std::result::Result<fs::File, Unread> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Unread::Gone,
            _ => Unread::NotAFile,
        })?;
    let meta = file.metadata().map_err(|_| Unread::NotAFile)?;
    if !meta.is_file() {
        return Err(Unread::NotAFile);
    }
    if meta.len() > MAX_FILE {
        return Err(Unread::TooLarge);
    }
    Ok(file)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spool {
    dir: PathBuf,
    rejected: PathBuf,
    /// The most entries of a folder looked at in one pass.
    limit: usize,
}

impl Spool {
    /// The shared spool, or a hook's own: refused events go in its
    /// `rejected` folder.
    pub fn new(dir: &Path) -> Spool {
        Spool {
            dir: dir.to_path_buf(),
            rejected: dir.join("rejected"),
            limit: usize::MAX,
        }
    }

    /// A container environment's own spool folder, as the daemon reads it.
    /// Nothing is ever set aside inside it: the container could have put a
    /// link there in place of a folder.
    pub fn of_environment(state_dir: &Path, environment: &str) -> Option<Spool> {
        valid_env_key(environment).then(|| Spool {
            dir: spools_dir(state_dir).join(environment),
            rejected: set_aside_dir(state_dir),
            limit: usize::MAX,
        })
    }

    /// Look at no more than `limit` entries of a folder in one pass. A
    /// folder holding more is over its own limit, which only something
    /// other than a hook makes it.
    pub fn capped(mut self, limit: usize) -> Spool {
        self.limit = limit.max(1);
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, event_id: &str) -> PathBuf {
        self.dir.join(format!("{event_id}.json"))
    }

    pub fn write(&self, record: &Record) -> Result<PathBuf> {
        let path = self.path(&record.event.event_id);
        paths::write_private(&path, serde_json::to_string(record)?.as_bytes())?;
        Ok(path)
    }

    /// The ids of the events waiting, oldest first.
    pub fn pending(&self) -> Vec<String> {
        ids_in(&self.dir, self.limit)
    }

    pub fn rejected(&self) -> Vec<String> {
        match real_dir(&self.rejected) {
            true => ids_in(&self.rejected, self.limit),
            false => Vec::new(),
        }
    }

    /// True once the spool holds `limit` files. Counting stops there, so this
    /// stays cheap for the hook however large the spool has grown.
    pub fn holds_at_least(&self, limit: usize) -> bool {
        fs::read_dir(&self.dir).is_ok_and(|entries| entries.take(limit).count() >= limit)
    }

    /// Read one event, or nothing if its file is not a valid record.
    pub fn read(&self, event_id: &str) -> Option<Record> {
        self.take(event_id).ok()
    }

    /// Read one event: a regular file, read without following a link and
    /// without waiting on a pipe, of no more than [`MAX_FILE`], holding a
    /// valid record of that very event.
    pub fn take(&self, event_id: &str) -> std::result::Result<Record, Unread> {
        let file = open_file(&self.path(event_id))?;
        let mut text = String::new();
        file.take(MAX_FILE + 1)
            .read_to_string(&mut text)
            .map_err(|_| Unread::NotARecord)?;
        if text.len() as u64 > MAX_FILE {
            return Err(Unread::TooLarge);
        }
        let record: Record = serde_json::from_str(&text).map_err(|_| Unread::NotARecord)?;
        match record.event.event_id == event_id && record.event.is_valid() {
            true => Ok(record),
            false => Err(Unread::NotARecord),
        }
    }

    /// Remove an event's file. A link is removed, never what it points at.
    pub fn remove(&self, event_id: &str) {
        let _ = fs::remove_file(self.path(event_id));
    }

    /// Set an event aside: Watch refused it, or its file is not a record. A
    /// `rejected` that is not a folder of its own is not followed: the event
    /// is removed instead.
    pub fn reject(&self, event_id: &str) {
        let moved = paths::create_private_dir(&self.rejected)
            .context("creating the rejected folder")
            .and_then(|()| match real_dir(&self.rejected) {
                true => fs::rename(
                    self.path(event_id),
                    self.rejected.join(format!("{event_id}.json")),
                )
                .context("moving the event"),
                false => Err(anyhow::anyhow!("the rejected folder is not a folder")),
            });
        if moved.is_err() {
            self.remove(event_id);
        }
    }

    /// Delete refused events and abandoned temporary files older than
    /// `max_age`. Nothing is followed through a link: a link is as old as
    /// itself, and only it is removed.
    pub fn prune(&self, max_age: Duration, now: SystemTime) {
        let old = |path: &Path| {
            fs::symlink_metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > max_age)
        };
        for id in self.rejected() {
            let path = self.rejected.join(format!("{id}.json"));
            if old(&path) {
                let _ = fs::remove_file(path);
            }
        }
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.take(self.limit).flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".tmp") && old(&entry.path()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{unix_ms, Draft, EventType};
    use std::os::unix::fs::symlink;
    use std::time::{Instant, UNIX_EPOCH};

    fn record(env_id: &str, at: SystemTime) -> Record {
        let draft = Draft {
            env_id: env_id.into(),
            session_id: "s1".into(),
            ..Draft::default()
        };
        Record {
            event: Envelope::new(EventType::Stop, draft, at).unwrap(),
            at_ms: unix_ms(at),
            workspace: None,
            process: None,
        }
    }

    fn fifo(path: &Path) {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    /// A spool, and an event id and the path its file would have.
    fn planted() -> (tempfile::TempDir, Spool, String, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(&dir.path().join("spool"));
        fs::create_dir_all(spool.dir()).unwrap();
        let id = record("client-a", SystemTime::now()).event.event_id;
        let path = spool.path(&id);
        (dir, spool, id, path)
    }

    #[test]
    fn a_record_is_read_back_whole() {
        let (_dir, spool, _, _) = planted();
        let written = record("client-a", SystemTime::now());
        spool.write(&written).unwrap();
        assert_eq!(
            spool.pending(),
            std::slice::from_ref(&written.event.event_id)
        );
        assert_eq!(spool.take(&written.event.event_id), Ok(written));
    }

    #[test]
    fn a_link_in_place_of_a_record_is_not_followed() {
        let (dir, spool, id, path) = planted();
        // A good record elsewhere, which the link names.
        let elsewhere = dir.path().join("elsewhere.json");
        let mut good = record("client-a", SystemTime::now());
        good.event.event_id = id.clone();
        fs::write(&elsewhere, serde_json::to_string(&good).unwrap()).unwrap();
        symlink(&elsewhere, &path).unwrap();
        assert_eq!(spool.pending(), std::slice::from_ref(&id));
        assert_eq!(spool.take(&id), Err(Unread::NotAFile));
        // Removing it removes the link and leaves what it named.
        spool.remove(&id);
        assert!(fs::symlink_metadata(&path).is_err());
        assert!(elsewhere.exists());
    }

    #[test]
    fn a_pipe_in_place_of_a_record_is_refused_without_waiting_for_a_writer() {
        let (_dir, spool, id, path) = planted();
        fifo(&path);
        let started = Instant::now();
        assert_eq!(spool.take(&id), Err(Unread::NotAFile));
        assert!(started.elapsed() < Duration::from_secs(1));
        spool.remove(&id);
        assert!(fs::symlink_metadata(&path).is_err());
    }

    #[test]
    fn a_folder_or_a_link_to_a_device_in_place_of_a_record_is_refused() {
        let (_dir, spool, id, path) = planted();
        fs::create_dir(&path).unwrap();
        assert_eq!(spool.take(&id), Err(Unread::NotAFile));
        fs::remove_dir(&path).unwrap();
        symlink("/dev/zero", &path).unwrap();
        assert_eq!(spool.take(&id), Err(Unread::NotAFile));
    }

    #[test]
    fn a_file_bigger_than_any_record_is_not_read() {
        let (_dir, spool, id, path) = planted();
        let mut good = serde_json::to_value(record("client-a", SystemTime::now())).unwrap();
        good["event"]["event_id"] = serde_json::json!(id);
        let text = good.to_string();
        // A record padded past the cap with spaces, which JSON allows.
        let padded = format!("{text}{}", " ".repeat(MAX_FILE as usize));
        fs::write(&path, &padded).unwrap();
        assert_eq!(spool.take(&id), Err(Unread::TooLarge));
        // The same record within the cap reads.
        fs::write(&path, &text).unwrap();
        assert!(spool.take(&id).is_ok());
        assert!(text.len() < 4096, "a record is small: {}", text.len());
    }

    #[test]
    fn a_file_that_is_not_a_record_of_its_own_event_is_not_one() {
        let (_dir, spool, id, path) = planted();
        fs::write(&path, "not json").unwrap();
        assert_eq!(spool.take(&id), Err(Unread::NotARecord));
        // A good record filed under another event's id.
        let other = record("client-a", UNIX_EPOCH + Duration::from_secs(1_795_000_000));
        fs::write(&path, serde_json::to_string(&other).unwrap()).unwrap();
        assert_eq!(spool.take(&id), Err(Unread::NotARecord));
        assert_eq!(spool.take(&other.event.event_id), Err(Unread::Gone));
    }

    #[test]
    fn a_pass_looks_at_no_more_entries_than_its_cap() {
        let (_dir, spool, _, _) = planted();
        let start = SystemTime::now();
        for n in 0..10 {
            spool
                .write(&record("client-a", start + Duration::from_millis(n)))
                .unwrap();
        }
        assert_eq!(spool.pending().len(), 10);
        assert_eq!(spool.clone().capped(4).pending().len(), 4);
    }

    #[test]
    fn a_rejected_folder_that_is_a_link_is_not_followed() {
        let (dir, spool, _, _) = planted();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep.json"), "keep").unwrap();
        symlink(&elsewhere, spool.dir().join("rejected")).unwrap();
        let written = record("client-a", SystemTime::now());
        spool.write(&written).unwrap();
        spool.reject(&written.event.event_id);
        // The event is gone, and nothing went through the link.
        assert!(spool.pending().is_empty());
        assert_eq!(
            fs::read_dir(&elsewhere).unwrap().count(),
            1,
            "an event was moved through a link"
        );
        assert!(spool.rejected().is_empty());
        // Pruning never reaches through it either.
        spool.prune(Duration::ZERO, SystemTime::now() + Duration::from_secs(60));
        assert!(elsewhere.join("keep.json").exists());
    }

    #[test]
    fn an_environments_spool_sets_events_aside_where_no_container_writes() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::of_environment(dir.path(), "client-a").unwrap();
        assert_eq!(spool.dir(), dir.path().join("spools/client-a"));
        let written = record("client-a", SystemTime::now());
        spool.write(&written).unwrap();
        spool.reject(&written.event.event_id);
        assert!(spool.pending().is_empty());
        assert_eq!(
            spool.rejected(),
            std::slice::from_ref(&written.event.event_id)
        );
        assert!(dir
            .path()
            .join("spools/.rejected")
            .join(format!("{}.json", written.event.event_id))
            .is_file());
        assert_eq!(set_aside_from_environments(dir.path(), 100), 1);
        // A name that is not an environment's names no folder.
        for not_one in ["", ".rejected", "..", "a/b", "Client A"] {
            assert_eq!(
                Spool::of_environment(dir.path(), not_one),
                None,
                "{not_one}"
            );
        }
    }

    #[test]
    fn an_old_temporary_file_is_pruned_and_a_link_only_as_itself() {
        let (dir, spool, _, _) = planted();
        let target = dir.path().join("target");
        fs::write(&target, "kept").unwrap();
        symlink(&target, spool.dir().join(".x.json.1.1.tmp")).unwrap();
        fs::write(spool.dir().join(".y.json.1.2.tmp"), "").unwrap();
        spool.prune(Duration::ZERO, SystemTime::now() + Duration::from_secs(60));
        assert_eq!(fs::read_dir(spool.dir()).unwrap().count(), 0);
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
    }
}
