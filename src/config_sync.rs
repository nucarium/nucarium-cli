//! Config sync: an agent's settings, instructions and skills, pulled by the
//! box from a git repository its owner names, at one pinned branch.
//!
//! This file is the part of it the rest of the daemon touches: what is said
//! about it in a heartbeat, the small state file that is read for that, the
//! thread it runs on, and the `nucleated config sync` command. The work
//! itself, fetching, verifying, merging and writing, is in
//! [`sync`](crate::config_sync::sync).
//!
//! What leaves the box is three values per environment and nothing else: the
//! commit its config folder was last applied from, whether a synced file has
//! been changed by hand since, and one fixed word for how the sync stands.
//! The repository's address, its branch, every file's name and content, and
//! git's own words for an error all stay on the box. Content may rest in the
//! config folders and in the cache folder. The state folder holds commit ids,
//! yes-or-noes, times and fixed words.

use crate::envelope::{unix_ms, valid_commit_id, valid_error_kind};
use crate::housekeeping;
use crate::paths;
use crate::settings::{self, Environment, Settings};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

pub mod sync;

/// How often the thread passes. A pass is cheap unless a fetch is due: it
/// checks whether a synced file has moved, and refreshes `checked_at_ms`.
pub const PASS_EVERY: Duration = Duration::from_secs(30);
/// What the state file says about a hand change, or about how the sync
/// stands, is sent only while it is this fresh. A sync that has stopped
/// checking says nothing rather than something old.
pub const STALE_AFTER: Duration = Duration::from_secs(600);

/// How an environment's sync stands, in the one fixed word a heartbeat
/// carries. Why exactly is kept on the box, in [`Synced::detail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The config folder is at the newest commit the box has fetched.
    Ok,
    /// A commit is there to apply, and waits for the owner: nothing is
    /// applied to an environment for the first time until its owner has run
    /// a dry run that shows what would change.
    Pending,
    /// A newer commit is held back by a change made by hand, which the
    /// owner chose to keep.
    Held,
    /// The repository could not be fetched.
    Unreachable,
    /// The newest commit was refused: not signed by a named signer, not a
    /// descendant of the last, or holding something the sync will not write.
    Rejected,
    /// The commit was accepted and could not be written on the box.
    Failed,
}

impl State {
    pub const ALL: [State; 6] = [
        State::Ok,
        State::Pending,
        State::Held,
        State::Unreachable,
        State::Rejected,
        State::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Pending => "pending",
            State::Held => "held",
            State::Unreachable => "unreachable",
            State::Rejected => "rejected",
            State::Failed => "failed",
        }
    }

    pub fn parse(word: &str) -> Option<State> {
        State::ALL.into_iter().find(|state| state.as_str() == word)
    }
}

/// What is known about one environment's sync.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synced {
    /// The commit the config folder was last applied from, in full
    /// lower-case hex. Nothing until a commit has been applied.
    #[serde(default)]
    pub commit: Option<String>,
    /// Whether a file the sync wrote differs from that commit now.
    #[serde(default)]
    pub modified: Option<bool>,
    #[serde(default)]
    pub state: Option<State>,
    /// A finer fixed word for `nucleated status`, such as `unverified` or
    /// `fetch_failed`: lower-case letters, digits, `_`, `.` or `-`. It stays
    /// on the box, and it is never git's own text.
    #[serde(default)]
    pub detail: Option<String>,
    /// When the sync last looked at this environment.
    #[serde(default)]
    pub checked_at_ms: u64,
    #[serde(default)]
    pub applied_at_ms: Option<u64>,
}

/// What `config-sync.json` holds, by environment name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub environments: BTreeMap<String, Synced>,
}

pub fn status_file(state_dir: &Path) -> PathBuf {
    state_dir.join("config-sync.json")
}

/// What the sync last wrote down, or nothing when it is off or has written
/// nothing. One small file: the heartbeat reads it.
pub fn read_status(settings: &Settings) -> Option<Status> {
    settings.config_sync.as_ref()?;
    serde_json::from_str(&fs::read_to_string(status_file(&settings.state_dir)).ok()?).ok()
}

/// Write the state file. The sync calls this after every pass.
pub fn write_status(state_dir: &Path, status: &Status) -> Result<()> {
    paths::write_private(
        &status_file(state_dir),
        serde_json::to_string(status)?.as_bytes(),
    )
}

/// What a heartbeat and `nucleated status` say about one environment's
/// config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub commit: Option<String>,
    pub modified: Option<bool>,
    pub state: Option<State>,
    /// For the local view only.
    pub detail: Option<String>,
}

/// Read one environment's entry as a report, holding every value to its
/// shape: a commit that is not a commit id is no commit, a hand-change flag
/// goes only with a commit, and neither the flag nor the state is passed on
/// from a check older than [`STALE_AFTER`].
pub fn report(status: Option<&Status>, environment: &str, now: SystemTime) -> Report {
    let Some(synced) = status.and_then(|status| status.environments.get(environment)) else {
        return Report::default();
    };
    let commit = synced.commit.clone().filter(|id| valid_commit_id(id));
    let fresh = unix_ms(now)
        .checked_sub(synced.checked_at_ms)
        .is_some_and(|age| age < STALE_AFTER.as_millis() as u64);
    Report {
        modified: synced.modified.filter(|_| fresh && commit.is_some()),
        state: synced.state.filter(|_| fresh),
        detail: synced
            .detail
            .clone()
            .filter(|word| fresh && valid_error_kind(word)),
        commit,
    }
}

/// The environments a sync applies to: each one that is one login's config
/// folder. An environment watched through a driver is not one, and neither
/// is one whose config names no folder.
pub fn synced_environments(settings: &Settings) -> Vec<&Environment> {
    settings
        .environments
        .iter()
        .filter(|environment| environment.driver.is_none() && environment.config_dir.is_some())
        .collect()
}

/// The config-sync thread: a little after start, then every [`PASS_EVERY`],
/// one pass. The config is read afresh each time, as housekeeping reads it.
/// With no `[config_sync]` table nothing runs, and what an earlier sync wrote
/// down stops being reported.
pub fn run_schedule(config: &Path, stop: &AtomicBool) {
    if housekeeping::wait(stop, Duration::from_secs(20)) {
        return;
    }
    loop {
        if let Ok(settings) = settings::load_from(config) {
            match &settings.config_sync {
                Some(config_sync) => {
                    // A pass that panics has done nothing more. It costs this
                    // pass and never the daemon.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        sync::pass(&settings, config_sync, SystemTime::now());
                    }));
                }
                None => {
                    let _ = fs::remove_file(status_file(&settings.state_dir));
                }
            }
        }
        if housekeeping::wait(stop, PASS_EVERY) {
            return;
        }
    }
}

/// `nucleated config sync [--env NAME] [--dry-run]`: one pass now, for one
/// environment or for all of them. With `--dry-run` it prints what would
/// change and changes nothing; that is also what lets a first apply go
/// ahead. Returns the report and whether nothing in it failed.
pub fn run_now(settings: &Settings, env: Option<&str>, dry_run: bool) -> Result<(Value, bool)> {
    let config_sync = settings.config_sync.as_ref().ok_or_else(|| {
        anyhow!(
            "config sync is off: {} has no [config_sync] table",
            settings.path.display()
        )
    })?;
    let all = synced_environments(settings);
    let environments = match env {
        None => all,
        Some(name) => {
            if settings.environment(name).is_none() {
                bail!("no environment named {name} in {}", settings.path.display());
            }
            let chosen: Vec<_> = all.into_iter().filter(|e| e.name == name).collect();
            if chosen.is_empty() {
                bail!("environment {name} has no config_dir to sync into");
            }
            chosen
        }
    };
    sync::run_now(settings, config_sync, &environments, dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn status(synced: Synced) -> Status {
        Status {
            environments: BTreeMap::from([("client-a".to_string(), synced)]),
        }
    }

    #[test]
    fn a_report_is_the_commit_the_flag_and_one_fixed_word() {
        let now = UNIX_EPOCH + Duration::from_millis(1_795_089_600_000);
        let commit = "a1".repeat(20);
        let known = status(Synced {
            commit: Some(commit.clone()),
            modified: Some(true),
            state: Some(State::Rejected),
            detail: Some("unverified".into()),
            checked_at_ms: unix_ms(now) - 30_000,
            applied_at_ms: Some(1),
        });
        assert_eq!(
            report(Some(&known), "client-a", now),
            Report {
                commit: Some(commit),
                modified: Some(true),
                state: Some(State::Rejected),
                detail: Some("unverified".into()),
            }
        );
        assert_eq!(report(Some(&known), "client-b", now), Report::default());
        assert_eq!(report(None, "client-a", now), Report::default());
    }

    #[test]
    fn what_a_sync_that_stopped_checking_last_said_is_not_repeated() {
        let now = UNIX_EPOCH + Duration::from_millis(1_795_089_600_000);
        let commit = "a1".repeat(20);
        let old = status(Synced {
            commit: Some(commit.clone()),
            modified: Some(false),
            state: Some(State::Ok),
            checked_at_ms: unix_ms(now) - STALE_AFTER.as_millis() as u64,
            ..Synced::default()
        });
        let said = report(Some(&old), "client-a", now);
        // What was applied is still what is on disk, as far as anyone knows.
        assert_eq!(said.commit, Some(commit));
        assert_eq!(said.modified, None);
        assert_eq!(said.state, None);
        // A check dated ahead of the clock is from before the clock moved.
        let ahead = status(Synced {
            state: Some(State::Ok),
            checked_at_ms: unix_ms(now) + 60_000,
            ..Synced::default()
        });
        assert_eq!(report(Some(&ahead), "client-a", now).state, None);
    }

    #[test]
    fn words_where_a_commit_or_a_kind_belongs_are_dropped() {
        let now = UNIX_EPOCH + Duration::from_millis(1_795_089_600_000);
        let wordy = status(Synced {
            commit: Some("the commit that fixed the login page".into()),
            modified: Some(true),
            state: Some(State::Pending),
            detail: Some("git said: fatal: could not read from remote".into()),
            checked_at_ms: unix_ms(now),
            applied_at_ms: None,
        });
        let said = report(Some(&wordy), "client-a", now);
        assert_eq!(said.commit, None);
        // A hand change is a change from a commit, so it goes with one.
        assert_eq!(said.modified, None);
        assert_eq!(said.state, Some(State::Pending));
        assert_eq!(said.detail, None);
        // A state that is not one of the fixed words is no state file at all.
        let text = r#"{"environments":{"client-a":{"state":"it went badly"}}}"#;
        assert!(serde_json::from_str::<Status>(text).is_err());
    }

    #[test]
    fn every_state_has_one_word_and_reads_back_from_it() {
        for state in State::ALL {
            assert_eq!(State::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
        assert_eq!(State::parse("it went badly"), None);
    }
}
