//! The work of a config sync: fetch the pinned branch, verify its tip, read
//! the allowlisted paths from the tree, and write them into each
//! environment's config folder.
//!
//! Everything that knows what an agent's config folder looks like is in this
//! file; what git is asked is in [`repo`], and how the owner's settings are
//! laid under the entries `nucleate` and `nucleated` own is in [`merge`].
//!
//! Where things rest. The config folders hold what the owner's repository
//! says they hold. The cache folder holds the clone, a note per environment
//! of the files a sync wrote (with their names), the owner's settings as
//! last applied, and one copy of what the last sync replaced. The state
//! folder holds `config-sync.json` and nothing else of this feature: commit
//! ids, yes-or-noes, times and words from the fixed list in [`Why`].
//!
//! What is never said. The repository's address and branch, a file's name
//! or content, and git's own words for a failure are not logged, not put in
//! the state folder and not sent. A log line says an environment's name and
//! fixed words.

use super::{read_status, write_status, State, Status, Synced};
use crate::envelope::{unix_ms, valid_commit_id};
use crate::paths;
use crate::settings::{ConfigSync, Environment, OnDrift, Settings};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod merge;
mod repo;

use repo::{Entry, FetchFailure, Repo};

/// The most files one environment's config may come to, the most one file
/// may hold, and the most they may hold together.
const MAX_FILES: usize = 2_000;
const MAX_FILE: u64 = 1024 * 1024;
const MAX_TOTAL: u64 = 20 * 1024 * 1024;
/// A fetch that failed, or a commit that could not be written, is tried
/// again this soon, or at the sync's own interval when that is sooner.
const RETRY: Duration = Duration::from_secs(300);
/// How long `nucleated config sync` waits for a pass that is under way.
const WAIT_FOR_PASS: Duration = Duration::from_secs(60);

/// What one agent's config folder looks like: the only names a sync reads
/// from the repository and writes under the folder. An agent with no entry
/// here is not synced. The repository uses the same names.
struct Layout {
    /// The settings file. Merged, never copied.
    settings: &'static str,
    /// The instructions file.
    instructions: &'static str,
    /// The folder of skills, one folder each.
    skills: &'static str,
    /// The variable that names a login's config folder. The owner's
    /// settings may not set it.
    config_dir_var: &'static str,
    /// Skill folder names that are the vendor's own and never synced.
    reserved_skill: fn(&str) -> bool,
}

/// Claude Code, from <https://code.claude.com/docs/en/claude-directory> and
/// <https://code.claude.com/docs/en/skills>: `settings.json`, `CLAUDE.md`
/// and `skills/<name>/` are files a person authors in the user scope. A
/// skill folder named `synced` is the vendor's own sync from claude.ai,
/// names starting `anthropic-skills` are the vendor's, and a name starting
/// with `.` (`.trash`) is the vendor's bookkeeping.
const CLAUDE_CODE: Layout = Layout {
    settings: "settings.json",
    instructions: "CLAUDE.md",
    skills: "skills",
    config_dir_var: "CLAUDE_CONFIG_DIR",
    reserved_skill: |name| {
        name.starts_with('.')
            || name.eq_ignore_ascii_case("synced")
            || name.to_ascii_lowercase().starts_with("anthropic-skills")
    },
};

fn layout(agent: &str) -> Option<&'static Layout> {
    match agent {
        "claude-code" => Some(&CLAUDE_CODE),
        _ => None,
    }
}

/// Why a sync stopped, as the finer fixed word `nucleated status` shows.
/// Each belongs to one of the words a heartbeat carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    FetchFailed,
    NoGit,
    FetchTimeout,
    AwaitingDryRun,
    HandChange,
    NoSigners,
    Unverified,
    NotDescendant,
    InvalidSettings,
    ForbiddenKey,
    UnsafeEntry,
    TooLarge,
    ReadFailed,
    NoConfigDir,
    SettingsUnreadable,
    UnsafeDestination,
    WriteFailed,
}

impl Why {
    const ALL: [Why; 17] = [
        Why::FetchFailed,
        Why::NoGit,
        Why::FetchTimeout,
        Why::AwaitingDryRun,
        Why::HandChange,
        Why::NoSigners,
        Why::Unverified,
        Why::NotDescendant,
        Why::InvalidSettings,
        Why::ForbiddenKey,
        Why::UnsafeEntry,
        Why::TooLarge,
        Why::ReadFailed,
        Why::NoConfigDir,
        Why::SettingsUnreadable,
        Why::UnsafeDestination,
        Why::WriteFailed,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Why::FetchFailed => "fetch_failed",
            Why::NoGit => "no_git",
            Why::FetchTimeout => "fetch_timeout",
            Why::AwaitingDryRun => "awaiting_dry_run",
            Why::HandChange => "hand_change",
            Why::NoSigners => "no_signers",
            Why::Unverified => "unverified",
            Why::NotDescendant => "not_descendant",
            Why::InvalidSettings => "invalid_settings",
            Why::ForbiddenKey => "forbidden_key",
            Why::UnsafeEntry => "unsafe_entry",
            Why::TooLarge => "too_large",
            Why::ReadFailed => "read_failed",
            Why::NoConfigDir => "no_config_dir",
            Why::SettingsUnreadable => "settings_unreadable",
            Why::UnsafeDestination => "unsafe_destination",
            Why::WriteFailed => "write_failed",
        }
    }

    fn parse(word: &str) -> Option<Why> {
        Why::ALL.into_iter().find(|why| why.as_str() == word)
    }

    fn state(self) -> State {
        match self {
            Why::FetchFailed | Why::NoGit | Why::FetchTimeout => State::Unreachable,
            Why::AwaitingDryRun => State::Pending,
            Why::HandChange => State::Held,
            Why::NoSigners
            | Why::Unverified
            | Why::NotDescendant
            | Why::InvalidSettings
            | Why::ForbiddenKey
            | Why::UnsafeEntry
            | Why::TooLarge => State::Rejected,
            Why::ReadFailed
            | Why::NoConfigDir
            | Why::SettingsUnreadable
            | Why::UnsafeDestination
            | Why::WriteFailed => State::Failed,
        }
    }
}

/// Which repository and branch something in the cache came from. Kept in
/// the cache folder only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Source {
    repository: String,
    branch: String,
}

/// What the last fetch found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Fetched {
    #[serde(default)]
    source: Source,
    #[serde(default)]
    attempted_at_ms: u64,
    #[serde(default)]
    ok: bool,
    /// For a fetch that failed, which way: one of the fixed words of
    /// [`Why`] that stand for an unreachable repository.
    #[serde(default)]
    failure: Option<String>,
    #[serde(default)]
    tip: Option<String>,
}

/// What a file looked like when it was last known to be right, or wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stat {
    size: u64,
    mtime_s: u64,
    mtime_ns: u32,
    executable: bool,
}

/// One file a sync wrote.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Written {
    /// Git's id for what was written.
    id: String,
    stat: Stat,
    /// Set when the file was last found changed, so that it is not read
    /// again until it changes again.
    #[serde(default)]
    dirty: Option<Stat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SettingsKept {
    #[serde(default)]
    stat: Option<Stat>,
    #[serde(default)]
    modified: bool,
}

/// What the signature check went by: whether one was asked for, and the
/// signers file as it was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Signers {
    #[serde(default)]
    required: bool,
    #[serde(default)]
    file: Option<Stat>,
}

/// A commit that could not be applied, so it is not tried at every pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stuck {
    commit: String,
    detail: String,
    #[serde(default)]
    at_ms: u64,
    #[serde(default)]
    signers: Signers,
}

/// What is kept about one environment, in the cache folder.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    source: Source,
    /// The owner has run a dry run for this environment and this
    /// repository. Nothing is applied to it before.
    #[serde(default)]
    approved: bool,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    applied_at_ms: Option<u64>,
    /// The files a sync wrote, by their path under the config folder.
    #[serde(default)]
    files: BTreeMap<String, Written>,
    /// Set while the settings file is one the sync writes.
    #[serde(default)]
    settings: Option<SettingsKept>,
    #[serde(default)]
    stuck: Option<Stuck>,
}

impl Kept {
    fn file(dir: &Path) -> PathBuf {
        dir.join("sync.json")
    }

    fn load(dir: &Path) -> Kept {
        fs::read_to_string(Kept::file(dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, dir: &Path) -> bool {
        serde_json::to_vec(self)
            .ok()
            .is_some_and(|bytes| paths::write_private(&Kept::file(dir), &bytes).is_ok())
    }

    /// The reason this commit was last stopped for, while that reason can
    /// still hold: a refusal until the signers change, a failure to write
    /// until it is time to try again.
    fn still_stuck(&self, commit: &str, signers: &Signers, now_ms: u64) -> Option<Why> {
        let stuck = self.stuck.as_ref().filter(|stuck| stuck.commit == commit)?;
        let why = Why::parse(&stuck.detail)?;
        let holds = match why.state() {
            State::Rejected => stuck.signers == *signers,
            _ => now_ms
                .checked_sub(stuck.at_ms)
                .is_some_and(|age| age < RETRY.as_millis() as u64),
        };
        holds.then_some(why)
    }
}

/// One file a commit says the config folder holds.
#[derive(Debug, Clone)]
struct Planned {
    id: String,
    size: u64,
    executable: bool,
}

/// What a commit says one environment's config folder holds.
struct Plan {
    files: BTreeMap<String, Planned>,
    /// The owner's settings, checked and with nothing owned in them.
    settings: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Add,
    Update,
    Remove,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Add => "add",
            Kind::Update => "update",
            Kind::Remove => "remove",
        }
    }
}

#[derive(Debug, Clone)]
struct Change {
    path: String,
    kind: Kind,
}

/// A plan held against the folder as it is: what would be written, what is
/// right already, what would go.
struct Prepared {
    plan: Plan,
    changes: Vec<Change>,
    /// Files that are already what the commit says, as they are on disk.
    right: BTreeMap<String, Stat>,
    /// The settings file as it would be written, and whether that differs
    /// from what is there.
    settings: Option<(Value, bool)>,
}

/// The parts of a tree a sync may write, as found at the repository's root
/// or under one environment's own folder.
#[derive(Default)]
struct Picked {
    settings: Option<Entry>,
    instructions: Option<Entry>,
    /// By skill name, then by the file's path under the config folder.
    skills: BTreeMap<String, BTreeMap<String, Entry>>,
}

impl Picked {
    /// Take an entry if its path is on the allowlist. Anything else is
    /// passed over unread. What is on the allowlist and is not a plain file
    /// with a plain name refuses the whole commit.
    fn take(&mut self, layout: &Layout, path: &str, entry: &Entry) -> Result<(), Why> {
        let skill = if path == layout.settings || path == layout.instructions {
            None
        } else if let Some(inside) = path
            .strip_prefix(layout.skills)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            let Some((name, _)) = inside.split_once('/') else {
                // A file beside the skill folders is not a skill. A link or
                // a submodule there could become one.
                return if entry.is_file() {
                    Ok(())
                } else {
                    Err(Why::UnsafeEntry)
                };
            };
            if (layout.reserved_skill)(name) {
                return Ok(());
            }
            Some(name)
        } else {
            return Ok(());
        };
        if !entry.is_file() || !plain_path(path) {
            return Err(Why::UnsafeEntry);
        }
        let mut entry = entry.clone();
        entry.path = path.to_string();
        match skill {
            Some(name) => {
                self.skills
                    .entry(name.to_string())
                    .or_default()
                    .insert(path.to_string(), entry);
            }
            None if path == layout.settings => self.settings = Some(entry),
            None => self.instructions = Some(entry),
        }
        Ok(())
    }
}

/// A path whose every part is an ordinary name.
fn plain_path(path: &str) -> bool {
    path.len() <= 1024
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part
                    .chars()
                    .any(|c| c.is_control() || c == '\\' || c == char::REPLACEMENT_CHARACTER)
        })
}

fn stat(path: &Path) -> Option<Stat> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    #[cfg(unix)]
    let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
    #[cfg(not(unix))]
    let executable = false;
    Some(Stat {
        size: meta.len(),
        mtime_s: modified.as_secs(),
        mtime_ns: modified.subsec_nanos(),
        executable,
    })
}

/// Where a path under the config folder lands, if getting there passes
/// through no symbolic link and nothing stands where a file or a folder
/// should.
fn destination(config_dir: &Path, path: &str) -> Result<PathBuf, Why> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut at = config_dir.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        at.push(part);
        let last = index + 1 == parts.len();
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(Why::UnsafeDestination),
            Ok(meta) if last && !meta.is_file() => return Err(Why::UnsafeDestination),
            Ok(meta) if !last && !meta.is_dir() => return Err(Why::UnsafeDestination),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(Why::UnsafeDestination),
        }
    }
    Ok(config_dir.join(path))
}

/// The settings file on the box, if there is one. An error when it is there
/// and cannot be read as settings: the entries in it that are owned could
/// then not be carried over.
fn settings_on_disk(file: &Path) -> Result<Option<Value>, Why> {
    match fs::symlink_metadata(file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(meta) if meta.file_type().is_file() && meta.len() <= 4 * MAX_FILE => {}
        _ => return Err(Why::SettingsUnreadable),
    }
    let bytes = fs::read(file).map_err(|_| Why::SettingsUnreadable)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| Why::SettingsUnreadable)
}

/// Write a file whole: beside its place, then moved in.
fn write_file(file: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = file.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(dir)?;
    let name = file
        .file_name()
        .ok_or(std::io::ErrorKind::InvalidInput)?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.nucleated-{}.tmp", std::process::id()));
    let written = (|| {
        let mut out = fs::File::create(&tmp)?;
        out.write_all(bytes)?;
        #[cfg(unix)]
        out.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        #[cfg(not(unix))]
        let _ = mode;
        out.sync_all()?;
        fs::rename(&tmp, file)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Held while a sync works, so the daemon's pass and a sync run by hand
/// never write the same folder at once. It lets go when dropped, and when
/// the process holding it dies.
struct Lock(fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        // Let go by name, not only by closing: a program another thread is
        // starting at this moment holds a copy of the open file for an
        // instant, and the lock would outlive this one with it.
        #[cfg(unix)]
        // SAFETY: as in `take`.
        unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&self.0),
                libc::LOCK_UN,
            );
        }
        #[cfg(not(unix))]
        let _ = &self.0;
    }
}

impl Lock {
    fn take(root: &Path, wait: Duration) -> Option<Lock> {
        paths::create_private_dir(root).ok()?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join("lock"))
            .ok()?;
        let deadline = Instant::now() + wait;
        loop {
            #[cfg(unix)]
            // SAFETY: `flock` takes no pointers, and the descriptor is this
            // open file's for as long as `file` lives.
            let held = unsafe {
                libc::flock(
                    std::os::unix::io::AsRawFd::as_raw_fd(&file),
                    libc::LOCK_EX | libc::LOCK_NB,
                ) == 0
            };
            #[cfg(not(unix))]
            let held = true;
            if held {
                return Some(Lock(file));
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The daemon's own pass.
    Scheduled,
    /// `nucleated config sync --dry-run`.
    Dry,
    /// `nucleated config sync`.
    Now,
}

/// How one environment came out of a pass.
struct Outcome {
    why: Option<Why>,
    /// Nothing is known yet: no fetch has been made.
    unknown: bool,
    action: &'static str,
    changes: Vec<Change>,
    applied: bool,
}

impl Outcome {
    fn stopped(why: Why) -> Outcome {
        Outcome {
            why: Some(why),
            unknown: false,
            action: why.state().as_str(),
            changes: Vec::new(),
            applied: false,
        }
    }

    fn fine(action: &'static str, changes: Vec<Change>, applied: bool) -> Outcome {
        Outcome {
            why: None,
            unknown: false,
            action,
            changes,
            applied,
        }
    }

    fn state(&self) -> Option<State> {
        match self.why {
            Some(why) => Some(why.state()),
            None if self.unknown => None,
            None => Some(State::Ok),
        }
    }
}

/// One pass over the environments, by the daemon or by hand.
struct Run<'a> {
    config_sync: &'a ConfigSync,
    root: PathBuf,
    repo: Repo,
    source: Source,
    fetched: Fetched,
    signers: Signers,
    now_ms: u64,
    /// Commits whose signature has been checked in this pass.
    verified: HashMap<String, bool>,
}

impl<'a> Run<'a> {
    fn open(settings: &Settings, config_sync: &'a ConfigSync, now: SystemTime) -> Run<'a> {
        let root = settings.cache_dir.join("config-sync");
        let repo = Repo::at(root.join("repo.git"));
        let source = Source {
            repository: config_sync.repository.clone(),
            branch: config_sync.branch.clone(),
        };
        let mut fetched: Fetched = fs::read_to_string(root.join("fetch.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if fetched.source != source {
            // Another repository, or another branch of it: what was fetched
            // before says nothing about this one.
            repo.forget();
            fetched = Fetched {
                source: source.clone(),
                ..Fetched::default()
            };
        }
        let signers = Signers {
            required: config_sync.require_signed,
            file: config_sync
                .allowed_signers
                .as_deref()
                .filter(|_| config_sync.require_signed)
                .and_then(stat),
        };
        Run {
            config_sync,
            root,
            repo,
            source,
            fetched,
            signers,
            now_ms: unix_ms(now),
            verified: HashMap::new(),
        }
    }

    fn fetch_due(&self) -> bool {
        let Some(interval) = self.config_sync.interval else {
            return false;
        };
        if self.fetched.attempted_at_ms == 0 {
            return true;
        }
        let every = if self.fetched.ok {
            interval
        } else {
            interval.min(RETRY)
        };
        self.now_ms
            .checked_sub(self.fetched.attempted_at_ms)
            .is_none_or(|age| age >= every.as_millis() as u64)
    }

    fn fetch(&mut self) {
        let failure = self
            .repo
            .fetch(&self.source.repository, &self.source.branch)
            .err()
            .map(|failure| match failure {
                FetchFailure::NoGit => Why::NoGit,
                FetchFailure::Late => Why::FetchTimeout,
                FetchFailure::Refused => Why::FetchFailed,
            });
        self.fetched.ok = failure.is_none();
        self.fetched.attempted_at_ms = self.now_ms.max(1);
        if self.fetched.ok {
            self.fetched.tip = self.repo.tip(&self.source.branch);
            self.fetched.ok = self.fetched.tip.is_some();
        }
        self.fetched.failure =
            (!self.fetched.ok).then(|| failure.unwrap_or(Why::FetchFailed).as_str().to_string());
        if let Ok(bytes) = serde_json::to_vec(&self.fetched) {
            let _ = paths::write_private(&self.root.join("fetch.json"), &bytes);
        }
    }

    fn environment_dir(&self, environment: &Environment) -> PathBuf {
        self.root.join("env").join(&environment.name)
    }

    /// What a commit says this environment's config folder holds: the
    /// repository's root, with the environment's own folder laid over it.
    fn plan(&self, layout: &Layout, environment: &Environment, commit: &str) -> Result<Plan, Why> {
        let own = format!("environments/{}", environment.name);
        let entries = self
            .repo
            .tree(
                commit,
                &[layout.settings, layout.instructions, layout.skills, &own],
            )
            .ok_or(Why::ReadFailed)?;
        let own = format!("{own}/");
        let (mut root, mut over) = (Picked::default(), Picked::default());
        for entry in &entries {
            match entry.path.strip_prefix(&own) {
                Some(path) => over.take(layout, path, entry)?,
                None => root.take(layout, &entry.path, entry)?,
            }
        }

        let mut files = BTreeMap::new();
        let mut total = 0u64;
        let mut add = |entry: Entry| -> Result<(), Why> {
            let size = entry.size.unwrap_or(0);
            total += size;
            if size > MAX_FILE || total > MAX_TOTAL {
                return Err(Why::TooLarge);
            }
            files.insert(
                entry.path.clone(),
                Planned {
                    executable: entry.executable(),
                    id: entry.id,
                    size,
                },
            );
            Ok(())
        };
        if let Some(entry) = over.instructions.or(root.instructions) {
            add(entry)?;
        }
        // A skill the environment has of its own replaces the root's whole.
        for name in over.skills.keys() {
            root.skills.remove(name);
        }
        for entry in root
            .skills
            .into_values()
            .chain(over.skills.into_values())
            .flatten()
            .map(|(_, entry)| entry)
        {
            add(entry)?;
        }
        if files.len() > MAX_FILES {
            return Err(Why::TooLarge);
        }

        let read = |entry: Option<Entry>| -> Result<Option<Value>, Why> {
            let Some(entry) = entry else {
                return Ok(None);
            };
            if entry.size.unwrap_or(0) > MAX_FILE {
                return Err(Why::TooLarge);
            }
            let bytes = self.repo.blob(&entry.id, MAX_FILE).ok_or(Why::ReadFailed)?;
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| Why::InvalidSettings)
        };
        let settings = match (read(root.settings)?, read(over.settings)?) {
            (Some(base), Some(over)) => Some(merge::overlay(base, over)),
            (base, over) => over.or(base),
        };
        let settings = settings
            .map(|settings| {
                let ours = merge::Ours::on_this_box();
                merge::owners(&ours, settings, layout.config_dir_var).map_err(|refused| {
                    match refused {
                        merge::Refused::Invalid => Why::InvalidSettings,
                        merge::Refused::ForbiddenKey => Why::ForbiddenKey,
                    }
                })
            })
            .transpose()?;
        Ok(Plan { files, settings })
    }

    /// Check a commit, read what it holds for one environment, and hold
    /// that against the config folder as it is. Nothing is written.
    fn prepare(
        &mut self,
        layout: &Layout,
        environment: &Environment,
        config_dir: &Path,
        kept: &Kept,
        commit: &str,
    ) -> Result<Prepared, Why> {
        if self.config_sync.require_signed {
            let signers = self
                .config_sync
                .allowed_signers
                .as_deref()
                .filter(|_| self.signers.file.is_some())
                .ok_or(Why::NoSigners)?;
            let repo = &self.repo;
            let verified = *self
                .verified
                .entry(commit.to_string())
                .or_insert_with(|| repo.verified(commit, signers));
            if !verified {
                return Err(Why::Unverified);
            }
        }
        // An older commit, however well signed, is not a newer one.
        if let Some(applied) = kept.commit.as_deref().filter(|applied| *applied != commit) {
            if !self.repo.descends(commit, applied) {
                return Err(Why::NotDescendant);
            }
        }
        let plan = self.plan(layout, environment, commit)?;
        if !config_dir.is_dir() {
            return Err(Why::NoConfigDir);
        }

        let mut changes = Vec::new();
        let mut right = BTreeMap::new();
        let mut unsure: Vec<(&String, PathBuf, Stat)> = Vec::new();
        for (path, planned) in &plan.files {
            let file = destination(config_dir, path)?;
            match stat(&file) {
                None => changes.push(Change {
                    path: path.clone(),
                    kind: Kind::Add,
                }),
                Some(now)
                    if kept
                        .files
                        .get(path)
                        .is_some_and(|written| written.id == planned.id && written.stat == now)
                        && now.executable == planned.executable =>
                {
                    right.insert(path.clone(), now);
                }
                Some(now) => unsure.push((path, file, now)),
            }
        }
        if !unsure.is_empty() {
            let files: Vec<PathBuf> = unsure.iter().map(|(_, file, _)| file.clone()).collect();
            let ids = self.repo.ids_of(&files).ok_or(Why::ReadFailed)?;
            for ((path, _, now), id) in unsure.into_iter().zip(ids) {
                let planned = &plan.files[path];
                if id == planned.id && now.executable == planned.executable {
                    right.insert(path.clone(), now);
                } else {
                    changes.push(Change {
                        path: path.clone(),
                        kind: Kind::Update,
                    });
                }
            }
        }
        // Only a file an earlier sync wrote is ever removed.
        for path in kept
            .files
            .keys()
            .filter(|path| !plan.files.contains_key(*path))
        {
            if stat(&destination(config_dir, path)?).is_some() {
                changes.push(Change {
                    path: path.clone(),
                    kind: Kind::Remove,
                });
            }
        }
        // The settings file is the sync's while the repository has one, and
        // once more after it stops having one, to take the owner's out.
        let settings = if plan.settings.is_some() || kept.settings.is_some() {
            let file =
                destination(config_dir, layout.settings).map_err(|_| Why::UnsafeDestination)?;
            let current = settings_on_disk(&file)?;
            let owner = plan.settings.clone().unwrap_or_else(|| json!({}));
            let merged = merge::merged(&merge::Ours::on_this_box(), &owner, current.as_ref());
            let differs = current.as_ref() != Some(&merged);
            if differs {
                changes.push(Change {
                    path: layout.settings.to_string(),
                    kind: if current.is_some() {
                        Kind::Update
                    } else {
                        Kind::Add
                    },
                });
            }
            Some((merged, differs))
        } else {
            None
        };
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Prepared {
            plan,
            changes,
            right,
            settings,
        })
    }

    /// Make the config folder what the commit says. One copy of every file
    /// this replaces or removes is kept in the cache folder first.
    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        layout: &Layout,
        config_dir: &Path,
        dir: &Path,
        kept: &mut Kept,
        commit: &str,
        prepared: Prepared,
    ) -> Result<(), Why> {
        let Prepared {
            plan,
            changes,
            right,
            settings,
        } = prepared;
        // Everything to be written is read first, so a commit that cannot
        // be read whole writes nothing.
        let mut contents: Vec<(&Change, Vec<u8>)> = Vec::new();
        for change in changes.iter().filter(|change| change.kind != Kind::Remove) {
            let Some(planned) = plan.files.get(&change.path) else {
                continue;
            };
            let bytes = self
                .repo
                .blob(&planned.id, MAX_FILE)
                .filter(|bytes| bytes.len() as u64 == planned.size)
                .ok_or(Why::ReadFailed)?;
            contents.push((change, bytes));
        }

        let replaced: Vec<&Change> = changes
            .iter()
            .filter(|change| change.kind != Kind::Add)
            .collect();
        if !replaced.is_empty() {
            let copy = dir.join("replaced");
            let _ = fs::remove_dir_all(&copy);
            for change in &replaced {
                let to = copy.join(&change.path);
                let made = to
                    .parent()
                    .is_some_and(|parent| paths::create_private_dir(parent).is_ok());
                if !made || fs::copy(config_dir.join(&change.path), &to).is_err() {
                    return Err(Why::WriteFailed);
                }
            }
        }

        let mut files: BTreeMap<String, Written> = BTreeMap::new();
        let known = |path: &str, now: Stat| Written {
            id: plan.files[path].id.clone(),
            stat: now,
            dirty: None,
        };
        for (path, now) in &right {
            files.insert(path.clone(), known(path, *now));
        }
        for (change, bytes) in &contents {
            let file = destination(config_dir, &change.path)?;
            let mode = if plan.files[&change.path].executable {
                0o755
            } else {
                0o644
            };
            write_file(&file, bytes, mode).map_err(|_| Why::WriteFailed)?;
            let now = stat(&file).ok_or(Why::WriteFailed)?;
            files.insert(change.path.clone(), known(&change.path, now));
        }
        for change in changes.iter().filter(|change| change.kind == Kind::Remove) {
            let file = destination(config_dir, &change.path)?;
            fs::remove_file(&file).map_err(|_| Why::WriteFailed)?;
            // A skill's folder that is left empty goes too; the folder of
            // skills itself stays.
            let mut folder = file.parent();
            let skills = config_dir.join(layout.skills);
            while let Some(empty) =
                folder.filter(|folder| folder.starts_with(&skills) && *folder != skills)
            {
                if fs::remove_dir(empty).is_err() {
                    break;
                }
                folder = empty.parent();
            }
        }

        // The settings file last: it is what turns the rest on.
        let owners = dir.join("owner-settings.json");
        match &plan.settings {
            Some(owner) => paths::write_private(&owners, merge::text(owner).as_bytes())
                .map_err(|_| Why::WriteFailed)?,
            None => {
                let _ = fs::remove_file(&owners);
            }
        }
        let file = config_dir.join(layout.settings);
        if let Some((merged, true)) = &settings {
            // A file that is already there keeps who may read it.
            let mode = paths::mode(&file).unwrap_or(0o600);
            write_file(&file, merge::text(merged).as_bytes(), mode)
                .map_err(|_| Why::WriteFailed)?;
        }

        kept.commit = Some(commit.to_string());
        kept.applied_at_ms = Some(self.now_ms);
        kept.files = files;
        kept.settings = plan.settings.as_ref().map(|_| SettingsKept {
            stat: stat(&file),
            modified: false,
        });
        kept.stuck = None;
        if kept.save(dir) {
            Ok(())
        } else {
            Err(Why::WriteFailed)
        }
    }

    /// Whether a file the sync wrote differs from the commit it was applied
    /// from. One `lstat` per file; a file is read only when that has moved.
    /// Returns the answer and whether what is kept changed.
    fn drift(
        &self,
        layout: &Layout,
        config_dir: &Path,
        dir: &Path,
        kept: &mut Kept,
    ) -> (bool, bool) {
        let (mut modified, mut changed) = (false, false);
        let mut moved: Vec<(String, PathBuf, Stat)> = Vec::new();
        for (path, written) in &kept.files {
            let file = destination(config_dir, path).ok();
            match file.as_deref().and_then(stat) {
                None => modified = true,
                Some(now) if now == written.stat => {}
                Some(now) if Some(now) == written.dirty => modified = true,
                Some(now) => moved.push((path.clone(), file.unwrap_or_default(), now)),
            }
        }
        if !moved.is_empty() {
            let files: Vec<PathBuf> = moved.iter().map(|(_, file, _)| file.clone()).collect();
            match self.repo.ids_of(&files) {
                Some(ids) => {
                    for ((path, _, now), id) in moved.into_iter().zip(ids) {
                        let Some(written) = kept.files.get_mut(&path) else {
                            continue;
                        };
                        if id == written.id && now.executable == written.stat.executable {
                            written.stat = now;
                            written.dirty = None;
                        } else {
                            written.dirty = Some(now);
                            modified = true;
                        }
                        changed = true;
                    }
                }
                // It moved, and what it holds now cannot be told.
                None => modified = true,
            }
        }
        if let Some(settings) = &mut kept.settings {
            let file = destination(config_dir, layout.settings).ok();
            let now = file.as_deref().and_then(stat);
            if now.is_none() || now != settings.stat {
                // By meaning, not by bytes: the agent rewrites this file
                // itself, and so does whoever registers the hooks.
                let owner: Option<Value> = fs::read(dir.join("owner-settings.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                let current = file
                    .filter(|_| now.is_some())
                    .and_then(|file| settings_on_disk(&file).ok().flatten());
                let differs = match (owner, current) {
                    (Some(owner), Some(current)) => {
                        !merge::same_but_for_owned(&merge::Ours::on_this_box(), &owner, &current)
                    }
                    _ => true,
                };
                if settings.stat != now || settings.modified != differs {
                    settings.stat = now;
                    settings.modified = differs;
                    changed = true;
                }
            }
            modified |= settings.modified;
        }
        (modified, changed)
    }

    /// One environment: see whether its files have been changed by hand,
    /// then decide, and in a pass that applies, apply.
    fn one(&mut self, environment: &Environment, layout: &Layout, mode: Mode) -> (Synced, Value) {
        let dir = self.environment_dir(environment);
        let mut kept = Kept::load(&dir);
        if kept.source != self.source {
            // The files an earlier sync wrote are still its own, to replace
            // or remove. The commit they came from, and the owner's go-ahead,
            // were for another repository.
            kept.source = self.source.clone();
            kept.approved = false;
            kept.commit = None;
            kept.applied_at_ms = None;
            kept.stuck = None;
        }
        let config_dir = environment.config_dir.clone().unwrap_or_default();
        let mut modified = None;
        if kept.commit.is_some() {
            let (differs, changed) = self.drift(layout, &config_dir, &dir, &mut kept);
            modified = Some(differs);
            if changed {
                kept.save(&dir);
            }
        }
        let outcome = self.decide(
            layout,
            environment,
            &config_dir,
            &dir,
            &mut kept,
            modified,
            mode,
        );
        if outcome.applied {
            modified = Some(false);
        }
        let commit = kept.commit.clone().filter(|id| valid_commit_id(id));
        let synced = Synced {
            modified: modified.filter(|_| commit.is_some()),
            state: outcome.state(),
            detail: outcome.why.map(|why| why.as_str().to_string()),
            checked_at_ms: self.now_ms,
            applied_at_ms: kept.applied_at_ms.filter(|_| commit.is_some()),
            commit,
        };
        let changes: Vec<Value> = outcome
            .changes
            .iter()
            .map(|change| json!({ "path": change.path, "change": change.kind.as_str() }))
            .collect();
        let report = json!({
            "environment": environment.name,
            "action": outcome.action,
            "detail": synced.detail,
            "commit": self.fetched.tip,
            "applied_commit": synced.commit,
            "modified": synced.modified,
            "changes": changes,
        });
        (synced, report)
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(
        &mut self,
        layout: &Layout,
        environment: &Environment,
        config_dir: &Path,
        dir: &Path,
        kept: &mut Kept,
        modified: Option<bool>,
        mode: Mode,
    ) -> Outcome {
        if !self.fetched.ok && (self.fetched.attempted_at_ms != 0 || mode != Mode::Scheduled) {
            // The cache file is the box's own, and something else may have
            // written to it: only a word for an unreachable repository is
            // taken from it.
            let why = self
                .fetched
                .failure
                .as_deref()
                .and_then(Why::parse)
                .filter(|why| why.state() == State::Unreachable)
                .unwrap_or(Why::FetchFailed);
            return Outcome::stopped(why);
        }
        let Some(tip) = self.fetched.tip.clone() else {
            return Outcome {
                unknown: true,
                ..Outcome::fine("not_fetched", Vec::new(), false)
            };
        };
        let behind = kept.commit.as_deref() != Some(tip.as_str());
        let held = modified == Some(true) && self.config_sync.on_drift == OnDrift::Keep;
        if mode == Mode::Scheduled {
            if !behind {
                return Outcome::fine("up_to_date", Vec::new(), false);
            }
            if let Some(why) = kept.still_stuck(&tip, &self.signers, self.now_ms) {
                return Outcome::stopped(why);
            }
            if !kept.approved {
                return Outcome::stopped(Why::AwaitingDryRun);
            }
            if held {
                return Outcome::stopped(Why::HandChange);
            }
        }
        let prepared = match self.prepare(layout, environment, config_dir, kept, &tip) {
            Ok(prepared) => prepared,
            Err(why) => return self.stuck(kept, dir, &tip, why),
        };
        if kept.stuck.take().is_some() {
            kept.save(dir);
        }
        let changes = prepared.changes.clone();
        let to_do = behind || !changes.is_empty();
        match mode {
            Mode::Dry => {
                // The owner has now seen what would change here.
                if !kept.approved {
                    kept.approved = true;
                    kept.save(dir);
                }
                let action = if to_do { "would_apply" } else { "up_to_date" };
                return Outcome::fine(action, changes, false);
            }
            Mode::Now if !to_do => return Outcome::fine("up_to_date", changes, false),
            Mode::Now if !kept.approved => {
                return Outcome {
                    changes,
                    ..Outcome::stopped(Why::AwaitingDryRun)
                }
            }
            Mode::Now if held => {
                return Outcome {
                    changes,
                    ..Outcome::stopped(Why::HandChange)
                }
            }
            Mode::Now | Mode::Scheduled => {}
        }
        match self.write(layout, config_dir, dir, kept, &tip, prepared) {
            Ok(()) => Outcome::fine("applied", changes, true),
            Err(why) => self.stuck(kept, dir, &tip, why),
        }
    }

    /// Note why a commit was stopped, so it is not tried again at once.
    fn stuck(&self, kept: &mut Kept, dir: &Path, commit: &str, why: Why) -> Outcome {
        kept.stuck = Some(Stuck {
            commit: commit.to_string(),
            detail: why.as_str().to_string(),
            at_ms: self.now_ms,
            signers: self.signers.clone(),
        });
        kept.save(dir);
        Outcome::stopped(why)
    }
}

/// Say on the box's own log that an environment's sync now stands
/// differently: its name and fixed words, never anything of the repository.
fn log_change(name: &str, before: Option<&Synced>, after: &Synced) {
    let said = |synced: &Synced| {
        (
            synced.state,
            synced.detail.clone(),
            synced.commit.clone(),
            synced.modified,
        )
    };
    if before.map(said) == Some(said(after)) {
        return;
    }
    let mut line = format!("nucleated: config sync for {name}:");
    match after.state {
        Some(state) => line.push_str(&format!(" {}", state.as_str())),
        None => line.push_str(" not fetched yet"),
    }
    if let Some(detail) = &after.detail {
        line.push_str(&format!(" ({detail})"));
    }
    if let Some(commit) = &after.commit {
        line.push_str(&format!(", at {}", &commit[..8.min(commit.len())]));
    }
    if after.modified == Some(true) {
        line.push_str(", changed by hand");
    }
    eprintln!("{line}");
}

/// One pass, called every [`super::PASS_EVERY`] while `[config_sync]` is set.
/// It decides for itself whether a fetch is due, and records what it found
/// with [`super::write_status`]. A pass with no fetch due and nothing new to
/// apply runs no program: it looks at each synced file once.
pub fn pass(settings: &Settings, config_sync: &ConfigSync, now: SystemTime) {
    let root = settings.cache_dir.join("config-sync");
    // A sync run by hand is at work. It writes the state itself.
    let Some(_lock) = Lock::take(&root, Duration::ZERO) else {
        return;
    };
    let mut run = Run::open(settings, config_sync, now);
    if run.fetch_due() {
        run.fetch();
    }
    let before = read_status(settings).unwrap_or_default();
    let mut status = Status::default();
    for environment in super::synced_environments(settings) {
        // An agent whose config folder is not described is not synced, and
        // nothing is said about its config.
        let Some(layout) = layout(&environment.agent) else {
            continue;
        };
        let (synced, _) = run.one(environment, layout, Mode::Scheduled);
        log_change(
            &environment.name,
            before.environments.get(&environment.name),
            &synced,
        );
        status.environments.insert(environment.name.clone(), synced);
    }
    let _ = write_status(&settings.state_dir, &status);
}

/// One pass now, for the environments given. Returns the report to print and
/// whether nothing in it failed. With `dry_run` nothing is written to a
/// config folder; the report says what would be, and from then on the
/// daemon may apply to those environments.
pub fn run_now(
    settings: &Settings,
    config_sync: &ConfigSync,
    environments: &[&Environment],
    dry_run: bool,
) -> Result<(Value, bool)> {
    let root = settings.cache_dir.join("config-sync");
    let Some(_lock) = Lock::take(&root, WAIT_FOR_PASS) else {
        bail!("another config sync is at work on this box; try again in a moment");
    };
    let mut run = Run::open(settings, config_sync, SystemTime::now());
    run.fetch();
    let mode = if dry_run { Mode::Dry } else { Mode::Now };
    let mut status = read_status(settings).unwrap_or_default();
    let mut ok = true;
    let mut reports = Vec::new();
    for environment in environments {
        let Some(layout) = layout(&environment.agent) else {
            reports.push(json!({
                "environment": environment.name,
                "action": "skipped",
                "detail": "no_layout",
            }));
            continue;
        };
        let (synced, report) = run.one(environment, layout, mode);
        ok &= synced.detail.is_none();
        if !dry_run {
            status.environments.insert(environment.name.clone(), synced);
        }
        reports.push(report);
    }
    if !dry_run {
        write_status(&settings.state_dir, &status)?;
    }
    // Whoever reads a dry run is deciding whether to let the first apply go
    // ahead. Say whether a commit has to be signed to get that far: a config
    // that only looks as if it asked for signatures must not pass unseen.
    let signatures = match config_sync.require_signed {
        true => "required",
        false => "not_checked",
    };
    Ok((
        json!({ "dry_run": dry_run, "signatures": signatures, "environments": reports }),
        ok,
    ))
}
