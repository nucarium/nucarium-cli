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
//! than [`MAX_FILE`] of one, never more than a set number in one pass. A
//! file is looked at by its name in the folder held open, before anything
//! opens it, and is read only if what opens is the file looked at. Refused
//! events from every folder are set aside in `<state_dir>/spools/.rejected`,
//! which no container mounts.

use crate::envelope::{valid_env_key, valid_event_id, Envelope};
use crate::paths::{self, Dir, Entry};
use crate::process::Identity;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The most files of one spool folder that are not records the daemon sets
/// aside in one pass. The rest of that folder's are removed without a copy:
/// a folder filled with them is not let make every pass slow for the rest.
pub const MAX_SET_ASIDE_PER_PASS: usize = 16;

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

/// Where refused events are set aside, from every spool folder the daemon
/// reads: beside the environments' folders, in one no container mounts.
fn set_aside_dir(state_dir: &Path) -> PathBuf {
    spools_dir(state_dir).join(".rejected")
}

/// Where the shared spool set refused events aside before they went where
/// no container writes. Old ones there are still counted and pruned.
const LEGACY_REJECTED: &str = "rejected";

/// The event ids among a spool folder's entry names, sorted: `<id>.json`
/// with a valid id, and nothing else. A name set aside events go under is
/// no event waiting, whatever folder it is planted in.
fn event_ids(names: Vec<String>) -> Vec<String> {
    let mut ids: Vec<String> = names
        .into_iter()
        .filter_map(|name| {
            let id = name.strip_suffix(".json")?;
            valid_event_id(id).then(|| id.to_string())
        })
        .collect();
    ids.sort();
    ids
}

/// The event ids among a set-aside folder's entry names, sorted, once each.
fn set_aside_ids(names: Vec<String>) -> Vec<String> {
    let mut ids: Vec<String> = set_aside_entries(names)
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The entries among a folder's names that hold an event: `<id>.json` as a
/// spool names them, or `<id>@<origin>.json` as one is set aside. Each with
/// its event id.
fn set_aside_entries(names: Vec<String>) -> Vec<(String, String)> {
    names
        .into_iter()
        .filter_map(|name| {
            let stem = name.strip_suffix(".json")?;
            let id = stem.split_once('@').map_or(stem, |(id, _)| id);
            valid_event_id(id).then(|| (name.clone(), id.to_string()))
        })
        .collect()
}

/// The name an event is set aside under: its id, and the folder it came
/// from, so that one folder's event never takes the place of another's.
fn set_aside_name(event_id: &str, origin: &str) -> String {
    format!("{event_id}@{origin}.json")
}

/// What the shared spool's events are set aside as coming from: no
/// environment's key can start with `_`.
const SHARED_ORIGIN: &str = "_shared";

fn file_name(event_id: &str) -> String {
    format!("{event_id}.json")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spool {
    dir: PathBuf,
    /// Where refused events go, in a folder no container mounts, and what
    /// they are set aside as coming from. Nothing for a hook's spool, which
    /// never sets one aside.
    set_aside: Option<(PathBuf, String)>,
    /// Whether this is the shared spool, whose events set aside in its own
    /// `rejected` folder before are still counted and pruned there.
    legacy: bool,
    /// Whether the folder itself is opened as the config names it, link or
    /// not: the shared spool's and a hook's. The container whose spool it is
    /// has it mounted and cannot replace it. An environment's own folder,
    /// which the daemon makes, is never opened through a link.
    configured: bool,
    /// The most entries of a folder looked at in one pass.
    limit: usize,
}

impl Spool {
    /// A hook's spool, which it writes events to.
    pub fn new(dir: &Path) -> Spool {
        Spool {
            dir: dir.to_path_buf(),
            set_aside: None,
            legacy: false,
            configured: true,
            limit: usize::MAX,
        }
    }

    /// The shared spool, as the daemon reads it. Containers started before
    /// their own folder existed still write it, so nothing is ever set aside
    /// inside it: refused events go where no container writes.
    pub fn shared(dir: &Path, state_dir: &Path) -> Spool {
        Spool {
            dir: dir.to_path_buf(),
            set_aside: Some((set_aside_dir(state_dir), SHARED_ORIGIN.to_string())),
            legacy: true,
            configured: true,
            limit: usize::MAX,
        }
    }

    /// A container environment's own spool folder, as the daemon reads it.
    /// Nothing is ever set aside inside it: refused events go where no
    /// container writes.
    pub fn of_environment(state_dir: &Path, environment: &str) -> Option<Spool> {
        valid_env_key(environment).then(|| Spool {
            dir: spools_dir(state_dir).join(environment),
            set_aside: Some((set_aside_dir(state_dir), environment.to_string())),
            legacy: false,
            configured: false,
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
        self.dir.join(file_name(event_id))
    }

    fn own(&self) -> io::Result<Dir> {
        Dir::open(&self.dir, self.configured)
    }

    /// The folders refused events are kept in: where they are set aside,
    /// and, for the shared spool, where they were set aside before. Neither
    /// is reached through a link.
    fn kept(&self) -> Vec<Dir> {
        let set_aside = self
            .set_aside
            .as_ref()
            .and_then(|(dir, _)| Dir::open(dir, false).ok());
        let legacy = self
            .legacy
            .then(|| self.own().and_then(|own| own.at(LEGACY_REJECTED)).ok())
            .flatten();
        set_aside.into_iter().chain(legacy).collect()
    }

    pub fn write(&self, record: &Record) -> Result<PathBuf> {
        let path = self.path(&record.event.event_id);
        paths::write_private(&path, serde_json::to_string(record)?.as_bytes())?;
        Ok(path)
    }

    /// The ids of the events waiting, oldest first.
    pub fn pending(&self) -> Vec<String> {
        self.own()
            .map(|own| event_ids(own.names(self.limit)))
            .unwrap_or_default()
    }

    /// The ids of the events set aside, from every spool folder the daemon
    /// reads, and those the shared spool set aside before.
    pub fn rejected(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .kept()
            .iter()
            .flat_map(|dir| set_aside_ids(dir.names(self.limit)))
            .collect();
        ids.sort();
        ids.dedup();
        ids
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

    /// Read one event: a regular file, looked at before it is opened, so a
    /// pipe or a device is never opened, and opened only if it is still the
    /// file looked at; never through a link, of no more than [`MAX_FILE`],
    /// holding a valid record of that very event.
    pub fn take(&self, event_id: &str) -> std::result::Result<Record, Unread> {
        let own = self.own().map_err(|_| Unread::Gone)?;
        let name = file_name(event_id);
        let entry = own.look(&name).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => Unread::Gone,
            _ => Unread::NotAFile,
        })?;
        Self::read_looked(&own, &name, event_id, &entry)
    }

    /// Read the file a look found, refusing anything but that very regular
    /// file.
    fn read_looked(
        own: &Dir,
        name: &str,
        event_id: &str,
        entry: &Entry,
    ) -> std::result::Result<Record, Unread> {
        if !entry.regular {
            return Err(Unread::NotAFile);
        }
        if entry.looked.len > MAX_FILE {
            return Err(Unread::TooLarge);
        }
        let file = own
            .open_looked(name, &entry.looked)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => Unread::Gone,
                _ => Unread::NotAFile,
            })?;
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
        if let Ok(own) = self.own() {
            let _ = own.unlink(&file_name(event_id));
        }
    }

    /// Set an event aside: Watch refused it, or its file is not a record.
    /// What is kept is the daemon's own copy, in a new regular file in the
    /// set-aside folder held open, named for the event and the folder it
    /// came from: `held`, the record the daemon read, or else what the file
    /// holds now if it is still a regular file of no more than [`MAX_FILE`].
    /// Nothing at the event's name is ever moved, so a folder or a link a
    /// container put there stays in the container's own folder, and an
    /// event already set aside under that name is never replaced. The
    /// event's file is then removed. A hook's spool has nowhere to set one
    /// aside, and only removes it.
    pub fn reject(&self, event_id: &str, held: Option<&Record>) {
        let name = file_name(event_id);
        let _ = (|| -> Result<()> {
            let (to, origin) = self
                .set_aside
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("nowhere to set an event aside"))?;
            let bytes = match held {
                Some(record) => serde_json::to_vec(record)?,
                None => self.bytes_of(&name)?,
            };
            paths::create_private_dir(to)?;
            let into = Dir::open(to, false).context("opening the set-aside folder")?;
            into.create_new(&set_aside_name(event_id, origin), &bytes)
                .context("setting the event aside")
        })();
        self.remove(event_id);
    }

    /// What a regular file in this spool holds, read as [`Spool::take`]
    /// reads one.
    fn bytes_of(&self, name: &str) -> Result<Vec<u8>> {
        let own = self.own()?;
        Ok(own.read_plain(name, MAX_FILE)?.into_bytes())
    }

    /// Delete refused events and abandoned temporary files older than
    /// `max_age`. Nothing is followed through a link: a link is as old as
    /// itself, and only it is removed.
    pub fn prune(&self, max_age: Duration, now: SystemTime) {
        let old = |dir: &Dir, name: &str| {
            dir.look(name)
                .ok()
                .and_then(|entry| entry.modified)
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > max_age)
        };
        for dir in self.kept() {
            for (name, _) in set_aside_entries(dir.names(self.limit)) {
                if old(&dir, &name) {
                    let _ = dir.unlink(&name);
                }
            }
        }
        if let Ok(own) = self.own() {
            for name in own.names(self.limit) {
                if name.ends_with(".tmp") && old(&own, &name) {
                    let _ = own.unlink(&name);
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

    /// The shared spool, as the daemon reads it, and an event id and the
    /// path its file would have.
    fn planted() -> (tempfile::TempDir, Spool, String, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::shared(&dir.path().join("spool"), dir.path());
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
    fn only_an_events_own_name_is_an_event_waiting() {
        let (_dir, spool, id, path) = planted();
        fs::write(spool.dir().join(format!("{id}@x.json")), "planted").unwrap();
        assert!(spool.pending().is_empty());
        fs::write(&path, "").unwrap();
        assert_eq!(spool.pending(), std::slice::from_ref(&id));
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

    /// Every name in a folder, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_shared_spool_sets_events_aside_where_no_container_writes() {
        let (dir, spool, _, _) = planted();
        // A container made the old `rejected` folder a link out of it.
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep.json"), "keep").unwrap();
        symlink(&elsewhere, spool.dir().join("rejected")).unwrap();
        let written = record("client-a", SystemTime::now());
        spool.write(&written).unwrap();
        spool.reject(&written.event.event_id, None);
        assert!(spool.pending().is_empty());
        // Set aside beside the environments' folders, and nothing went
        // through the link.
        assert_eq!(
            entries(&dir.path().join("spools/.rejected")),
            [format!("{}@_shared.json", written.event.event_id)]
        );
        assert_eq!(entries(&elsewhere), ["keep.json"]);
        assert_eq!(
            spool.rejected(),
            std::slice::from_ref(&written.event.event_id)
        );
        // Pruning never reaches through the link either.
        spool.prune(Duration::ZERO, SystemTime::now() + Duration::from_secs(60));
        assert_eq!(entries(&elsewhere), ["keep.json"]);
        assert!(spool.rejected().is_empty());
    }

    #[test]
    fn a_link_in_place_of_the_set_aside_folder_is_never_followed() {
        let (dir, spool, _, _) = planted();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep.json"), "keep").unwrap();
        fs::create_dir_all(dir.path().join("spools")).unwrap();
        symlink(&elsewhere, dir.path().join("spools/.rejected")).unwrap();
        let written = record("client-a", SystemTime::now());
        spool.write(&written).unwrap();
        spool.reject(&written.event.event_id, None);
        // The event is gone, and nothing was written outside.
        assert!(spool.pending().is_empty());
        assert_eq!(entries(&elsewhere), ["keep.json"]);
        assert!(spool.rejected().is_empty());
        spool.prune(Duration::ZERO, SystemTime::now() + Duration::from_secs(60));
        assert_eq!(entries(&elsewhere), ["keep.json"]);
        // An environment's spool is held to the same folder.
        let theirs = Spool::of_environment(dir.path(), "client-a").unwrap();
        let written = record("client-a", SystemTime::now());
        theirs.write(&written).unwrap();
        theirs.reject(&written.event.event_id, None);
        assert!(theirs.pending().is_empty());
        assert_eq!(entries(&elsewhere), ["keep.json"]);
    }

    #[test]
    fn events_the_shared_spool_set_aside_before_are_counted_and_pruned_where_they_are() {
        let (dir, spool, _, _) = planted();
        let legacy = spool.dir().join("rejected");
        fs::create_dir_all(&legacy).unwrap();
        let old = record("client-a", SystemTime::now()).event.event_id;
        fs::write(legacy.join(format!("{old}.json")), "old").unwrap();
        // A link among them, to a file outside.
        let target = dir.path().join("target.json");
        fs::write(&target, "kept").unwrap();
        let linked = record("client-a", SystemTime::now() + Duration::from_millis(5))
            .event
            .event_id;
        symlink(&target, legacy.join(format!("{linked}.json"))).unwrap();
        let mut both = vec![old.clone(), linked.clone()];
        both.sort();
        assert_eq!(spool.rejected(), both);
        // Only the shared spool reads them; a hook's spool sets nothing
        // aside and reads nothing set aside.
        assert!(Spool::new(spool.dir()).rejected().is_empty());
        assert!(Spool::of_environment(dir.path(), "client-a")
            .unwrap()
            .rejected()
            .is_empty());
        spool.prune(Duration::ZERO, SystemTime::now() + Duration::from_secs(60));
        assert!(spool.rejected().is_empty());
        assert!(entries(&legacy).is_empty());
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
    }

    #[test]
    fn a_folder_put_in_place_of_an_event_is_never_moved_into_the_set_aside_folder() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::of_environment(dir.path(), "client-a").unwrap();
        let written = record("client-a", SystemTime::now());
        let path = spool.write(&written).unwrap();
        let id = written.event.event_id.clone();
        // The daemon has read it; meanwhile the container puts a folder of
        // its own in its place, and keeps a handle on it.
        let held = spool.take(&id).unwrap();
        fs::remove_file(&path).unwrap();
        let staged = spool.dir().join("staged");
        fs::create_dir(&staged).unwrap();
        let handle = fs::File::open(&staged).unwrap();
        fs::rename(&staged, &path).unwrap();
        // Watch refuses the event.
        spool.reject(&id, Some(&held));
        let aside = dir.path().join("spools/.rejected");
        let name = format!("{id}@client-a.json");
        assert_eq!(entries(&aside), std::slice::from_ref(&name));
        let kept: Record =
            serde_json::from_str(&fs::read_to_string(aside.join(&name)).unwrap()).unwrap();
        assert_eq!(kept, held);
        // What the container writes through its handle stays in its own
        // folder.
        let planted = std::ffi::CString::new("planted").unwrap();
        // SAFETY: a descriptor the test owns, and a valid C string.
        let fd = unsafe {
            libc::openat(
                handle.as_raw_fd(),
                planted.as_ptr(),
                libc::O_CREAT | libc::O_WRONLY,
                0o600 as libc::c_uint,
            )
        };
        assert!(fd >= 0);
        // SAFETY: the descriptor just opened.
        unsafe { libc::close(fd) };
        assert_eq!(entries(&aside), std::slice::from_ref(&name));
        assert!(path.join("planted").exists());
        // A folder in its place, with nothing held, sets nothing aside.
        let other = record("client-a", SystemTime::now() + Duration::from_millis(3));
        fs::create_dir(spool.path(&other.event.event_id)).unwrap();
        spool.reject(&other.event.event_id, None);
        assert_eq!(entries(&aside), std::slice::from_ref(&name));
    }

    #[test]
    fn the_same_event_set_aside_from_two_folders_is_kept_twice_and_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Spool::shared(&dir.path().join("spool"), dir.path());
        let theirs = Spool::of_environment(dir.path(), "client-a").unwrap();
        let written = record("client-a", SystemTime::now());
        let id = written.event.event_id.clone();
        for spool in [&shared, &theirs] {
            spool.write(&written).unwrap();
            spool.reject(&id, None);
        }
        let aside = dir.path().join("spools/.rejected");
        assert_eq!(
            entries(&aside),
            [format!("{id}@_shared.json"), format!("{id}@client-a.json")]
        );
        // The same folder's event again, saying something else, leaves the
        // one already set aside as it was.
        let first = fs::read_to_string(aside.join(format!("{id}@client-a.json"))).unwrap();
        fs::write(theirs.path(&id), "{ not the same").unwrap();
        theirs.reject(&id, None);
        assert_eq!(
            fs::read_to_string(aside.join(format!("{id}@client-a.json"))).unwrap(),
            first
        );
        assert!(theirs.pending().is_empty());
        assert_eq!(shared.rejected(), std::slice::from_ref(&id));
    }

    #[test]
    fn a_hooks_spool_removes_what_it_would_set_aside() {
        let (dir, _, _, _) = planted();
        let spool = Spool::new(&dir.path().join("spool"));
        let written = record("client-a", SystemTime::now());
        spool.write(&written).unwrap();
        spool.reject(&written.event.event_id, None);
        assert!(spool.pending().is_empty());
        assert!(!dir.path().join("spool/rejected").exists());
        assert!(!dir.path().join("spools").exists());
    }

    #[test]
    fn a_pipe_is_refused_by_its_look_before_anything_opens_it() {
        let (_dir, spool, id, path) = planted();
        fifo(&path);
        let own = spool.own().unwrap();
        let entry = own.look(&file_name(&id)).unwrap();
        assert!(!entry.regular);
        let started = Instant::now();
        assert_eq!(
            Spool::read_looked(&own, &file_name(&id), &id, &entry),
            Err(Unread::NotAFile)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_file_swapped_in_between_the_look_and_the_open_is_refused() {
        let (dir, spool, id, path) = planted();
        let mut good = record("client-a", SystemTime::now());
        good.event.event_id = id.clone();
        let text = serde_json::to_string(&good).unwrap();
        fs::write(&path, &text).unwrap();
        let own = spool.own().unwrap();
        let name = file_name(&id);
        let looked = own.look(&name).unwrap();
        // A record just as good, renamed into its place after the look.
        let other = dir.path().join("spool").join("other");
        fs::write(&other, &text).unwrap();
        fs::rename(&other, &path).unwrap();
        assert_eq!(
            Spool::read_looked(&own, &name, &id, &looked),
            Err(Unread::NotAFile)
        );
        // A pipe swapped in is refused, without waiting for a writer.
        fs::remove_file(&path).unwrap();
        fifo(&path);
        let started = Instant::now();
        assert_eq!(
            Spool::read_looked(&own, &name, &id, &looked),
            Err(Unread::NotAFile)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        // Looked at afresh, the file in its place now reads.
        fs::remove_file(&path).unwrap();
        fs::write(&path, &text).unwrap();
        assert_eq!(spool.take(&id), Ok(good));
    }

    #[test]
    fn an_environments_spool_sets_events_aside_where_no_container_writes() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::of_environment(dir.path(), "client-a").unwrap();
        assert_eq!(spool.dir(), dir.path().join("spools/client-a"));
        let written = record("client-a", SystemTime::now());
        spool.write(&written).unwrap();
        spool.reject(&written.event.event_id, None);
        assert!(spool.pending().is_empty());
        assert_eq!(
            spool.rejected(),
            std::slice::from_ref(&written.event.event_id)
        );
        assert!(dir
            .path()
            .join("spools/.rejected")
            .join(format!("{}@client-a.json", written.event.event_id))
            .is_file());
        assert_eq!(
            Spool::shared(&dir.path().join("spool"), dir.path()).rejected(),
            std::slice::from_ref(&written.event.event_id)
        );
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
