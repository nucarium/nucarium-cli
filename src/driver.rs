//! The seam between a driver and the event envelope.
//!
//! A driver is a tool people use to run and steer agents on the box: T3
//! Code, Herdr, Orca. It calls no hook, so the daemon asks it, on threads of
//! its own beside housekeeping and the reporter. Each driver has one adapter,
//! in a file of its own under `src/driver/`, and that file holds everything
//! that knows how the driver is asked and what its answer looks like.
//!
//! The host never asks a driver, and never opens a file, a socket or a
//! folder a driver names. A look is the box's own `nucleated`, run inside the
//! environment's container through one `podman exec` with a deadline of its
//! own ([`inside`]). There the adapter asks its driver and the folders it
//! names are read, as a hook in that container would read them; what comes
//! back is a small versioned answer of ids, fixed words, numbers, repo names
//! and branches ([`answer`]), which the host holds field by field to fixed
//! lists, patterns and caps before it believes any of it.
//!
//! An adapter does one thing: it takes one look at its driver and says which
//! sessions it saw and what each is doing, in the words below. Everything
//! else is here and is the same for every driver: remembering what was seen
//! last time, turning the difference into the lifecycle events a hook would
//! have recorded, writing them to the spool, and standing aside for a session
//! that an agent's own hook already reports.
//!
//! Three rules bind every adapter.
//!
//! - **State only.** A driver's answer carries content beside the state, and
//!   no driver offers a way to leave it out. An adapter reads its answer
//!   through [`crate::allowlist`] into structs that name only the fields its
//!   design lists. What comes out of an adapter is an id, a phase, a short
//!   kind, a time, a counter and a folder; the folder is used inside the
//!   container to find a repo name and a branch, and never leaves it.
//! - **Observe only.** An adapter sends its driver only the read calls its
//!   design names, and a test of its own asserts the exact calls made.
//!   Nothing here starts, stops, steers or sends a command to anything.
//! - **Failures are boring.** A driver that is absent, slow, refusing or
//!   speaking a newer protocol is no look at all: no event, no ended
//!   session, and no change to whether the environment's server is alive,
//!   which is asked of its supervisor and never of the driver.

use crate::bounded::{self, Failure};
use crate::envelope::{self, unix_ms, valid_env_key, valid_session_id, Draft, Envelope, EventType};
use crate::housekeeping;
use crate::paths;
use crate::sessions::{Status, Store, ACTIVITY_BEAT};
use crate::settings::{self, Environment, Settings};
use crate::spool::{Record, Spool};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub mod answer;
pub mod cut_off;
pub mod herdr_core;
pub mod herdr_tui;
pub mod inside;
pub mod orca;
pub mod t3code;

use answer::{Answer, Seen};

/// A look is over within this long on the host, whatever the driver or the
/// container does: `podman exec`, the look inside it, and its answer.
pub const LOOK_LIMIT: Duration = Duration::from_secs(5);
/// Inside the container, a look ends itself and everything it started
/// within this long, so that it is over before the host gives up on it.
pub const INNER_LIMIT: Duration = Duration::from_millis(4500);
/// An adapter's own calls to its driver are given up after this long, inside
/// [`INNER_LIMIT`] with room for reading the folders it names.
pub const ASK_LIMIT: Duration = Duration::from_secs(4);
/// How long the host gives the container engine to say which process is a
/// container's first, to end a look it gave up on.
const END_LIMIT: Duration = Duration::from_secs(2);
/// How long `podman exec` is then given to return by itself, as it does once
/// the look has ended, before it is stopped.
const END_GRACE: Duration = Duration::from_secs(3);
/// A session a complete look does not list is ended once this many looks in
/// a row have not listed it. One look can catch a driver between two states.
pub const MISSING_LOOKS: u8 = 2;
/// A hook's claim on a driver's session stands this long before the daemon
/// has applied the hook's first event.
const CLAIM_GRACE: Duration = Duration::from_secs(120);
/// A hook renews its claim once the claim is this old, so a claim's age says
/// how long ago its hook last spoke from inside the driver's session.
const CLAIM_RENEW: Duration = Duration::from_secs(30);
/// A claim its hook has not renewed for this long is quiet, and a quiet
/// claim lapses once the driver has, for this long again, seen the session
/// do what the hook's session is not doing. An agent whose hooks speak for
/// it is never that quiet while it works or starts to wait, so by then the
/// hook's agent has gone without saying so, and whatever is in the driver's
/// session now has no hook.
const CLAIM_QUIET: Duration = Duration::from_secs(120);
/// How long a driver's session may wait on its owner, or stand failed,
/// before the hook that speaks for it has to be showing the same. A hook is
/// believed for what its own session shows and for nothing more, so past
/// this the driver's view is reported, whatever the hook goes on saying. It
/// is longer than the minute Claude Code leaves a prompt idle before its
/// hook says so, with the look and the batch that follow it, so a hook that
/// has a word for what the driver sees has said it by then. One that has
/// none (Codex waiting on an answer) leaves the session reported from both.
const CLAIM_UNHEARD: Duration = Duration::from_secs(90);
/// A claim names at most this many folders: where its hook's session works,
/// and the folders that one is in.
const CLAIM_FOLDERS: usize = envelope::MAX_CLAIM_FOLDERS;
/// A driver environment holds claims on at most this many of its sessions,
/// as many as one look can report.
const CLAIMED_KEYS: usize = answer::MAX_SESSIONS;
/// At most this many hook-reported sessions hold a claim on one of a
/// driver's sessions at once. A claim beyond either limit is not noted: the
/// claims are the box's own, but a container chooses how many it makes.
const CLAIMS_PER_KEY: usize = 8;

/// What a driver's agent is doing, in the daemon's words. An adapter maps
/// its driver's documented state words to these and says nothing for a word
/// it does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Ready for its owner: finished a turn, or not yet given one.
    Idle,
    Working,
    /// Waiting on its owner to allow something.
    NeedsPermission,
    /// Waiting on its owner to answer or say something.
    NeedsInput,
    /// Waiting on its owner to answer a tool's own question.
    NeedsElicitation,
    /// Its turn ended in an error.
    Failed,
    /// The driver says the session is over. A session the driver merely
    /// stops listing needs no such word: see [`MISSING_LOOKS`].
    Ended,
}

impl Phase {
    pub fn needs_input(self) -> bool {
        matches!(
            self,
            Phase::NeedsPermission | Phase::NeedsInput | Phase::NeedsElicitation
        )
    }
}

/// One session a driver listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// The session's id, the same at every look for as long as the session
    /// lasts, and fit to send: `[A-Za-z0-9._:-]{1,128}`. One that does not
    /// fit is not reported at all. It must never be built from a path, a
    /// title or a name a person chose.
    pub id: String,
    /// `None` is "no opinion": the driver's word was not one the adapter
    /// knows, or the driver itself is not sure. The session keeps the phase
    /// it had, and still counts as listed.
    pub phase: Option<Phase>,
    /// With [`Phase::Failed`], the driver's own short machine-readable kind,
    /// one of a list its documentation or source gives. Never a message.
    pub error_kind: Option<String>,
    /// With [`Phase::Failed`] on a usage limit, when the driver says the
    /// limit resets. A time the adapter parsed from a documented field.
    pub limit_resets_at: Option<SystemTime>,
    /// The folder the session works in. Read inside the container, as a
    /// hook's `cwd` is, to find the session's repos and branch, and never
    /// handed to the host.
    pub cwd: Option<PathBuf>,
    /// A number that only grows while the agent is doing things: a count of
    /// items, or a time of last activity in milliseconds. While the session
    /// stays working and this grows, the daemon passes on an `activity`
    /// event once in [`ACTIVITY_BEAT`]. `None` for a driver that reports no
    /// such thing; nothing is invented in its place.
    pub activity: Option<u64>,
    /// A number that goes up each time a turn ends, when the driver keeps
    /// one. It reveals a turn that began and ended between two looks.
    pub turn: Option<u64>,
    /// What a hook running inside this session would answer from
    /// [`Driver::claims`]. When a live hook-reported session has claimed it,
    /// this session is the same agent seen twice, and the hook wins.
    pub claim_key: Option<String>,
    /// The agent's own id for its session, when the driver's answer gives
    /// it. When a live hook-reported session on this box has that very id,
    /// this session is that agent seen twice, and the hook wins. Compared on
    /// the box, never stored, never sent.
    pub agent_session_id: Option<String>,
}

impl Observed {
    pub fn new(id: impl Into<String>, phase: Option<Phase>) -> Observed {
        Observed {
            id: id.into(),
            phase,
            error_kind: None,
            limit_resets_at: None,
            cwd: None,
            activity: None,
            turn: None,
            claim_key: None,
            agent_session_id: None,
        }
    }
}

/// What one look at a driver found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    pub sessions: Vec<Observed>,
    /// False when the driver said its list was cut short. No session is
    /// ended for being missing from such a list.
    pub complete: bool,
    /// The driver's own id for this installation, when its link template
    /// needs one. Sent in the heartbeat as `driver_ref` if it fits
    /// `[A-Za-z0-9._:-]{1,128}`, and left out if not.
    pub link_ref: Option<String>,
    /// Small things the adapter learned about its driver and wants handed
    /// back at the next look, in [`Memory::facts`]: what the driver said it
    /// can do, asked once and not at every look. Each is a name the adapter
    /// wrote itself and a number, so nothing the driver said in words can
    /// ride along. They are kept on the box, in the driver's memory file,
    /// and are never sent.
    pub facts: BTreeMap<&'static str, u64>,
    /// For a driver that owns its worktrees: the folder of every git
    /// worktree its answer listed, with an agent in it or not. Compared
    /// inside the container with what [`Driver::worktrees`] finds, so that
    /// the count of unsaved work is never sent as a number while it leaves
    /// one of them out. Only how many are left out reaches the host.
    pub worktrees: Vec<PathBuf>,
}

/// Why a look found nothing, or `Ok`. One fixed word, for `nucleated status`
/// and the daemon's log: never text from the driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    Ok,
    /// The driver is not there: no socket, no runtime file, no program.
    #[default]
    Absent,
    /// It is there and did not answer.
    Unreachable,
    Timeout,
    /// It answered that this program may not ask.
    Rejected,
    /// The owner has not given this program the read-only token it needs.
    NoToken,
    /// It speaks a protocol this adapter does not.
    Unsupported,
    /// It answered this call with an error of its own, or its program ran
    /// and said it failed. What the error says is never read.
    Error,
    /// It answered with something that is not its documented answer.
    BadReply,
}

impl Condition {
    pub fn as_str(self) -> &'static str {
        match self {
            Condition::Ok => "ok",
            Condition::Absent => "absent",
            Condition::Unreachable => "unreachable",
            Condition::Timeout => "timeout",
            Condition::Rejected => "rejected",
            Condition::NoToken => "no_token",
            Condition::Unsupported => "unsupported",
            Condition::Error => "error",
            Condition::BadReply => "bad_reply",
        }
    }
}

/// The process a hook entry point is running in, as much of it as a driver
/// may look at to say whether the hook's agent is one of its own.
pub struct HookProcess<'a> {
    /// The driver's environment.
    pub environment: &'a Environment,
    /// The agent whose hook this is, by its config name: one of
    /// [`crate::agent::NAMES`]. A driver that runs agents of several kinds
    /// side by side puts it in the key, so that one agent's hook stands in
    /// for that agent's row and no other's.
    pub agent: &'a str,
    /// The folder the hook's session is working in.
    pub cwd: Option<&'a Path>,
    /// Reads one named environment variable of the hook's process. A driver
    /// asks only for the variables its documentation says it sets.
    pub var: &'a dyn Fn(&str) -> Option<String>,
}

pub trait Driver: Sync {
    /// The name used for this driver in the config file, and sent in the
    /// heartbeat as `driver`.
    fn name(&self) -> &'static str;

    /// The `driver_*` config keys this driver reads. Any other is refused
    /// when the config is loaded. `driver_poll` is every driver's.
    fn keys(&self) -> &'static [&'static str] {
        &[]
    }

    /// The systemd user unit the driver's own documentation or source names
    /// for its server, or nothing when it names none. With nothing, the
    /// config must say `unit` or `check`.
    fn default_unit(&self) -> Option<&'static str> {
        None
    }

    /// How often the driver is asked when the config does not say.
    fn default_poll(&self) -> Duration {
        Duration::from_secs(15)
    }

    /// The config may not ask more often than this.
    fn min_poll(&self) -> Duration {
        Duration::from_secs(5)
    }

    /// Refuse a config this driver cannot use, with the reason in a
    /// sentence. Called when the config is loaded; reads no file and starts
    /// nothing.
    fn check_config(&self, _environment: &Environment) -> Result<(), String> {
        Ok(())
    }

    /// One look at the driver's agents: only the read calls the adapter's
    /// design names, within [`ASK_LIMIT`], touching nothing beyond the box.
    /// Runs inside the environment's container, in `nucleated look`.
    /// `previous` is what the last look left, for an adapter that asks its
    /// driver something once and not at every look: its condition, when it
    /// last found the driver, its link reference and its facts.
    ///
    /// An `Err` is "could not know". The daemon then changes nothing.
    fn observe(
        &self,
        environment: &Environment,
        previous: &Memory,
        now: SystemTime,
    ) -> Result<Look, Condition>;

    /// True for a driver that makes and removes the worktrees under the
    /// environment's root itself. Housekeeping then never touches anything
    /// under that root, and the unsaved-work count covers what
    /// [`Driver::worktrees`] lists.
    fn owns_worktrees(&self) -> bool {
        false
    }

    /// The worktrees a driver owns, found without the driver's help, so the
    /// count still works while the driver is stopped. `None` is "cannot
    /// know", and the count is then not known either.
    fn worktrees(&self, _environment: &Environment) -> Option<Vec<PathBuf>> {
        None
    }

    /// The address that opens one session in the driver's own apps, with
    /// `{driver_ref}` and `{session_id}` in it, exactly as the driver
    /// documents it. Nothing for a driver that documents none.
    ///
    /// Each of the two is one segment of a path. Whoever fills the template
    /// in percent-encodes each value as a whole segment, leaving only
    /// letters, digits, `-` and `_` as they are: `.` and `:` are encoded
    /// too. Both values are held to `[A-Za-z0-9._:-]{1,128}` before they
    /// leave the box, so nothing else needs encoding.
    fn link_template(&self) -> Option<&'static str> {
        None
    }

    /// Called from the hook entry point: if the hook's agent is running
    /// inside one of this driver's sessions, the key that session will be
    /// listed under ([`Observed::claim_key`]). Read from the variables the
    /// driver documents, from the folder and from which agent's hook it is,
    /// held to the driver's own grammar for such a key, and nothing where
    /// there is any doubt. Must be instant: no file larger than a line, no
    /// program, no socket.
    ///
    /// The variables are the hook's, not the daemon's. An adapter that finds
    /// its driver by one of them when the config names no place (a runtime
    /// folder, say) finds another place from a hook whose agent was started
    /// with that variable changed, and then claims nothing: its page tells
    /// the owner which config key settles it.
    fn claims(&self, _hook: &HookProcess) -> Option<String> {
        None
    }
}

/// Every driver there is an adapter for, by its config name.
pub const NAMES: [&str; 4] = ["t3code", "herdr-core", "herdr-tui", "orca"];

/// The one registry of driver adapters.
pub fn by_name(name: &str) -> Option<&'static dyn Driver> {
    match name {
        "t3code" => Some(&t3code::T3Code),
        "herdr-core" => Some(&herdr_core::HerdrCore),
        "herdr-tui" => Some(&herdr_tui::HerdrTui),
        "orca" => Some(&orca::Orca),
        _ => None,
    }
}

/// `http://<this machine>[:port]` and nothing more, or nothing. A driver's
/// server is asked on the box and never anywhere else, so neither a token
/// nor an answer can leave it.
pub fn loopback_origin(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("http://")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (host, port) = match rest.strip_prefix('[') {
        // An IPv6 address is only itself between its brackets.
        Some(bracketed) => match bracketed.split_once(']')? {
            ("::1", "") => ("::1", None),
            ("::1", after) => ("::1", Some(after.strip_prefix(':')?)),
            _ => return None,
        },
        None => match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        },
    };
    let local = matches!(host, "127.0.0.1" | "localhost" | "::1");
    let port_ok = port.is_none_or(|port| {
        (1..=5).contains(&port.len()) && port.bytes().all(|c| c.is_ascii_digit())
    });
    (local && port_ok).then(|| format!("http://{rest}"))
}

// ---- what is remembered between one look and the next ----------------------

/// One session as the last look left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remembered {
    pub phase: Phase,
    #[serde(default)]
    pub activity: Option<u64>,
    #[serde(default)]
    pub turn: Option<u64>,
    /// When an event for this session was last written while it worked.
    #[serde(default)]
    pub beat_ms: u64,
    /// How many complete looks in a row have not listed it.
    #[serde(default)]
    pub missing: u8,
    /// True while nothing has ever been written for this session: it was
    /// idle when the driver was first seen, and has done nothing since. A
    /// session nobody was told of is never ended either. It is forgotten
    /// without a word.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub silent: bool,
}

/// What `drivers/<environment>.json` holds: ids, fixed words, counters and
/// times, and nothing else. `nucleated status` and the heartbeat read it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub driver: String,
    pub condition: Condition,
    /// When `condition` last changed.
    #[serde(default)]
    pub since_ms: u64,
    #[serde(default)]
    pub looked_at_ms: u64,
    /// When a look last found the driver.
    #[serde(default)]
    pub good_at_ms: Option<u64>,
    /// Set once a look has found the driver. Before that, what is idle is
    /// history and is remembered without a word.
    #[serde(default)]
    pub baseline: bool,
    #[serde(default)]
    pub link_ref: Option<String>,
    /// What the adapter asked to be handed back, from the last look that
    /// found the driver: [`Look::facts`]. Names and numbers.
    #[serde(default)]
    pub facts: BTreeMap<String, u64>,
    #[serde(default)]
    pub sessions: BTreeMap<String, Remembered>,
    /// How many worktrees the driver's last answer listed that the count of
    /// its unsaved work does not reach: see [`uncounted`]. While there is
    /// one, that count is not known and no number is sent for it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncounted_worktrees: u64,
    /// By claim key, when a look first saw a session whose hook had gone
    /// quiet doing what the hook's session is not. See [`CLAIM_QUIET`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub doubts: BTreeMap<String, u64>,
    /// By the driver's id for a session a hook speaks for, when a look first
    /// saw it wait on its owner or stand failed while no hook's session
    /// showed the same. See [`CLAIM_UNHEARD`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unheard: BTreeMap<String, u64>,
}

impl Memory {
    fn new(driver: &str) -> Memory {
        Memory {
            driver: driver.to_string(),
            ..Memory::default()
        }
    }
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// How many of the worktrees a driver listed the count of its unsaved work
/// leaves out. The count is made from the folders the adapter finds by
/// looking, with the driver stopped or running ([`Driver::worktrees`]), and
/// a driver may keep a worktree anywhere. One is left out when it is a
/// linked worktree, in none of the folders that are looked in, and not a
/// nucleus's own session, which that nucleus counts. A project's own
/// checkout is its owner's and not the driver's, and is never counted.
fn uncounted(settings: &Settings, environment: &Environment, listed: &[PathBuf]) -> u64 {
    let adapter = environment
        .driver_adapter()
        .filter(|adapter| adapter.owns_worktrees());
    let Some(adapter) = adapter.filter(|_| !listed.is_empty()) else {
        return 0;
    };
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let found: BTreeSet<PathBuf> = adapter
        .worktrees(environment)
        .unwrap_or_default()
        .iter()
        .map(|path| resolve(path))
        .collect();
    let sessions: Vec<PathBuf> = settings
        .environments
        .iter()
        .filter(|other| !other.driver_owns_worktrees())
        .map(|other| resolve(&other.root.join(".worktrees")))
        .collect();
    let left_out: BTreeSet<PathBuf> = listed
        .iter()
        // A linked worktree's `.git` is a file that names its repository.
        .filter(|path| fs::symlink_metadata(path.join(".git")).is_ok_and(|git| git.is_file()))
        .map(|path| resolve(path))
        .filter(|path| !found.contains(path))
        .filter(|path| !sessions.iter().any(|folder| path.starts_with(folder)))
        .collect();
    left_out.len() as u64
}

fn valid_key(key: &str) -> bool {
    valid_session_id(key) && !matches!(key, "." | "..")
}

pub fn memory_file(state_dir: &Path, environment: &str) -> PathBuf {
    state_dir
        .join("drivers")
        .join(format!("{environment}.json"))
}

/// What the last look at an environment's driver left, or nothing when
/// there has been none. One small file: the heartbeat reads it.
pub fn read_memory(state_dir: &Path, environment: &str) -> Option<Memory> {
    let text = fs::read_to_string(memory_file(state_dir, environment)).ok()?;
    let mut memory: Memory = serde_json::from_str(&text).ok()?;
    // The file is the box's own, and something else may have written to it.
    memory.sessions.retain(|id, _| valid_session_id(id));
    memory.link_ref = memory
        .link_ref
        .filter(|reference| valid_session_id(reference));
    memory.facts.retain(|name, _| valid_key(name));
    memory.doubts.retain(|key, _| valid_key(key));
    memory.unheard.retain(|id, _| valid_session_id(id));
    Some(memory)
}

// ---- the hook wins ---------------------------------------------------------

fn claims_dir(state_dir: &Path, driver_environment: &str) -> PathBuf {
    state_dir.join("claims").join(driver_environment)
}

/// A number made from a folder's path, so that a claim can say where its
/// hook's session works without the folder's name being written anywhere.
pub(crate) fn folder_number(folder: &Path) -> String {
    // FNV-1a, written out so the hook and the daemon always agree.
    let hash = folder
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{hash:016x}")
}

fn resolved(folder: &Path) -> PathBuf {
    folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf())
}

/// The numbers of the folder a hook's session works in and of every folder
/// that one is in, its own first.
fn folder_numbers(folder: &Path) -> Vec<String> {
    resolved(folder)
        .ancestors()
        .take(CLAIM_FOLDERS)
        .map(folder_number)
        .collect()
}

/// Note, in the daemon's own state, that a hook-reported session is running
/// inside one of a driver's sessions: one small file, written when it is not
/// already there and again once it is [`CLAIM_RENEW`] old, so a claim is
/// only as good as its hook's last word. The daemon calls this for a claim
/// it read in a hook's spool record and counted ([`claim_counts`]). The
/// claim keeps numbers made from where the hook's session works and never
/// that folder's name. Beside it goes the session's note of which claim it
/// holds ([`claim_index`]). A new claim past [`CLAIMED_KEYS`] or
/// [`CLAIMS_PER_KEY`] is not noted.
pub fn note_claim(state_dir: &Path, claim: &envelope::Claim, environment: &str, session_id: &str) {
    let fits = claim.fits() && valid_key(&claim.key);
    if !fits || !valid_env_key(environment) || !valid_session_id(session_id) {
        return;
    }
    let driver = claims_dir(state_dir, &claim.driver_env);
    let path = driver.join(&claim.key).join(format!("{session_id}.json"));
    let mut note = environment.to_string();
    if !claim.folders.is_empty() {
        note.push('\n');
        note.push_str(&claim.folders.join(" "));
    }
    let meta = fs::symlink_metadata(&path).ok();
    if meta.is_none() && !room_for(&driver, &claim.key) {
        return;
    }
    let renewed = meta
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < CLAIM_RENEW);
    if !renewed || fs::read_to_string(&path).ok().as_deref() != Some(note.as_str()) {
        let _ = paths::write_private(&path, note.as_bytes());
    }
    let index = claim_index(state_dir, environment, session_id);
    let names = index_note(&claim.driver_env, &claim.key);
    if fs::read_to_string(&index).ok().as_deref() != Some(names.as_str()) {
        let _ = paths::write_private(&index, names.as_bytes());
    }
}

/// Whether a new claim on `key` fits under [`CLAIMED_KEYS`] and
/// [`CLAIMS_PER_KEY`]. It reads one folder, and only as far as its limit,
/// however many entries that folder has.
fn room_for(driver: &Path, key: &str) -> bool {
    let under = |dir: &Path, limit: usize| {
        fs::read_dir(dir).map_or(0, |entries| entries.take(limit).count()) < limit
    };
    let folder = driver.join(key);
    if folder.is_dir() {
        under(&folder, CLAIMS_PER_KEY)
    } else {
        under(driver, CLAIMED_KEYS)
    }
}

/// Where the daemon notes which claim a hook-reported session holds, so that
/// finding it is one small read however many claims there are.
fn claim_index(state_dir: &Path, environment: &str, session_id: &str) -> PathBuf {
    state_dir.join("claimed").join(environment).join(session_id)
}

fn index_note(driver_environment: &str, key: &str) -> String {
    format!("{driver_environment}\n{key}")
}

/// Remove a session's note of its claim if it still names `names`: a later
/// claim the session made elsewhere is left standing.
fn forget_index(index: &Path, names: &str) {
    if fs::read_to_string(index).is_ok_and(|held| held == names) {
        let _ = fs::remove_file(index);
    }
}

/// Ask the drivers in the hook's own container whether its agent is one of
/// theirs, and say which, for the hook's spool record: the first driver
/// environment there whose adapter claims it, the key, and numbers made from
/// where the hook's session works. `own` is the environment the hook
/// records for. A driver in another container cannot be running this agent,
/// and an environment on the host has no claim the daemon would count. The
/// hook entry point calls this for every event it records.
pub fn hook_claim(
    settings: &Settings,
    own: &Environment,
    agent: &str,
    cwd: Option<&Path>,
    var: &dyn Fn(&str) -> Option<String>,
) -> Option<envelope::Claim> {
    let container = own.container()?;
    settings.environments.iter().find_map(|candidate| {
        if candidate.container() != Some(container) {
            return None;
        }
        let adapter = candidate.driver_adapter()?;
        let hook = HookProcess {
            environment: candidate,
            agent,
            cwd,
            var,
        };
        let key = adapter.claims(&hook).filter(|key| valid_key(key))?;
        Some(envelope::Claim {
            driver_env: candidate.name.clone(),
            key,
            folders: cwd.map(folder_numbers).unwrap_or_default(),
        })
    })
}

/// Whether the daemon counts a claim it read in a spool record. Only one
/// read from the spool folder of the environment `from`, the record's own,
/// that runs in the same container as the driver environment the claim
/// names: that container's hooks are the only ones that can see its
/// driver's sessions. Never one from the shared folder, from an environment
/// on the host, or naming an environment with no driver.
pub fn claim_counts(
    settings: &Settings,
    from: Option<&Environment>,
    claim: &envelope::Claim,
) -> bool {
    let Some(from) = from else {
        return false;
    };
    let Some(driver) = settings.environment(&claim.driver_env) else {
        return false;
    };
    claim.fits()
        && driver.driver_adapter().is_some()
        && from.container().is_some()
        && from.container() == driver.container()
}

/// The driver environment and key of a claim the daemon counted for a
/// hook-reported session, while it stands: until the hook's session ends,
/// or the claim lapses (see [`claims_stand`]). The session's events carry
/// them to Watch as its driver reference.
pub fn standing_claim(
    settings: &Settings,
    environment: &str,
    session_id: &str,
) -> Option<(String, String)> {
    if !valid_env_key(environment) || !valid_session_id(session_id) {
        return None;
    }
    let index = claim_index(&settings.state_dir, environment, session_id);
    let names = fs::read_to_string(&index).ok()?;
    let (driver_env, key) = names.split_once('\n')?;
    let file = claims_dir(&settings.state_dir, driver_env)
        .join(key)
        .join(format!("{session_id}.json"));
    let stands = settings
        .environment(driver_env)
        .is_some_and(|driver| driver.driver_adapter().is_some())
        && valid_key(key)
        && fs::read_to_string(file)
            .is_ok_and(|note| note.lines().next().map(str::trim) == Some(environment));
    if !stands {
        let _ = fs::remove_file(&index);
        return None;
    }
    Some((driver_env.to_string(), key.to_string()))
}

/// One live hook-reported session's claim on a driver's session.
struct Claim {
    file: PathBuf,
    /// The session's note of which claim it holds, and what that note says
    /// while it is this one: see [`claim_index`].
    index: PathBuf,
    names: String,
    /// The hook has spoken from inside the driver's session within
    /// [`CLAIM_QUIET`].
    fresh: bool,
    /// What the hook's session was last doing. Nothing while the daemon has
    /// not yet applied the hook's first event.
    status: Option<Status>,
    /// Where the hook's session works, as numbers: see [`folder_numbers`].
    folders: Vec<String>,
}

/// The claims that live hook-reported sessions hold on a driver's sessions,
/// by key. A claim whose session has ended is forgotten here.
fn claimed(
    settings: &Settings,
    driver_environment: &str,
    now: SystemTime,
) -> BTreeMap<String, Vec<Claim>> {
    let store = Store::new(&settings.state_dir);
    let mut held = BTreeMap::new();
    let Ok(keys) = fs::read_dir(claims_dir(&settings.state_dir, driver_environment)) else {
        return held;
    };
    for key in keys.flatten() {
        let name = key.file_name().to_string_lossy().to_string();
        let Ok(claims) = fs::read_dir(key.path()) else {
            continue;
        };
        let mut live = Vec::new();
        for claim in claims.flatten() {
            let file = claim.file_name().to_string_lossy().to_string();
            // Anything else here is a claim a hook is in the middle of
            // writing, and is left to it.
            let Some(session_id) = file.strip_suffix(".json") else {
                continue;
            };
            let note = fs::read_to_string(claim.path()).unwrap_or_default();
            let mut lines = note.lines();
            let environment = lines.next().unwrap_or_default().trim().to_string();
            let session = settings
                .environment(&environment)
                .map(|_| store.load(&environment, session_id));
            let index = claim_index(&settings.state_dir, &environment, session_id);
            let names = index_note(driver_environment, &name);
            // The file is the box's own, and something else may have
            // written to it: a number is sixteen digits or it is nothing.
            let folders: Vec<String> = lines
                .next()
                .unwrap_or_default()
                .split(' ')
                .filter(|number| {
                    number.len() == 16 && number.bytes().all(|c| c.is_ascii_hexdigit())
                })
                .take(CLAIM_FOLDERS)
                .map(str::to_string)
                .collect();
            // A claim dated ahead of the clock is from before the clock
            // moved, and is as new as a claim can be.
            let age = fs::metadata(claim.path())
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok());
            let status = match session {
                // The daemon has not applied the hook's event yet.
                Some(None) if age.is_none_or(|age| age < CLAIM_GRACE) => Some(None),
                Some(Some(session)) if session.status != Status::Ended => {
                    Some(Some(session.status))
                }
                _ => None,
            };
            match status {
                Some(status) => live.push(Claim {
                    file: claim.path(),
                    index,
                    names,
                    fresh: age.is_none_or(|age| age < CLAIM_QUIET),
                    status,
                    folders,
                }),
                None => {
                    let _ = fs::remove_file(claim.path());
                    if valid_env_key(&environment) && valid_session_id(session_id) {
                        forget_index(&index, &names);
                    }
                }
            }
        }
        if live.is_empty() {
            let _ = fs::remove_dir(key.path());
        } else if valid_key(&name) {
            held.insert(name, live);
        }
    }
    held
}

/// True when a driver that sees a session in `phase` is seeing it do what a
/// hook's session in `status` is not doing: work, or wait on its owner. A
/// hook speaks when its agent starts either. A session at rest, over, or of
/// which the driver has no opinion contradicts nothing.
fn contradicts(phase: Option<Phase>, status: Status) -> bool {
    match phase {
        Some(Phase::Working) => status != Status::Working,
        Some(phase) if phase.needs_input() => status != Status::NeedsInput,
        _ => false,
    }
}

/// Whether the claims on one of a driver's sessions still stand, now that a
/// look has seen it in `phase`. They stand while any hook among them has
/// spoken lately, and while the driver sees nothing the hooks' sessions do
/// not say. Otherwise the doubt is dated, in `doubts`, and once it is
/// [`CLAIM_QUIET`] old with still no word from a hook, the claims lapse and
/// are removed: an agent that died without its `SessionEnd` must not hide
/// whatever runs in its place. A hook that speaks again claims again.
fn claims_stand(
    doubts: &mut BTreeMap<String, u64>,
    key: &str,
    held: &[Claim],
    phase: Option<Phase>,
    now_ms: u64,
) -> bool {
    if held.iter().any(|claim| claim.fresh) {
        doubts.remove(key);
        return true;
    }
    let contradicted = held.iter().all(|claim| {
        claim
            .status
            .is_some_and(|status| contradicts(phase, status))
    });
    if !contradicted {
        return true;
    }
    let since = *doubts.entry(key.to_string()).or_insert(now_ms);
    if now_ms.saturating_sub(since) < CLAIM_QUIET.as_millis() as u64 {
        return true;
    }
    for claim in held {
        let _ = fs::remove_file(&claim.file);
        forget_index(&claim.index, &claim.names);
    }
    doubts.remove(key);
    false
}

/// True when a hook's session in `status` shows what a driver sees of a
/// session in `phase` that its owner needs to hear of: that it waits on
/// them, or that it failed. Any other phase asks nothing of the hook.
fn accounts_for(phase: Option<Phase>, status: Option<Status>) -> bool {
    match phase {
        Some(phase) if phase.needs_input() => status == Some(Status::NeedsInput),
        Some(Phase::Failed) => status == Some(Status::Failed),
        _ => true,
    }
}

// ---- from two looks to events ----------------------------------------------

/// The events that take a session from the phase it was remembered in to the
/// phase it is seen in now, in order. `was` is nothing for a session not
/// seen before.
fn transition(was: Option<Phase>, now: Phase) -> Vec<EventType> {
    use EventType as E;
    let prompt = |phase: Phase| match phase {
        Phase::NeedsPermission => E::PermissionPrompt,
        Phase::NeedsElicitation => E::ElicitationPrompt,
        _ => E::InputPrompt,
    };
    match (was, now) {
        (None, Phase::Ended) => Vec::new(),
        (None, Phase::Idle) => vec![E::SessionStart],
        (None, Phase::Working) => vec![E::SessionStart, E::UserPromptSubmit],
        (None, Phase::Failed) => vec![E::SessionStart, E::StopFailure],
        (None, waiting) => vec![E::SessionStart, prompt(waiting)],
        (Some(_), Phase::Ended) => vec![E::SessionEnd],
        (Some(same), now) if same == now => Vec::new(),
        // The owner answered, so the agent is working again: what a tool
        // call after a granted permission tells a hook.
        (Some(waited), Phase::Working) if waited.needs_input() => vec![E::Activity],
        (Some(_), Phase::Working) => vec![E::UserPromptSubmit],
        (Some(_), Phase::Idle) => vec![E::Stop],
        (Some(_), Phase::Failed) => vec![E::StopFailure],
        (Some(_), waiting) => vec![prompt(waiting)],
    }
}

fn grew(before: Option<u64>, now: Option<u64>) -> bool {
    before.zip(now).is_some_and(|(before, now)| now > before)
}

fn log(line: &str) {
    eprintln!("nucleated: {line}");
}

/// Apply the result of one look to what is remembered about an
/// environment's driver, writing the lifecycle events the difference
/// amounts to. Returns how many were written.
///
/// `result` is a look's answer as the host took it ([`Answer::check`]). A
/// failed look changes the condition word and nothing else. A session is
/// reported under the driver's own id for it; a session a live hook has
/// claimed is not reported, and is ended if it was. A claim does not outlive
/// its hook's silence for ever: see [`claims_stand`]. It counts only when it
/// was made from where the driver says the session works: see
/// [`Seen::same_place`]. And it never hides a session that waits on its
/// owner or has failed unless the hook's own session shows the same: see
/// [`CLAIM_UNHEARD`].
pub fn apply(
    settings: &Settings,
    environment: &Environment,
    memory: &mut Memory,
    result: Result<Answer, Condition>,
    now: SystemTime,
) -> usize {
    let now_ms = unix_ms(now);
    let condition = match &result {
        Ok(_) => Condition::Ok,
        Err(condition) => *condition,
    };
    if condition != memory.condition || memory.looked_at_ms == 0 {
        // A fixed word, and the config's own names. Nothing from the driver.
        log(&format!(
            "driver {} for {}: {}",
            memory.driver,
            environment.name,
            condition.as_str()
        ));
        memory.since_ms = now_ms;
    }
    memory.condition = condition;
    memory.looked_at_ms = now_ms;
    let Ok(look) = result else {
        return 0;
    };
    memory.good_at_ms = Some(now_ms);
    memory.link_ref = look
        .link_ref
        .filter(|reference| valid_session_id(reference));
    memory.facts = look
        .facts
        .iter()
        .filter(|(name, _)| valid_key(name))
        .map(|(name, value)| (name.clone(), *value))
        .collect();
    memory.uncounted_worktrees = look.uncounted_worktrees;

    let spool = Spool::new(&settings.spool_dir);
    // A spool nobody is emptying is not allowed to fill the disk. What was
    // seen is seen again at the next look.
    if spool.holds_at_least(settings.spool_max_events.saturating_mul(2)) {
        return 0;
    }
    let mut claimed = claimed(settings, &environment.name, now);
    memory.doubts.retain(|key, _| claimed.contains_key(key));
    let store = Store::new(&settings.state_dir);
    // What each session a hook has reported under this id, and not ended,
    // is doing.
    let hook_reports = |observed: &Seen, id: &str| -> Vec<Option<Status>> {
        if !valid_session_id(id) {
            return Vec::new();
        }
        settings
            .environments
            .iter()
            // A driver's own session is not a hook's, whatever its id.
            .filter(|other| other.name != environment.name || id != observed.id)
            .filter_map(|other| store.load(&other.name, id))
            .filter(|session| session.status != Status::Ended)
            .map(|session| Some(session.status))
            .collect()
    };
    let grace = CLAIM_UNHEARD.as_millis() as u64;
    let mut unheard = BTreeMap::new();
    let beat = ACTIVITY_BEAT.as_millis() as u64;
    let mut written = 0u64;
    let mut seen = BTreeSet::new();
    let emit = |observed: &Seen, kinds: &[EventType], written: &mut u64| {
        if kinds.is_empty() {
            return;
        }
        let limit_resets_at = observed
            .limit_resets_at_ms
            .map(|ms| std::time::UNIX_EPOCH + Duration::from_millis(ms));
        for kind in kinds {
            // Event ids sort by the millisecond they were made in, and a
            // session's events are applied in that order.
            let at = now + Duration::from_millis(*written);
            let draft = Draft {
                env_id: environment.name.clone(),
                session_id: observed.id.clone(),
                repos: observed.repos.clone(),
                branch: observed.branch.clone(),
                error_kind: observed.error_kind.clone(),
                limit_resets_at,
                ..Draft::default()
            };
            if let Some(event) = Envelope::new(*kind, draft, at) {
                let record = Record {
                    event,
                    at_ms: unix_ms(at),
                    workspace: observed.workspace.clone(),
                    process: None,
                };
                if spool.write(&record).is_ok() {
                    *written += 1;
                }
            }
        }
    };

    for observed in &look.sessions {
        if !valid_session_id(&observed.id) || !seen.insert(observed.id.clone()) {
            continue;
        }
        let known = memory.sessions.get(&observed.id).cloned();
        // The same agent seen twice: the hook wins. What each hook that
        // speaks for this session shows its own session doing.
        let mut speaking: Vec<Option<Status>> = Vec::new();
        if let Some(key) = &observed.claim_key {
            let stands = claimed.get(key).is_some_and(|held| {
                claims_stand(&mut memory.doubts, key, held, observed.phase, now_ms)
            });
            if !stands {
                claimed.remove(key);
            }
            // A claim made from somewhere else is another session's.
            speaking.extend(
                claimed
                    .get(key)
                    .into_iter()
                    .flatten()
                    .filter(|claim| observed.same_place(&claim.folders))
                    .map(|claim| claim.status),
            );
        }
        if let Some(id) = observed.agent_session_id.as_deref() {
            speaking.extend(hook_reports(observed, id));
        }
        let mut hooked = !speaking.is_empty();
        if hooked
            && !speaking
                .iter()
                .any(|status| accounts_for(observed.phase, *status))
        {
            // The owner needs to hear of this, and no hook is saying it. A
            // hook about to say it is waited for, a short while, when the
            // driver's own report is not already out. After that, or when it
            // is, the driver's view is the one reported.
            let since = memory.unheard.get(&observed.id).copied().unwrap_or(now_ms);
            unheard.insert(observed.id.clone(), since);
            if known.is_none() && now_ms.saturating_sub(since) < grace {
                continue;
            }
            hooked = false;
        }
        if hooked {
            if let Some(known) = known {
                // Only a session that was reported has an end to report.
                if !known.silent {
                    emit(observed, &[EventType::SessionEnd], &mut written);
                }
                memory.sessions.remove(&observed.id);
            }
            continue;
        }
        let Some(phase) = observed.phase else {
            if let Some(known) = memory.sessions.get_mut(&observed.id) {
                known.missing = 0;
            }
            continue;
        };
        let Some(known) = known else {
            // What is idle when the driver is first seen is history, and is
            // remembered without a word.
            if phase == Phase::Idle && !memory.baseline {
                memory.sessions.insert(
                    observed.id.clone(),
                    Remembered {
                        phase,
                        activity: observed.activity,
                        turn: observed.turn,
                        beat_ms: 0,
                        missing: 0,
                        silent: true,
                    },
                );
                continue;
            }
            emit(observed, &transition(None, phase), &mut written);
            if phase != Phase::Ended {
                memory.sessions.insert(
                    observed.id.clone(),
                    Remembered {
                        phase,
                        activity: observed.activity,
                        turn: observed.turn,
                        beat_ms: now_ms,
                        missing: 0,
                        silent: false,
                    },
                );
            }
            continue;
        };
        let mut kinds = transition(Some(known.phase), phase);
        // Nobody was told this session began, so nobody is told it is over.
        if known.silent && phase == Phase::Ended {
            kinds.clear();
        }
        let mut beat_ms = known.beat_ms;
        if known.phase == phase {
            let turned = grew(known.turn, observed.turn);
            match phase {
                // A turn ended and another began between two looks.
                Phase::Working if turned => {
                    kinds = vec![EventType::Stop, EventType::UserPromptSubmit];
                }
                Phase::Working
                    if grew(known.activity, observed.activity)
                        && now_ms.saturating_sub(known.beat_ms) >= beat =>
                {
                    kinds = vec![EventType::Activity];
                }
                // A whole turn came and went between two looks.
                Phase::Idle if turned => {
                    kinds = vec![EventType::UserPromptSubmit, EventType::Stop];
                }
                _ => {}
            }
        }
        if !kinds.is_empty() {
            beat_ms = now_ms;
        }
        emit(observed, &kinds, &mut written);
        if phase == Phase::Ended {
            memory.sessions.remove(&observed.id);
        } else {
            memory.sessions.insert(
                observed.id.clone(),
                Remembered {
                    phase,
                    activity: observed.activity.or(known.activity),
                    turn: observed.turn.or(known.turn),
                    beat_ms,
                    missing: 0,
                    // Its first event is the first anyone hears of it.
                    silent: known.silent && kinds.is_empty(),
                },
            );
        }
    }

    // What a complete list no longer holds is over, once a second look
    // agrees. A list cut short says nothing about what it left out.
    if look.complete {
        let gone: Vec<String> = memory
            .sessions
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            let Some(known) = memory.sessions.get_mut(&id) else {
                continue;
            };
            known.missing = known.missing.saturating_add(1);
            if known.missing >= MISSING_LOOKS {
                if !known.silent {
                    emit(&Seen::bare(&id), &[EventType::SessionEnd], &mut written);
                }
                memory.sessions.remove(&id);
            }
        }
    }
    memory.unheard = unheard;
    memory.baseline = true;
    written as usize
}

/// How the host has a look taken: the box's own `nucleated`, and `podman`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    /// The box's own `nucleated`, by its full path. The recipe mounts the
    /// folder it is in read-only at the same path in every container, and a
    /// container's own `PATH` is never asked for it.
    pub program: PathBuf,
    /// The container engine's command-line program.
    pub podman: PathBuf,
}

impl Runner {
    /// This program and the box's `podman`: what the daemon uses.
    pub fn of_this_box() -> Option<Runner> {
        let program = std::env::current_exe().ok()?;
        // A program replaced on disk while it runs is named with this after
        // its path; the path is where the new one is.
        let text = program.to_string_lossy();
        let program = match text.strip_suffix(" (deleted)") {
            Some(path) => PathBuf::from(path),
            None => program,
        };
        Some(Runner {
            program,
            podman: PathBuf::from("podman"),
        })
    }

    /// The command for one look at `environment`: inside its container
    /// through one `podman exec` of the box's own `nucleated`, or, for an
    /// environment on the host, that program itself with the daemon's
    /// config. The arguments are the look's own fixed ones, the
    /// environment's config name, numbers, and what the last look left.
    fn command(&self, settings: &Settings, environment: &Environment, args: &[String]) -> Command {
        match environment.container() {
            Some(container) => {
                let mut command = Command::new(&self.podman);
                command
                    .arg("exec")
                    .arg(container)
                    .arg(&self.program)
                    .args(args);
                command
            }
            None => {
                let mut command = Command::new(&self.program);
                command.args(args).env("NUCLEATED_CONFIG", &settings.path);
                command
            }
        }
    }

    /// Take one look at `environment`'s driver and hold its answer to what
    /// the host takes. "Could not know" is one of the fixed words.
    fn ask(
        &self,
        settings: &Settings,
        environment: &Environment,
        driver: &str,
        memory: &Memory,
        now: SystemTime,
    ) -> Result<Answer, Condition> {
        let token = token(now);
        // As much of the last look as an adapter may use: words and numbers.
        let previous = Memory {
            driver: driver.to_string(),
            condition: memory.condition,
            good_at_ms: memory.good_at_ms,
            link_ref: memory.link_ref.clone(),
            facts: memory.facts.clone(),
            ..Memory::default()
        };
        let args = [
            "look".to_string(),
            "--env".to_string(),
            environment.name.clone(),
            "--within".to_string(),
            INNER_LIMIT.as_millis().to_string(),
            "--now".to_string(),
            unix_ms(now).to_string(),
            "--token".to_string(),
            token.clone(),
            "--previous".to_string(),
            serde_json::to_string(&previous).unwrap_or_default(),
        ];
        let mut command = self.command(settings, environment, &args);
        let read = |out: &mut dyn std::io::Read| answer::read(out);
        let heard = match environment.container() {
            Some(container) => {
                // A look given up on is over for the host at once. What is
                // left of it in the container is ended from the box, and
                // `podman exec` then returns by itself, which is what lets
                // the engine forget the exec.
                let (runner, container) = (self.clone(), container.to_string());
                let ending = bounded::Ending {
                    end: Box::new(move || runner.end(&container, &token)),
                    grace: END_GRACE,
                };
                bounded::try_stream_ending(
                    &mut command,
                    LOOK_LIMIT,
                    answer::MAX_ANSWER,
                    read,
                    ending,
                )
            }
            None => bounded::try_stream(&mut command, LOOK_LIMIT, answer::MAX_ANSWER, read),
        };
        match heard {
            Ok(answer) => answer.check(driver),
            Err(Failure::Late) => Err(Condition::Timeout),
            // No `podman`, no container running, or nothing that could be
            // started in it: the driver is not there to be asked.
            Err(Failure::Absent | Failure::Failed) => Err(Condition::Absent),
            Err(Failure::TooMuch | Failure::NoAnswer) => Err(Condition::BadReply),
        }
    }

    /// The host gave up on a look in `container`, late or for printing too
    /// much. Whatever of it is still running there is ended from the box,
    /// where the container cannot hold it (see [`cut_off`]), so that neither
    /// the container nor the host keeps anything of it. Nothing more is run
    /// in the container.
    fn end(&self, container: &str, token: &str) {
        let mut command = Command::new(&self.podman);
        command
            .args(["container", "inspect", "--format", "{{.State.Pid}}"])
            .arg(container);
        let Ok(said) = bounded::run(&mut command, END_LIMIT, 64) else {
            return;
        };
        let first = String::from_utf8_lossy(&said.stdout).trim().parse::<u32>();
        if let (true, Ok(first)) = (said.ok, first) {
            cut_off::end(first, &self.program, token);
        }
    }
}

/// A look's token, so that one look can be told from another inside a
/// container: sixteen lower-case hex digits.
fn token(now: SystemTime) -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bytes = (nanos ^ (u64::from(std::process::id()) << 32)).to_be_bytes();
    }
    format!("{:016x}", u64::from_be_bytes(bytes))
}

/// Take one look at an environment's driver and apply it. Returns what is
/// now remembered, or nothing for an environment with no driver.
pub fn look(settings: &Settings, environment: &Environment, now: SystemTime) -> Option<Memory> {
    match Runner::of_this_box() {
        Some(runner) => look_with(&runner, settings, environment, now),
        None => {
            // This program cannot say where it is: there is nothing to run.
            let adapter = environment.driver_adapter()?;
            let mut memory = remembered(settings, environment, adapter.name());
            apply(
                settings,
                environment,
                &mut memory,
                Err(Condition::Absent),
                now,
            );
            keep(settings, environment, &memory);
            Some(memory)
        }
    }
}

/// As [`look`], with the programs `runner` names.
pub fn look_with(
    runner: &Runner,
    settings: &Settings,
    environment: &Environment,
    now: SystemTime,
) -> Option<Memory> {
    let adapter = environment.driver_adapter()?;
    let mut memory = remembered(settings, environment, adapter.name());
    let result = runner.ask(settings, environment, adapter.name(), &memory, now);
    apply(settings, environment, &mut memory, result, now);
    keep(settings, environment, &memory);
    Some(memory)
}

/// What the last look at this environment's driver left, or a fresh start.
fn remembered(settings: &Settings, environment: &Environment, driver: &str) -> Memory {
    read_memory(&settings.state_dir, &environment.name)
        // Another driver's memory says nothing about this one's sessions.
        .filter(|memory| memory.driver == driver)
        .unwrap_or_else(|| Memory::new(driver))
}

fn keep(settings: &Settings, environment: &Environment, memory: &Memory) {
    if let Ok(text) = serde_json::to_string(memory) {
        let _ = paths::write_private(
            &memory_file(&settings.state_dir, &environment.name),
            text.as_bytes(),
        );
    }
}

/// How long to wait after a look: the environment's own interval after a
/// good one, and ever longer, up to a minute, after ones that found nothing.
fn next_wait(every: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return every;
    }
    let ceiling = every.max(Duration::from_secs(60));
    every
        .saturating_mul(1 << (failures - 1).min(6))
        .min(ceiling)
}

/// When an environment's next look is due, and whether one is under way.
#[derive(Debug, Clone, Copy)]
struct Slot {
    next: Instant,
    failures: u32,
    busy: bool,
}

/// The driver thread: a few seconds after start, then once a second, start
/// a look at each driver that is due and has none under way. Each look runs
/// on a thread of its own with its own deadline, so a driver that does not
/// answer holds up no other environment's look, no heartbeat and no hook.
/// The config is read afresh each time, as housekeeping reads it.
pub fn run_schedule(config: &Path, stop: &AtomicBool) {
    if housekeeping::wait(stop, Duration::from_secs(5)) {
        return;
    }
    let runner = Runner::of_this_box();
    let slots: Arc<Mutex<BTreeMap<String, Slot>>> = Arc::new(Mutex::new(BTreeMap::new()));
    loop {
        if let Ok(settings) = settings::load_from(config) {
            let settings = Arc::new(settings);
            let mut table = lock(&slots);
            for environment in &settings.environments {
                let Some(every) = environment.driver.as_ref().and_then(|driver| driver.poll) else {
                    continue;
                };
                let now = Instant::now();
                let slot = table.entry(environment.name.clone()).or_insert(Slot {
                    next: now,
                    failures: 0,
                    busy: false,
                });
                if slot.busy || now < slot.next {
                    continue;
                }
                slot.busy = true;
                let (settings, environment) = (settings.clone(), environment.clone());
                let (runner, slots) = (runner.clone(), slots.clone());
                std::thread::spawn(move || {
                    let looked = match &runner {
                        Some(runner) => {
                            look_with(runner, &settings, &environment, SystemTime::now())
                        }
                        None => look(&settings, &environment, SystemTime::now()),
                    };
                    let good = looked.is_some_and(|memory| memory.condition == Condition::Ok);
                    let mut table = lock(&slots);
                    if let Some(slot) = table.get_mut(&environment.name) {
                        slot.failures = if good {
                            0
                        } else {
                            slot.failures.saturating_add(1)
                        };
                        slot.next = Instant::now() + next_wait(every, slot.failures);
                        slot.busy = false;
                    }
                });
            }
            table.retain(|name, slot| slot.busy || settings.environment(name).is_some());
        }
        // With no driver to ask, there is nothing to be on time for.
        let pause = if lock(&slots).is_empty() { 15 } else { 1 };
        if housekeeping::wait(stop, Duration::from_secs(pause)) {
            return;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The folders that belong to drivers that own their worktrees. Nothing in
/// housekeeping removes or changes anything under one.
pub fn protected_roots(settings: &Settings) -> Vec<PathBuf> {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    settings
        .environments
        .iter()
        .filter(|environment| environment.driver_owns_worktrees())
        .flat_map(|environment| {
            let more = environment
                .driver
                .iter()
                .flat_map(|driver| driver.dirs.iter());
            std::iter::once(&environment.root)
                .chain(more)
                .map(|path| resolve(path))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions;
    use std::time::UNIX_EPOCH;

    /// A box with one driver environment, `t3`, and one agent environment,
    /// `client-a`, every path inside the test's own folder.
    fn rig() -> (tempfile::TempDir, Settings) {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name).display().to_string();
        fs::create_dir_all(dir.path().join("work")).unwrap();
        let config = dir.path().join("config.toml");
        fs::write(
            &config,
            format!(
                "state_dir = \"{}\"\ncache_dir = \"{}\"\n\
                 [[environment]]\nname = \"t3\"\nroot = \"{}\"\ndriver = \"t3code\"\n\
                 driver_home = \"{}\"\n\
                 [[environment]]\nname = \"client-a\"\nroot = \"{}\"\ncheck = \"true\"\n",
                at("state"),
                at("cache"),
                at("work"),
                // A home of its own, with no driver in it: nothing of the
                // machine the tests run on is ever asked.
                at("no-driver-here"),
                at("nucleus"),
            ),
        )
        .unwrap();
        (dir, settings::load_from(&config).unwrap())
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_795_089_600 + seconds)
    }

    /// The daemon counting a claim a hook made from `folder`.
    fn claim(
        state_dir: &Path,
        driver_environment: &str,
        key: &str,
        environment: &str,
        session_id: &str,
        folder: Option<&Path>,
    ) {
        let claim = envelope::Claim {
            driver_env: driver_environment.into(),
            key: key.into(),
            folders: folder.map(folder_numbers).unwrap_or_default(),
        };
        note_claim(state_dir, &claim, environment, session_id);
    }

    /// Apply a look as the host takes it: settled into an answer, as inside
    /// the container, then held to what the host accepts.
    fn applied(
        settings: &Settings,
        environment: &Environment,
        memory: &mut Memory,
        result: Result<Look, Condition>,
        now: SystemTime,
    ) -> usize {
        let driver = environment.driver_adapter().unwrap().name();
        let answer = answer::settle(settings, environment, driver, result).check(driver);
        apply(settings, environment, memory, answer, now)
    }

    fn seen(id: &str, phase: Phase) -> Observed {
        Observed::new(id, Some(phase))
    }

    fn list(sessions: Vec<Observed>) -> Result<Look, Condition> {
        Ok(Look {
            sessions,
            complete: true,
            ..Look::default()
        })
    }

    /// Apply one look, and return what it wrote to the spool, in order, as
    /// (session, event type), leaving the spool empty.
    fn looked(
        settings: &Settings,
        memory: &mut Memory,
        result: Result<Look, Condition>,
        now: SystemTime,
    ) -> Vec<(String, String)> {
        let environment = settings.environment("t3").unwrap();
        applied(settings, environment, memory, result, now);
        let spool = Spool::new(&settings.spool_dir);
        spool
            .pending()
            .into_iter()
            .map(|id| {
                let record = spool.read(&id).expect("a record");
                spool.remove(&id);
                assert_eq!(record.event.env_id, "t3");
                let kind = serde_json::to_value(record.event.kind).unwrap();
                (record.event.session_id, kind.as_str().unwrap().to_string())
            })
            .collect()
    }

    fn events(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(session, kind)| (session.to_string(), kind.to_string()))
            .collect()
    }

    #[test]
    fn every_name_in_the_registry_finds_the_adapter_that_answers_to_it() {
        for name in NAMES {
            assert_eq!(by_name(name).map(|driver| driver.name()), Some(name));
        }
        assert!(by_name("another").is_none());
    }

    #[test]
    fn what_is_idle_when_a_driver_is_first_seen_is_history_and_what_is_busy_is_reported() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let first = looked(
            &settings,
            &mut memory,
            list(vec![
                seen("old-thread", Phase::Idle),
                seen("busy", Phase::Working),
                seen("asking", Phase::NeedsPermission),
                seen("over", Phase::Ended),
            ]),
            at(0),
        );
        assert_eq!(
            first,
            events(&[
                ("busy", "session_start"),
                ("busy", "user_prompt_submit"),
                ("asking", "session_start"),
                ("asking", "permission_prompt"),
            ])
        );
        // A session that turns up idle later is one that has just started.
        let later = looked(
            &settings,
            &mut memory,
            list(vec![
                seen("old-thread", Phase::Working),
                seen("busy", Phase::Working),
                seen("asking", Phase::NeedsPermission),
                seen("fresh", Phase::Idle),
            ]),
            at(15),
        );
        assert_eq!(
            later,
            events(&[
                ("old-thread", "user_prompt_submit"),
                ("fresh", "session_start"),
            ])
        );
    }

    #[test]
    fn each_change_of_phase_becomes_the_event_a_hook_would_have_recorded() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let mut step = |phase: Phase, seconds: u64| {
            looked(
                &settings,
                &mut memory,
                list(vec![seen("s1", phase)]),
                at(seconds),
            )
        };
        let kinds = |pairs: Vec<(String, String)>| -> Vec<String> {
            pairs.into_iter().map(|(_, kind)| kind).collect()
        };
        assert_eq!(
            kinds(step(Phase::Working, 0)),
            ["session_start", "user_prompt_submit"]
        );
        assert_eq!(kinds(step(Phase::Working, 15)), Vec::<String>::new());
        assert_eq!(kinds(step(Phase::NeedsInput, 30)), ["input_prompt"]);
        assert_eq!(kinds(step(Phase::NeedsInput, 45)), Vec::<String>::new());
        assert_eq!(
            kinds(step(Phase::NeedsElicitation, 50)),
            ["elicitation_prompt"]
        );
        // The owner answered: working again, as a tool call tells a hook.
        assert_eq!(kinds(step(Phase::Working, 60)), ["activity"]);
        assert_eq!(kinds(step(Phase::Idle, 75)), ["stop"]);
        assert_eq!(kinds(step(Phase::Working, 90)), ["user_prompt_submit"]);
        assert_eq!(kinds(step(Phase::Failed, 105)), ["stop_failure"]);
        assert_eq!(kinds(step(Phase::Failed, 120)), Vec::<String>::new());
        assert_eq!(kinds(step(Phase::Working, 135)), ["user_prompt_submit"]);
        assert_eq!(
            kinds(step(Phase::NeedsPermission, 150)),
            ["permission_prompt"]
        );
        assert_eq!(kinds(step(Phase::Idle, 165)), ["stop"]);
        assert_eq!(kinds(step(Phase::Ended, 180)), ["session_end"]);
        // An ended session the driver goes on listing is not news, and the
        // same id seen live again is a new session.
        assert_eq!(kinds(step(Phase::Ended, 195)), Vec::<String>::new());
        assert_eq!(kinds(step(Phase::Idle, 210)), ["session_start"]);
    }

    #[test]
    fn the_events_of_one_look_apply_in_order_to_the_sessions_the_box_keeps() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let environment = settings.environment("t3").unwrap();
        applied(
            &settings,
            environment,
            &mut memory,
            list(vec![seen("s1", Phase::NeedsPermission)]),
            at(0),
        );
        // Applied as the daemon applies them: oldest id first, and an event
        // with an id no newer than the last changes nothing.
        let spool = Spool::new(&settings.spool_dir);
        let mut session = None;
        for id in spool.pending() {
            let applied = sessions::apply(session, &spool.read(&id).unwrap()).unwrap();
            assert!(applied.changed);
            session = Some(applied.session);
        }
        let session = session.unwrap();
        assert_eq!(session.status, Status::NeedsInput);
        assert_eq!(session.needs_kind.as_deref(), Some("permission"));
    }

    #[test]
    fn a_failed_turn_carries_the_drivers_kind_and_reset_time_and_nothing_else_does() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let environment = settings.environment("t3").unwrap();
        let limited = |phase: Phase| Observed {
            error_kind: Some("usage_limit".into()),
            limit_resets_at: Some(at(3_600)),
            ..seen("s1", phase)
        };
        applied(
            &settings,
            environment,
            &mut memory,
            list(vec![limited(Phase::Failed)]),
            at(0),
        );
        let spool = Spool::new(&settings.spool_dir);
        let records: Vec<_> = spool
            .pending()
            .iter()
            .map(|id| spool.read(id).unwrap().event)
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, EventType::SessionStart);
        assert_eq!(records[0].error_kind, None);
        assert_eq!(records[0].limit_resets_at, None);
        assert_eq!(records[1].kind, EventType::StopFailure);
        assert_eq!(records[1].error_kind.as_deref(), Some("usage_limit"));
        assert!(records[1].limit_resets_at.is_some());
        // A driver session has no vendor id beside its own.
        assert_eq!(records[1].bridge_session_id, None);
    }

    #[test]
    fn a_session_remembered_without_a_word_is_never_ended_and_one_that_spoke_since_is() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        // Four old idle threads when the driver is first seen: history.
        let history = ["goes", "says-over", "worked-since", "hooked"];
        let idle = |ids: &[&str]| ids.iter().map(|id| seen(id, Phase::Idle)).collect();
        assert_eq!(
            looked(&settings, &mut memory, list(idle(&history)), at(0)),
            events(&[])
        );
        // One of them does a turn, and so is heard of.
        let mut now = idle(&history);
        now[2] = seen("worked-since", Phase::Working);
        assert_eq!(
            looked(&settings, &mut memory, list(now), at(15)),
            events(&[("worked-since", "user_prompt_submit")])
        );
        // The driver says one is over, a live hook turns out to report
        // another, and the third is no longer listed. Nobody was told any
        // of them began, and nobody is told they ended.
        hook_session(&settings, "vendor-1", &[EventType::SessionStart], at(20));
        let after = || {
            vec![
                seen("says-over", Phase::Ended),
                seen("worked-since", Phase::Idle),
                Observed {
                    agent_session_id: Some("vendor-1".into()),
                    ..seen("hooked", Phase::Idle)
                },
            ]
        };
        assert_eq!(
            looked(&settings, &mut memory, list(after()), at(30)),
            events(&[("worked-since", "stop")])
        );
        assert_eq!(
            looked(&settings, &mut memory, list(after()), at(45)),
            events(&[])
        );
        assert_eq!(
            memory.sessions.keys().collect::<Vec<_>>(),
            ["worked-since"],
            "what was never reported is forgotten all the same"
        );
        // The one that spoke is ended like any other session.
        looked(&settings, &mut memory, list(vec![]), at(60));
        assert_eq!(
            looked(&settings, &mut memory, list(vec![]), at(75)),
            events(&[("worked-since", "session_end")])
        );
    }

    #[test]
    fn a_session_is_ended_once_two_complete_lists_have_left_it_out() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let both = || vec![seen("stays", Phase::Working), seen("goes", Phase::Working)];
        let one = || vec![seen("stays", Phase::Working)];
        looked(&settings, &mut memory, list(both()), at(0));
        // One look can catch a driver between two states.
        assert_eq!(
            looked(&settings, &mut memory, list(one()), at(15)),
            events(&[])
        );
        // Seen again, it was never gone.
        assert_eq!(
            looked(&settings, &mut memory, list(both()), at(30)),
            events(&[])
        );
        assert_eq!(
            looked(&settings, &mut memory, list(one()), at(45)),
            events(&[])
        );
        // A list cut short says nothing about what it left out.
        let cut_short = Ok(Look {
            sessions: one(),
            complete: false,
            ..Look::default()
        });
        assert_eq!(
            looked(&settings, &mut memory, cut_short, at(60)),
            events(&[])
        );
        assert_eq!(
            looked(&settings, &mut memory, list(one()), at(75)),
            events(&[("goes", "session_end")])
        );
        assert!(!memory.sessions.contains_key("goes"));
        assert!(memory.sessions.contains_key("stays"));
    }

    #[test]
    fn a_look_that_finds_no_driver_changes_the_word_and_nothing_else() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        looked(
            &settings,
            &mut memory,
            list(vec![seen("s1", Phase::Working)]),
            at(0),
        );
        let before = memory.sessions.clone();
        for (condition, seconds) in [
            (Condition::Absent, 15),
            (Condition::Unreachable, 30),
            (Condition::Timeout, 45),
            (Condition::Rejected, 60),
            (Condition::NoToken, 75),
            (Condition::Unsupported, 90),
            (Condition::Error, 100),
            (Condition::BadReply, 105),
        ] {
            let wrote = looked(&settings, &mut memory, Err(condition), at(seconds));
            assert_eq!(wrote, events(&[]), "{condition:?} wrote an event");
            assert_eq!(memory.condition, condition);
            assert_eq!(memory.sessions, before, "{condition:?} forgot a session");
        }
        // When the word last changed, and when the driver was last found.
        assert_eq!(memory.since_ms, unix_ms(at(105)));
        assert_eq!(memory.good_at_ms, Some(unix_ms(at(0))));
        // The driver comes back, and the session is as it was left.
        let back = looked(
            &settings,
            &mut memory,
            list(vec![seen("s1", Phase::Idle)]),
            at(120),
        );
        assert_eq!(back, events(&[("s1", "stop")]));
        assert_eq!(memory.condition, Condition::Ok);
    }

    #[test]
    fn a_working_session_is_heard_from_once_in_a_beat_and_only_while_its_activity_grows() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let beat = ACTIVITY_BEAT.as_secs();
        let working = |activity: Option<u64>| Observed {
            activity,
            ..seen("s1", Phase::Working)
        };
        let mut step = |activity: Option<u64>, seconds: u64| {
            looked(
                &settings,
                &mut memory,
                list(vec![working(activity)]),
                at(seconds),
            )
            .len()
        };
        assert_eq!(step(Some(1), 0), 2);
        // Growing, but heard from too recently.
        assert_eq!(step(Some(5), 15), 0);
        assert_eq!(step(Some(9), beat), 1);
        assert_eq!(step(Some(12), beat + 15), 0);
        // A beat on, and nothing has happened: nothing is made up.
        assert_eq!(step(Some(12), 2 * beat + 15), 0);
        assert_eq!(step(Some(13), 2 * beat + 30), 1);
        // A driver that reports no activity is never spoken for.
        assert_eq!(step(None, 4 * beat), 0);
    }

    #[test]
    fn a_turn_that_came_and_went_between_two_looks_is_still_reported() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let turned = |id: &str, phase: Phase, turn: u64| Observed {
            turn: Some(turn),
            ..seen(id, phase)
        };
        looked(
            &settings,
            &mut memory,
            list(vec![
                turned("busy", Phase::Working, 3),
                turned("resting", Phase::Idle, 7),
            ]),
            at(0),
        );
        // `resting` was idle when first seen, so nothing was said of it.
        let wrote = looked(
            &settings,
            &mut memory,
            list(vec![
                turned("busy", Phase::Working, 4),
                turned("resting", Phase::Idle, 8),
            ]),
            at(15),
        );
        assert_eq!(
            wrote,
            events(&[
                ("busy", "stop"),
                ("busy", "user_prompt_submit"),
                ("resting", "user_prompt_submit"),
                ("resting", "stop"),
            ])
        );
        let same = looked(
            &settings,
            &mut memory,
            list(vec![
                turned("busy", Phase::Working, 4),
                turned("resting", Phase::Idle, 8),
            ]),
            at(30),
        );
        assert_eq!(same, events(&[]));
    }

    #[test]
    fn a_driver_with_no_opinion_leaves_a_session_as_it_was() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        looked(
            &settings,
            &mut memory,
            list(vec![seen("s1", Phase::Working)]),
            at(0),
        );
        let unsure = || list(vec![Observed::new("s1", None), Observed::new("new", None)]);
        assert_eq!(
            looked(&settings, &mut memory, unsure(), at(15)),
            events(&[])
        );
        assert_eq!(
            looked(&settings, &mut memory, unsure(), at(30)),
            events(&[])
        );
        // Listed with no opinion is still listed: it is not ended.
        assert_eq!(memory.sessions["s1"].phase, Phase::Working);
        assert!(!memory.sessions.contains_key("new"));
    }

    #[test]
    fn a_session_a_hook_already_reports_is_left_to_the_hook() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let thread = |phase: Phase| Observed {
            claim_key: Some("thread-7".into()),
            ..seen("thread-7", phase)
        };
        // The driver sees it first, and reports it.
        let first = looked(
            &settings,
            &mut memory,
            list(vec![thread(Phase::Working)]),
            at(0),
        );
        assert_eq!(first.len(), 2);
        // Then the agent's own hook reports the same agent, from inside it.
        claim(
            &settings.state_dir,
            "t3",
            "thread-7",
            "client-a",
            "hook-session",
            None,
        );
        let now = SystemTime::now();
        let hooked = looked(
            &settings,
            &mut memory,
            list(vec![thread(Phase::Working)]),
            now,
        );
        assert_eq!(hooked, events(&[("thread-7", "session_end")]));
        // While the hook's session lives, the driver says nothing of it.
        let store = Store::new(&settings.state_dir);
        let draft = Draft {
            env_id: "client-a".into(),
            session_id: "hook-session".into(),
            ..Draft::default()
        };
        let hook_event = |kind: EventType, ms: u64| Record {
            event: Envelope::new(kind, draft_like(&draft), now + Duration::from_millis(ms))
                .unwrap(),
            at_ms: unix_ms(now) + ms,
            workspace: None,
            process: None,
        };
        let started = sessions::apply(None, &hook_event(EventType::UserPromptSubmit, 1)).unwrap();
        store.save(&started.session).unwrap();
        let quiet = looked(
            &settings,
            &mut memory,
            list(vec![thread(Phase::NeedsInput)]),
            now,
        );
        assert_eq!(quiet, events(&[]));
        // The hook's session ends, and the driver's view is the only one.
        let ended =
            sessions::apply(Some(started.session), &hook_event(EventType::SessionEnd, 2)).unwrap();
        store.save(&ended.session).unwrap();
        let after = looked(
            &settings,
            &mut memory,
            list(vec![thread(Phase::NeedsInput)]),
            now,
        );
        assert_eq!(
            after,
            events(&[("thread-7", "session_start"), ("thread-7", "input_prompt")])
        );
        assert!(files_under(&claims_dir(&settings.state_dir, "t3")).is_empty());
    }

    #[test]
    fn an_agent_whose_own_session_a_hook_reports_is_left_to_the_hook_too() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let pane = |phase: Phase| Observed {
            agent_session_id: Some("agent-session-1".into()),
            ..seen("pane-3", phase)
        };
        // No hook reports that agent session: the driver's view is the one.
        let alone = looked(
            &settings,
            &mut memory,
            list(vec![pane(Phase::Working)]),
            at(0),
        );
        assert_eq!(alone.len(), 2);
        // A hook reports it, in another environment, and it has not ended.
        let now = at(15);
        let hook_event = |kind: EventType, ms: u64| Record {
            event: Envelope::new(
                kind,
                Draft {
                    env_id: "client-a".into(),
                    session_id: "agent-session-1".into(),
                    ..Draft::default()
                },
                now + Duration::from_millis(ms),
            )
            .unwrap(),
            at_ms: unix_ms(now) + ms,
            workspace: None,
            process: None,
        };
        let store = Store::new(&settings.state_dir);
        let started = sessions::apply(None, &hook_event(EventType::SessionStart, 0)).unwrap();
        store.save(&started.session).unwrap();
        let hooked = looked(
            &settings,
            &mut memory,
            list(vec![pane(Phase::Working)]),
            now,
        );
        assert_eq!(hooked, events(&[("pane-3", "session_end")]));
        let quiet = looked(
            &settings,
            &mut memory,
            list(vec![pane(Phase::NeedsInput)]),
            at(30),
        );
        assert_eq!(quiet, events(&[]));
        // A driver session whose own id is its agent's id is still the
        // driver's: once the daemon has applied its events, the session it
        // finds under that id is its own, and no hook's.
        let own = || Observed {
            agent_session_id: Some("solo".into()),
            ..seen("solo", Phase::Working)
        };
        let first = looked(&settings, &mut memory, list(vec![own()]), at(45));
        assert_eq!(first.len(), 2);
        let applied = Record {
            event: Envelope::new(
                EventType::UserPromptSubmit,
                Draft {
                    env_id: "t3".into(),
                    session_id: "solo".into(),
                    ..Draft::default()
                },
                at(45),
            )
            .unwrap(),
            at_ms: unix_ms(at(45)),
            workspace: None,
            process: None,
        };
        let known = sessions::apply(None, &applied).unwrap();
        store.save(&known.session).unwrap();
        let still = looked(&settings, &mut memory, list(vec![own()]), at(60));
        assert_eq!(still, events(&[]));
        assert!(memory.sessions.contains_key("solo"));
    }

    /// Save a hook-reported session in `client-a` as the daemon would have
    /// left it after these events.
    fn hook_session(settings: &Settings, id: &str, kinds: &[EventType], from: SystemTime) {
        let mut session = None;
        for (nth, kind) in kinds.iter().enumerate() {
            let at = from + Duration::from_millis(nth as u64);
            let draft = Draft {
                env_id: "client-a".into(),
                session_id: id.into(),
                ..Draft::default()
            };
            let record = Record {
                event: Envelope::new(*kind, draft, at).unwrap(),
                at_ms: unix_ms(at),
                workspace: None,
                process: None,
            };
            session = Some(sessions::apply(session, &record).unwrap().session);
        }
        Store::new(&settings.state_dir)
            .save(&session.unwrap())
            .unwrap();
    }

    /// Date a claim as if its hook had last spoken at `at`.
    fn hook_last_spoke(settings: &Settings, key: &str, session: &str, at: SystemTime) {
        let file = claims_dir(&settings.state_dir, "t3")
            .join(key)
            .join(format!("{session}.json"));
        fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[test]
    fn a_claim_whose_hook_has_gone_quiet_does_not_hide_what_runs_in_its_place() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let now = SystemTime::now();
        let later = |seconds: u64| now + Duration::from_secs(seconds);
        let slot = |phase: Phase| Observed {
            claim_key: Some("t7".into()),
            ..seen("t7-slot", phase)
        };
        // An agent with hooks ran in the driver's session, mid-turn, and was
        // killed: no `SessionEnd` hook ran, and its session never ended.
        hook_session(&settings, "killed", &[EventType::UserPromptSubmit], now);
        claim(&settings.state_dir, "t3", "t7", "client-a", "killed", None);
        hook_last_spoke(&settings, "t7", "killed", now - Duration::from_secs(3_600));

        // The session at rest contradicts nothing, however long it rests.
        for at in [0, 600] {
            let rest = looked(
                &settings,
                &mut memory,
                list(vec![slot(Phase::Idle)]),
                later(at),
            );
            assert_eq!(rest, events(&[]));
        }
        assert!(memory.doubts.is_empty());
        // Another agent, with no hooks, starts there and waits on its owner.
        // One look that sees it could be a hook about to speak...
        for at in [700, 760] {
            let doubted = looked(
                &settings,
                &mut memory,
                list(vec![slot(Phase::NeedsPermission)]),
                later(at),
            );
            assert_eq!(doubted, events(&[]));
        }
        assert!(memory.doubts.contains_key("t7"));
        // ...and when none has, the claim lapses and the wait is reported.
        let reported = looked(
            &settings,
            &mut memory,
            list(vec![slot(Phase::NeedsPermission)]),
            later(700 + CLAIM_QUIET.as_secs()),
        );
        assert_eq!(
            reported,
            events(&[
                ("t7-slot", "session_start"),
                ("t7-slot", "permission_prompt")
            ])
        );
        assert!(files_under(&claims_dir(&settings.state_dir, "t3")).is_empty());
        assert!(memory.doubts.is_empty());

        // Had the hook's agent been alive after all, its next word claims
        // the session again, and the hook wins again.
        claim(&settings.state_dir, "t3", "t7", "client-a", "killed", None);
        let again = looked(
            &settings,
            &mut memory,
            list(vec![slot(Phase::Working)]),
            SystemTime::now(),
        );
        assert_eq!(again, events(&[("t7-slot", "session_end")]));
    }

    #[test]
    fn a_hook_that_keeps_speaking_keeps_its_claim_whatever_the_driver_sees() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let now = SystemTime::now();
        let later = |seconds: u64| now + Duration::from_secs(seconds);
        let slot = |phase: Phase| Observed {
            claim_key: Some("t7".into()),
            ..seen("t7-slot", phase)
        };
        let mut look = |phase: Phase, at: u64| {
            looked(&settings, &mut memory, list(vec![slot(phase)]), later(at))
        };
        // A hooked agent asked its owner for something, an hour ago.
        hook_session(
            &settings,
            "waiting",
            &[EventType::UserPromptSubmit, EventType::PermissionPrompt],
            now,
        );
        claim(&settings.state_dir, "t3", "t7", "client-a", "waiting", None);
        hook_last_spoke(&settings, "t7", "waiting", now - Duration::from_secs(3_600));
        // Hook and driver say the same, and the hook's word stands.
        assert_eq!(look(Phase::NeedsPermission, 0), events(&[]));
        assert_eq!(look(Phase::NeedsPermission, 600), events(&[]));
        // The owner answers, and the agent works on with no hook to say so
        // until its tool call finishes. The driver sees it first.
        assert_eq!(look(Phase::Working, 700), events(&[]));
        // The hook speaks, a minute and a half on, and the doubt is gone...
        hook_last_spoke(&settings, "t7", "waiting", later(790));
        assert_eq!(
            look(Phase::Working, 700 + CLAIM_QUIET.as_secs()),
            events(&[])
        );
        // ...so a later doubt is counted from its own beginning.
        let quiet_again = 790 + CLAIM_QUIET.as_secs();
        assert_eq!(look(Phase::Working, quiet_again), events(&[]));
        assert_eq!(look(Phase::Working, quiet_again + 60), events(&[]));
        assert_eq!(files_under(&claims_dir(&settings.state_dir, "t3")).len(), 1);
    }

    #[test]
    fn a_hook_renews_a_claim_that_has_grown_old() {
        let (_dir, settings) = rig();
        let file = claims_dir(&settings.state_dir, "t3").join("t7/s1.json");
        let age = || {
            SystemTime::now()
                .duration_since(fs::metadata(&file).unwrap().modified().unwrap())
                .unwrap_or_default()
        };
        claim(&settings.state_dir, "t3", "t7", "client-a", "s1", None);
        hook_last_spoke(&settings, "t7", "s1", SystemTime::now() - CLAIM_QUIET * 3);
        assert!(age() >= CLAIM_QUIET);
        // The next hook event of the same session, from the same place.
        claim(&settings.state_dir, "t3", "t7", "client-a", "s1", None);
        assert!(age() < CLAIM_RENEW);
        assert_eq!(fs::read_to_string(&file).unwrap(), "client-a");
    }

    #[test]
    fn a_claim_does_not_hide_a_session_that_waits_or_fails_unless_its_hook_shows_the_same() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let now = SystemTime::now();
        let later = |seconds: u64| now + Duration::from_secs(seconds);
        // Something ran `nucleated hook` with the driver's variables: a
        // session that only ever started, and a claim it goes on renewing.
        hook_session(&settings, "decoy", &[EventType::SessionStart], now);
        claim(&settings.state_dir, "t3", "t7", "client-a", "decoy", None);
        let look = |memory: &mut Memory, phase: Phase, at: u64| {
            hook_last_spoke(&settings, "t7", "decoy", later(at));
            let slot = Observed {
                claim_key: Some("t7".into()),
                ..seen("t7-slot", phase)
            };
            looked(&settings, memory, list(vec![slot]), later(at))
        };
        // At work or at rest, the hook speaks for the session.
        assert_eq!(look(&mut memory, Phase::Working, 0), events(&[]));
        // It waits on its owner. A hook about to say so is waited for...
        assert_eq!(look(&mut memory, Phase::NeedsPermission, 15), events(&[]));
        assert_eq!(look(&mut memory, Phase::NeedsPermission, 60), events(&[]));
        // ...and when none has, the owner hears it from the driver, however
        // lately the hook has spoken of other things.
        assert_eq!(
            look(
                &mut memory,
                Phase::NeedsPermission,
                15 + CLAIM_UNHEARD.as_secs()
            ),
            events(&[
                ("t7-slot", "session_start"),
                ("t7-slot", "permission_prompt")
            ])
        );
        // The claim has not lapsed, and still does not take the report back.
        assert_eq!(look(&mut memory, Phase::NeedsPermission, 200), events(&[]));
        assert!(memory.sessions.contains_key("t7-slot"));
        assert_eq!(files_under(&claims_dir(&settings.state_dir, "t3")).len(), 1);
        // A failure the hook does not show is reported as it happens.
        assert_eq!(
            look(&mut memory, Phase::Failed, 215),
            events(&[("t7-slot", "stop_failure")])
        );
        // Once the hook's own session shows the failure, the owner has it
        // from the hook, and the driver stands aside again.
        hook_session(
            &settings,
            "decoy",
            &[EventType::SessionStart, EventType::StopFailure],
            now,
        );
        assert_eq!(
            look(&mut memory, Phase::Failed, 230),
            events(&[("t7-slot", "session_end")])
        );
        assert_eq!(look(&mut memory, Phase::Working, 245), events(&[]));
        assert!(memory.unheard.is_empty());
    }

    #[test]
    fn a_session_already_reported_as_waiting_is_not_ended_by_a_claim_that_says_nothing_of_it() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let now = SystemTime::now();
        let slot = || Observed {
            claim_key: Some("t7".into()),
            ..seen("t7-slot", Phase::NeedsInput)
        };
        let first = looked(&settings, &mut memory, list(vec![slot()]), now);
        assert_eq!(
            first,
            events(&[("t7-slot", "session_start"), ("t7-slot", "input_prompt")])
        );
        hook_session(&settings, "decoy", &[EventType::SessionStart], now);
        claim(&settings.state_dir, "t3", "t7", "client-a", "decoy", None);
        for seconds in [15, 300] {
            let after = looked(
                &settings,
                &mut memory,
                list(vec![slot()]),
                now + Duration::from_secs(seconds),
            );
            assert_eq!(after, events(&[]));
            assert!(memory.sessions.contains_key("t7-slot"));
        }
    }

    #[test]
    fn an_agents_own_session_id_does_not_hide_a_wait_its_hook_never_showed() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let pane = |phase: Phase| Observed {
            agent_session_id: Some("agent-session-1".into()),
            ..seen("pane-3", phase)
        };
        // A hook reports a session of that very id, at work.
        hook_session(
            &settings,
            "agent-session-1",
            &[EventType::UserPromptSubmit],
            at(0),
        );
        let mut look = |phase: Phase, seconds: u64| {
            looked(&settings, &mut memory, list(vec![pane(phase)]), at(seconds))
        };
        assert_eq!(look(Phase::Working, 0), events(&[]));
        assert_eq!(look(Phase::NeedsElicitation, 15), events(&[]));
        assert_eq!(
            look(Phase::NeedsElicitation, 15 + CLAIM_UNHEARD.as_secs()),
            events(&[
                ("pane-3", "session_start"),
                ("pane-3", "elicitation_prompt")
            ])
        );
        // The owner answers, and the hook's word is enough again.
        assert_eq!(
            look(Phase::Working, 200),
            events(&[("pane-3", "session_end")])
        );
    }

    #[test]
    fn a_claim_speaks_only_for_a_session_the_driver_places_where_its_hook_works() {
        let (dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let now = SystemTime::now();
        let work = dir.path().join("work");
        let here = work.join("SENTINEL named for a task");
        let inside = here.join("src");
        let elsewhere = work.join("another");
        for folder in [&inside, &elsewhere] {
            fs::create_dir_all(folder).unwrap();
        }
        hook_session(
            &settings,
            "hook-session",
            &[EventType::UserPromptSubmit],
            now,
        );
        let from = |folder: Option<&Path>| {
            claim(
                &settings.state_dir,
                "t3",
                "thread-7",
                "client-a",
                "hook-session",
                folder,
            );
        };
        let placed = Observed {
            claim_key: Some("thread-7".into()),
            cwd: Some(here.clone()),
            ..seen("thread-7", Phase::Working)
        };
        let mut look =
            |observed: &Observed| looked(&settings, &mut memory, list(vec![observed.clone()]), now);
        let reported = events(&[
            ("thread-7", "session_start"),
            ("thread-7", "user_prompt_submit"),
        ]);
        // A hook run with the thread's variable from some other folder is
        // not the thread's agent, and neither is one that names no folder.
        from(Some(&elsewhere));
        assert_eq!(look(&placed), reported);
        from(None);
        assert_eq!(look(&placed), events(&[]));
        // The agent's own hook, from a folder inside the thread's, is.
        let boxed = in_one_container(&settings);
        let claimed = hook_claim(
            &boxed,
            boxed.environment("client-a").unwrap(),
            "claude-code",
            Some(&inside),
            &|_| Some("thread-7".to_string()),
        )
        .unwrap();
        assert_eq!(claimed.driver_env, "t3");
        assert_eq!(claimed.key, "thread-7");
        note_claim(&settings.state_dir, &claimed, "client-a", "hook-session");
        assert_eq!(look(&placed), events(&[("thread-7", "session_end")]));
        // So is one from above it that is still inside the driver's root:
        // a driver may place a session by what its agent is running.
        from(Some(&work));
        assert_eq!(look(&placed), events(&[]));
        // From above the root, it could be anything on the box.
        from(Some(dir.path()));
        assert_eq!(look(&placed), reported);
        // A driver that does not say where a session works is taken at the
        // claim's word.
        let unplaced = Observed {
            cwd: None,
            ..placed.clone()
        };
        assert_eq!(look(&unplaced), events(&[("thread-7", "session_end")]));

        // Where the hook works is kept as numbers, never as a name.
        from(Some(&inside));
        let claims = files_under(&claims_dir(&settings.state_dir, "t3"));
        assert_eq!(claims.len(), 1);
        let kept = fs::read_to_string(&claims[0]).unwrap();
        assert!(!kept.contains("SENTINEL"), "{kept}");
        assert!(!kept.contains('/'), "{kept}");
        assert!(!claims[0].to_string_lossy().contains("SENTINEL"));
    }

    fn draft_like(draft: &Draft) -> Draft {
        Draft {
            env_id: draft.env_id.clone(),
            session_id: draft.session_id.clone(),
            ..Draft::default()
        }
    }

    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                found.extend(files_under(&entry.path()));
            } else {
                found.push(entry.path());
            }
        }
        found
    }

    #[test]
    fn a_claim_counts_only_from_inside_the_drivers_container() {
        let (_dir, mut settings) = rig();
        let claim = envelope::Claim {
            driver_env: "t3".into(),
            key: "thread-7".into(),
            folders: Vec::new(),
        };
        let counts = |settings: &Settings, from: Option<&str>, claim: &envelope::Claim| {
            claim_counts(
                settings,
                from.and_then(|name| settings.environment(name)),
                claim,
            )
        };
        // On the host, no environment runs in a container.
        assert!(!counts(&settings, Some("client-a"), &claim));
        for environment in &mut settings.environments {
            environment.unit = "agent-env-client-a.service".into();
        }
        assert!(counts(&settings, Some("client-a"), &claim));
        assert!(counts(&settings, Some("t3"), &claim));
        // From the shared folder, which says nothing of where it came from.
        assert!(!counts(&settings, None, &claim));
        // Naming an environment that has no driver, or that is not there.
        for driver_env in ["client-a", "client-z"] {
            let other = envelope::Claim {
                driver_env: driver_env.into(),
                ..claim.clone()
            };
            assert!(!counts(&settings, Some("client-a"), &other), "{driver_env}");
        }
        // The driver in a container of its own.
        let t3 = settings.environments.iter_mut().find(|e| e.name == "t3");
        t3.unwrap().unit = "agent-env-t3.service".into();
        assert!(!counts(&settings, Some("client-a"), &claim));
    }

    #[test]
    fn a_standing_claim_is_found_by_its_session_until_it_is_gone() {
        let (_dir, settings) = rig();
        let state = &settings.state_dir;
        assert_eq!(standing_claim(&settings, "client-a", "s1"), None);
        claim(state, "t3", "thread-7", "client-a", "s1", None);
        assert_eq!(
            standing_claim(&settings, "client-a", "s1"),
            Some(("t3".to_string(), "thread-7".to_string()))
        );
        // Another session, or the same id in another environment, has none.
        assert_eq!(standing_claim(&settings, "client-a", "s2"), None);
        assert_eq!(standing_claim(&settings, "t3", "s1"), None);
        fs::remove_dir_all(state.join("claims")).unwrap();
        assert_eq!(standing_claim(&settings, "client-a", "s1"), None);
    }

    #[test]
    fn a_session_that_claims_again_elsewhere_is_found_by_its_latest_claim() {
        let (_dir, settings) = rig();
        let state = &settings.state_dir;
        claim(state, "t3", "thread-7", "client-a", "s1", None);
        claim(state, "t3", "thread-8", "client-a", "s1", None);
        assert_eq!(
            standing_claim(&settings, "client-a", "s1"),
            Some(("t3".to_string(), "thread-8".to_string()))
        );
        claim(state, "t3", "thread-7", "client-a", "s1", None);
        assert_eq!(
            standing_claim(&settings, "client-a", "s1"),
            Some(("t3".to_string(), "thread-7".to_string()))
        );
    }

    #[test]
    fn a_driver_holds_claims_on_no_more_sessions_than_a_look_reports() {
        let (_dir, settings) = rig();
        let state = &settings.state_dir;
        for n in 0..answer::MAX_SESSIONS + 10 {
            claim(
                state,
                "t3",
                &format!("thread-{n}"),
                "client-a",
                &format!("s{n}"),
                None,
            );
        }
        assert_eq!(
            files_under(&claims_dir(state, "t3")).len(),
            answer::MAX_SESSIONS
        );
        let past = answer::MAX_SESSIONS + 5;
        assert_eq!(
            standing_claim(&settings, "client-a", &format!("s{past}")),
            None
        );
        // A session that holds a claim goes on renewing it.
        claim(state, "t3", "thread-0", "client-a", "s0", None);
        assert_eq!(
            standing_claim(&settings, "client-a", "s0"),
            Some(("t3".to_string(), "thread-0".to_string()))
        );
    }

    #[test]
    fn one_driver_session_holds_a_few_claims_and_no_more() {
        let (_dir, settings) = rig();
        let state = &settings.state_dir;
        for n in 0..100 {
            claim(state, "t3", "thread-7", "client-a", &format!("s{n}"), None);
        }
        assert_eq!(files_under(&claims_dir(state, "t3")).len(), 8);
        assert_eq!(standing_claim(&settings, "client-a", "s50"), None);
    }

    #[test]
    fn a_claim_forgotten_with_its_ended_session_leaves_no_note_behind() {
        let (_dir, settings) = rig();
        let state = &settings.state_dir;
        claim(state, "t3", "thread-7", "client-a", "s1", None);
        assert!(!files_under(&state.join("claimed")).is_empty());
        // Long past the grace, with no session the daemon ever applied.
        let held = claimed(&settings, "t3", SystemTime::now() + 2 * CLAIM_GRACE);
        assert!(held.is_empty());
        assert!(files_under(&claims_dir(state, "t3")).is_empty());
        assert!(files_under(&state.join("claimed")).is_empty());
    }

    /// `settings` with every environment in `client-a`'s container.
    fn in_one_container(settings: &Settings) -> Settings {
        let mut settings = settings.clone();
        for environment in &mut settings.environments {
            environment.unit = "agent-env-client-a.service".into();
        }
        settings
    }

    #[test]
    fn a_hook_says_which_drivers_session_it_runs_in_by_numbers_and_never_a_folder() {
        let (dir, settings) = rig();
        let settings = in_one_container(&settings);
        let own = settings.environment("client-a").unwrap();
        let inside = dir.path().join("work").join("SENTINEL named for a task");
        fs::create_dir_all(&inside).unwrap();
        let thread = |name: &str| (name == "T3CODE_THREAD_ID").then(|| "thread-7".to_string());
        let claimed = hook_claim(&settings, own, "claude-code", Some(&inside), &thread).unwrap();
        assert_eq!(claimed.driver_env, "t3");
        assert_eq!(claimed.key, "thread-7");
        assert_eq!(claimed.folders, folder_numbers(&inside));
        assert!(claimed.fits());
        assert!(!serde_json::to_string(&claimed)
            .unwrap()
            .contains("SENTINEL"));
        // No folder, no numbers; no driver's variable, no claim.
        assert!(hook_claim(&settings, own, "claude-code", None, &thread)
            .unwrap()
            .folders
            .is_empty());
        assert_eq!(
            hook_claim(&settings, own, "claude-code", Some(&inside), &|_| None),
            None
        );
    }

    #[test]
    fn a_hook_asks_only_the_drivers_in_its_own_container() {
        let (_dir, settings) = rig();
        let thread = |name: &str| (name == "T3CODE_THREAD_ID").then(|| "thread-7".to_string());
        let claim = |settings: &Settings| {
            let own = settings.environment("client-a").unwrap();
            hook_claim(settings, own, "claude-code", None, &thread)
        };
        // On the host, no claim the daemon would count is made.
        assert_eq!(claim(&settings), None);
        // T3 Code in its own container cannot be running this hook's agent.
        let mut apart = in_one_container(&settings);
        let t3 = apart.environments.iter_mut().find(|e| e.name == "t3");
        t3.unwrap().unit = "agent-env-t3.service".into();
        assert_eq!(claim(&apart), None);
        // A second T3 Code, listed later, in the hook's own container, is
        // the one asked.
        let mut second = apart.environments[0].clone();
        second.name = "t3-beside".into();
        second.unit = "agent-env-client-a.service".into();
        apart.environments.push(second);
        assert_eq!(claim(&apart).unwrap().driver_env, "t3-beside");
    }

    #[test]
    fn a_claim_is_a_key_and_two_ids_and_nothing_that_could_be_a_path() {
        let (_dir, settings) = rig();
        let state = &settings.state_dir;
        claim(state, "t3", "..", "client-a", "s1", None);
        claim(state, "t3", "a/b", "client-a", "s1", None);
        claim(state, "t3", "a sentence, not a key", "client-a", "s1", None);
        claim(state, "t3", "t7", "client-a", "../../escape", None);
        assert!(files_under(state).is_empty());
        claim(state, "t3", "t7", "client-a", "s1", None);
        claim(state, "t3", "t7", "client-a", "s1", None);
        let sorted = |mut files: Vec<PathBuf>| {
            files.sort();
            files
        };
        let one = [
            state.join("claimed/client-a/s1"),
            claims_dir(state, "t3").join("t7/s1.json"),
        ];
        assert_eq!(sorted(files_under(state)), one);
        // A claim for an environment the config does not have stands for
        // nothing, and is cleared away.
        claim(state, "t3", "t8", "gone", "s2", None);
        let held = claimed(&settings, "t3", SystemTime::now());
        assert_eq!(held.into_keys().collect::<Vec<_>>(), ["t7"]);
        assert_eq!(sorted(files_under(state)), one);
    }

    #[test]
    fn an_id_that_is_not_an_id_is_not_reported_and_the_folder_goes_no_further_than_the_box() {
        let (dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let folder = dir.path().join("work").join("SENTINEL private project");
        fs::create_dir_all(&folder).unwrap();
        let wrote = looked(
            &settings,
            &mut memory,
            Ok(Look {
                sessions: vec![
                    seen("SENTINEL the title of the thread", Phase::Working),
                    seen("thread/with/a/path", Phase::Working),
                    seen("", Phase::Working),
                    Observed {
                        cwd: Some(folder),
                        error_kind: Some("SENTINEL it went wrong because".into()),
                        ..seen("s1", Phase::Failed)
                    },
                    // The same id twice is one session: the first stands.
                    seen("s1", Phase::Working),
                ],
                complete: true,
                link_ref: Some("SENTINEL not an id".into()),
                facts: BTreeMap::from([("SENTINEL not a name", 7), ("described", 1)]),
                worktrees: vec![dir.path().join("SENTINEL a worktree named for a task")],
            }),
            at(0),
        );
        assert_eq!(
            wrote,
            events(&[("s1", "session_start"), ("s1", "stop_failure")])
        );
        assert_eq!(memory.link_ref, None);
        assert_eq!(memory.facts, BTreeMap::from([("described".to_string(), 1)]));
        assert_eq!(memory.sessions.keys().collect::<Vec<_>>(), ["s1"]);
        let kept = serde_json::to_string(&memory).unwrap();
        assert!(!kept.contains("SENTINEL"), "{kept}");
    }

    #[test]
    fn what_is_remembered_is_read_back_and_words_in_it_are_dropped() {
        let (_dir, settings) = rig();
        let environment = settings.environment("t3").unwrap();
        // The stub adapter finds no driver; the look is still written down.
        let memory = look(&settings, environment, at(0)).unwrap();
        assert_ne!(memory.condition, Condition::Ok);
        assert_eq!(read_memory(&settings.state_dir, "t3"), Some(memory));
        assert_eq!(
            look(&settings, settings.environment("client-a").unwrap(), at(0)),
            None
        );
        // The file is the box's own, and something else may have written it.
        fs::write(
            memory_file(&settings.state_dir, "t3"),
            r#"{"driver":"t3code","condition":"ok","link_ref":"an id with words in it",
                "facts":{"a sentence, not a name":1,"described":1},
                "sessions":{"a title, not an id":{"phase":"working"},"s1":{"phase":"idle"}}}"#,
        )
        .unwrap();
        let read = read_memory(&settings.state_dir, "t3").unwrap();
        assert_eq!(read.link_ref, None);
        assert_eq!(read.facts.keys().collect::<Vec<_>>(), ["described"]);
        assert_eq!(read.sessions.keys().collect::<Vec<_>>(), ["s1"]);
        // A fact is a number: words where one belongs make the file no
        // memory at all.
        fs::write(
            memory_file(&settings.state_dir, "t3"),
            r#"{"driver":"t3code","condition":"ok","facts":{"described":"it said yes"}}"#,
        )
        .unwrap();
        assert_eq!(read_memory(&settings.state_dir, "t3"), None);
        fs::write(
            memory_file(&settings.state_dir, "t3"),
            r#"{"driver":"t3code","condition":"it could not be reached because"}"#,
        )
        .unwrap();
        assert_eq!(read_memory(&settings.state_dir, "t3"), None);
    }

    #[test]
    fn what_an_adapter_learned_is_handed_back_until_a_look_says_otherwise() {
        let (_dir, settings) = rig();
        let mut memory = Memory::new("t3code");
        let learned = |facts: &[(&'static str, u64)]| {
            Ok(Look {
                complete: true,
                facts: facts.iter().copied().collect(),
                ..Look::default()
            })
        };
        looked(
            &settings,
            &mut memory,
            learned(&[("described", 1), ("families", 1)]),
            at(0),
        );
        let kept = |memory: &Memory| -> Vec<(String, u64)> {
            memory
                .facts
                .iter()
                .map(|(name, value)| (name.clone(), *value))
                .collect()
        };
        assert_eq!(
            kept(&memory),
            [("described".to_string(), 1), ("families".to_string(), 1)]
        );
        // A look that found no driver leaves them: the adapter is handed
        // them with the word for why, and decides for itself.
        looked(&settings, &mut memory, Err(Condition::Timeout), at(15));
        assert_eq!(kept(&memory).len(), 2);
        assert_eq!(memory.condition, Condition::Timeout);
        // The next good look's are the ones that stand.
        looked(&settings, &mut memory, learned(&[("described", 2)]), at(30));
        assert_eq!(kept(&memory), [("described".to_string(), 2)]);
        looked(&settings, &mut memory, learned(&[]), at(45));
        assert!(memory.facts.is_empty());
    }

    #[test]
    fn a_full_spool_is_left_alone_and_what_was_seen_is_seen_again() {
        let (_dir, mut settings) = rig();
        settings.spool_max_events = 1;
        let mut memory = Memory::new("t3code");
        fs::create_dir_all(&settings.spool_dir).unwrap();
        for name in ["a.json", "b.json"] {
            fs::write(settings.spool_dir.join(name), "{}").unwrap();
        }
        let environment = settings.environment("t3").unwrap();
        let wrote = applied(
            &settings,
            environment,
            &mut memory,
            list(vec![seen("s1", Phase::Working)]),
            at(0),
        );
        assert_eq!(wrote, 0);
        assert!(memory.sessions.is_empty());
        assert!(!memory.baseline);
    }

    #[test]
    fn a_driver_is_asked_on_this_machine_and_nowhere_else() {
        for local in [
            "http://127.0.0.1:3773",
            "http://127.0.0.1:3773/",
            "http://localhost:3773",
            "http://localhost",
            "http://[::1]:3773",
            "http://[::1]",
        ] {
            assert!(loopback_origin(local).is_some(), "{local}");
        }
        assert_eq!(
            loopback_origin(" http://127.0.0.1:3773/ ").as_deref(),
            Some("http://127.0.0.1:3773")
        );
        for elsewhere in [
            "https://127.0.0.1:3773",
            "http://192.168.1.20:3773",
            "http://app.t3.codes",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com:3773",
            "http://user:secret@127.0.0.1:3773",
            "http://127.0.0.1:3773/api/orchestration/shell",
            "http://127.0.0.1:3773?to=elsewhere",
            "http://127.0.0.1:",
            "http://127.0.0.1:port",
            "http://::1:3773",
            "http://[::1",
            "http://[2001:db8::1]:3773",
            "127.0.0.1:3773",
            "",
        ] {
            assert_eq!(loopback_origin(elsewhere), None, "{elsewhere}");
        }
    }

    #[test]
    fn a_driver_that_is_not_there_is_asked_ever_less_often_up_to_a_minute() {
        let secs = Duration::from_secs;
        assert_eq!(next_wait(secs(5), 0), secs(5));
        let waits: Vec<u64> = (1..=6).map(|n| next_wait(secs(5), n).as_secs()).collect();
        assert_eq!(waits, [5, 10, 20, 40, 60, 60]);
        assert_eq!(next_wait(secs(15), 3), secs(60));
        // An interval longer than a minute is never shortened by a failure.
        assert_eq!(next_wait(secs(120), 4), secs(120));
        assert_eq!(next_wait(secs(5), u32::MAX), secs(60));
    }
}
