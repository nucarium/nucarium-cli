//! The spool: one small file per event, between the hook that writes it and
//! the daemon that applies and sends it.
//!
//! A file is written to a temporary name and renamed, so the daemon never
//! reads half of one. File names are event ids, which sort by time, so the
//! spool is replayed in the order things happened.

use crate::envelope::{valid_event_id, Envelope};
use crate::paths;
use crate::process::Identity;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

#[derive(Debug, Clone)]
pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    pub fn new(dir: &Path) -> Spool {
        Spool {
            dir: dir.to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, event_id: &str) -> PathBuf {
        self.dir.join(format!("{event_id}.json"))
    }

    fn rejected_dir(&self) -> PathBuf {
        self.dir.join("rejected")
    }

    pub fn write(&self, record: &Record) -> Result<PathBuf> {
        let path = self.path(&record.event.event_id);
        paths::write_private(&path, serde_json::to_string(record)?.as_bytes())?;
        Ok(path)
    }

    fn ids_in(dir: &Path) -> Vec<String> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
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

    /// The ids of the events waiting, oldest first.
    pub fn pending(&self) -> Vec<String> {
        Self::ids_in(&self.dir)
    }

    pub fn rejected(&self) -> Vec<String> {
        Self::ids_in(&self.rejected_dir())
    }

    /// True once the spool holds `limit` files. Counting stops there, so this
    /// stays cheap for the hook however large the spool has grown.
    pub fn holds_at_least(&self, limit: usize) -> bool {
        fs::read_dir(&self.dir).is_ok_and(|entries| entries.take(limit).count() >= limit)
    }

    /// Read one event, or nothing if its file is not a valid record.
    pub fn read(&self, event_id: &str) -> Option<Record> {
        let text = fs::read_to_string(self.path(event_id)).ok()?;
        let record: Record = serde_json::from_str(&text).ok()?;
        (record.event.event_id == event_id && record.event.is_valid()).then_some(record)
    }

    pub fn remove(&self, event_id: &str) {
        let _ = fs::remove_file(self.path(event_id));
    }

    /// Set an event aside: Watch refused it, or its file is not a record.
    pub fn reject(&self, event_id: &str) {
        let moved = paths::create_private_dir(&self.rejected_dir())
            .context("creating the rejected folder")
            .and_then(|()| {
                fs::rename(
                    self.path(event_id),
                    self.rejected_dir().join(format!("{event_id}.json")),
                )
                .context("moving the event")
            });
        if moved.is_err() {
            self.remove(event_id);
        }
    }

    /// Delete refused events and abandoned temporary files older than `max_age`.
    pub fn prune(&self, max_age: Duration, now: SystemTime) {
        let old = |path: &Path| {
            fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > max_age)
        };
        for id in self.rejected() {
            let path = self.rejected_dir().join(format!("{id}.json"));
            if old(&path) {
                let _ = fs::remove_file(path);
            }
        }
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".tmp") && old(&entry.path()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}
