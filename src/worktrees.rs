//! The session folders of a nucleus as Watch sees them, and the two things
//! Watch can ask the box to do with one: **Push**, which puts its branches on
//! GitHub, and **Clean up**, which removes it.
//!
//! - **The report.** The reporter builds it on the schedule it counts unsaved
//!   work on, and keeps it beside the count in `unsaved.json`. A heartbeat
//!   reads it there and runs no git.
//! - **The actions.** They come in the answer to a heartbeat. The daemon's
//!   loop only writes each one down. A thread of its own runs them, so a slow
//!   push never holds up a heartbeat, and writes down each result for the
//!   heartbeats that follow to carry, until one carrying it is answered.
//!
//! An action's id is written down before it runs and remembered for a day,
//! and it never runs twice. A result's detail is one plain sentence made
//! here from fixed words, repo names, counts and ref names: never git's own
//! text, a path, a host or a token.
//!
//! A folder's repos are the ones its manifest lists and the worktrees its
//! subagents made under `.subworktrees/`: each is reported, pushed, backed
//! up and removed like the others.

use crate::driver::Condition;
use crate::envelope::{rfc3339, unix_ms, valid_env_key};
use crate::housekeeping;
use crate::reporter;
use crate::repos::answer::Word;
use crate::repos::{self, could_not_know, transfer, Contained, Inside, Nucleus};
use crate::sessions::Store;
use crate::settings::{self, Environment, Settings};
use crate::workspace::{RepoInfo, Unsaved, Workspace};
use crate::{driver, git, paths, usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The most folders one environment reports, and repos one folder does.
pub const MAX_FOLDERS: usize = 200;
pub const MAX_REPOS: usize = 20;
/// The most results one heartbeat carries for one environment.
pub const MAX_RESULTS: usize = 50;
const MAX_DETAIL: usize = 300;
const MAX_UNPUSHED: u64 = 100_000;
const MAX_BRANCH: usize = 255;
/// The most files looked at to find when a folder last changed. A folder
/// with more is reported without the time.
const MAX_FILES: usize = 100_000;
/// How long the id of an action that has run is remembered.
const REMEMBER: Duration = Duration::from_secs(24 * 3_600);
/// An action not started this long after it was asked for, or after it
/// reached the box, is dropped unrun: Watch delivers none older.
const EXPIRES: Duration = Duration::from_secs(10 * 60);
/// How far the box's clock and Watch's may be apart.
const SKEW: Duration = Duration::from_secs(60);
/// The longest a push may take before it is stopped and counted failed.
const PUSH_TIMEOUT: Duration = Duration::from_secs(120);

// ---- the report -----------------------------------------------------------

/// One session folder of a nucleus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Its name under `.worktrees/`.
    pub folder: String,
    /// An agent session that has not ended names it, or a process has its
    /// working directory inside it, or the system cannot say.
    pub in_use: bool,
    /// The newest change to a file in it that git tracks or would track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_change_at: Option<String>,
    pub repos: Vec<WorktreeRepo>,
}

impl Worktree {
    /// Nothing in it is uncommitted or on no remote.
    pub fn is_saved(&self) -> bool {
        self.repos
            .iter()
            .all(|repo| !repo.uncommitted && repo.unpushed_commits == 0)
    }
}

/// One repo of a session folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRepo {
    /// `owner/name` for a repo on GitHub, else the repo's folder name.
    pub repo: String,
    /// The branch checked out; empty when `HEAD` is detached.
    pub branch: String,
    pub uncommitted: bool,
    /// Commits on no remote and in no merged pull request.
    pub unpushed_commits: u64,
    /// A merged pull request that holds `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_pull_request: Option<String>,
    /// An open pull request whose head is `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_pull_request: Option<String>,
}

/// `^[0-9A-Za-z._-]{1,128}$`, and neither `.` nor `..`.
pub fn valid_folder(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && !matches!(value, "." | "..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// `^[0-9A-Za-z._/-]{1,140}$`
pub(crate) fn valid_repo(value: &str) -> bool {
    (1..=140).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}

/// A character JavaScript's `\s` matches, as Watch checks with it.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// A branch as Watch takes one: at most 255 UTF-16 units, as JavaScript
/// counts a string's length, with nothing in it that JavaScript's `\s`
/// matches and no control character.
pub(crate) fn reportable_branch(value: &str) -> bool {
    value.encode_utf16().count() <= MAX_BRANCH
        && !value.chars().any(|c| js_space(c) || c.is_control())
}

/// The name a repo goes by in the report and in a result's detail: its
/// label, else the name it has in the folder. Nothing when neither is a
/// name Watch takes. A subagent's worktree has the label of the repo it is
/// a worktree of, and so is told as that repo.
fn repo_name(key: &str, info: &RepoInfo) -> Option<String> {
    [info.label.as_str(), key]
        .into_iter()
        .find(|name| valid_repo(name))
        .map(str::to_string)
}

/// Every worktree of a folder that is there: its repos and its subagents'.
pub(crate) fn worktrees_of(
    workspace: &Workspace,
    path: &Path,
) -> anyhow::Result<Vec<(String, RepoInfo)>> {
    Ok(workspace
        .session_worktrees(path)?
        .into_iter()
        .filter(|(_, info)| info.path.is_dir())
        .collect())
}

/// The newest modification time, in Unix milliseconds, of any file that git
/// tracks or would track in these worktrees. Nothing when git cannot list
/// them, or when there are too many to look at.
pub(crate) fn last_change(git: &git::Git, worktrees: &[&Path]) -> Option<u64> {
    let mut newest = None;
    let mut seen = 0;
    for worktree in worktrees {
        let (ok, listed, _) = git
            .tree_raw(
                worktree,
                &[
                    "ls-files",
                    "-z",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                ],
                &[],
            )
            .ok()?;
        if !ok {
            return None;
        }
        for name in listed
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            seen += 1;
            if seen > MAX_FILES {
                return None;
            }
            // A tracked file that was deleted has no time, and is no change
            // that can be dated.
            let path = worktree.join(String::from_utf8_lossy(name).as_ref());
            if let Ok(modified) = fs::symlink_metadata(&path).and_then(|meta| meta.modified()) {
                newest = newest.max(Some(unix_ms(modified)));
            }
        }
    }
    newest
}

/// Fold a repo into a folder's list: one of the same name, a subagent's
/// worktree of a repo already listed, adds what it holds to that entry.
fn add_repo(repos: &mut Vec<WorktreeRepo>, repo: WorktreeRepo) {
    match repos.iter_mut().find(|have| have.repo == repo.repo) {
        Some(have) => {
            have.uncommitted |= repo.uncommitted;
            have.unpushed_commits = have
                .unpushed_commits
                .saturating_add(repo.unpushed_commits)
                .min(MAX_UNPUSHED);
            have.merged_pull_request = have.merged_pull_request.take().or(repo.merged_pull_request);
            have.open_pull_request = have.open_pull_request.take().or(repo.open_pull_request);
        }
        None => repos.push(repo),
    }
}

/// One folder, or nothing when one of its worktrees cannot be read, a repo
/// has no name Watch takes, or it holds more repos than Watch takes: a
/// folder shown as holding less than it does would invite cleaning it up.
fn folder(
    workspace: &Workspace,
    store: &Store,
    sharing: &[String],
    name: &str,
    path: &Path,
) -> Option<Worktree> {
    let listed = workspace.session_worktrees(path).ok()?;
    let worktrees: Vec<&(String, RepoInfo)> = listed
        .iter()
        .filter(|(_, info)| info.path.is_dir())
        .collect();
    let sessions = housekeeping::agent_sessions(store, sharing, name);
    let in_use = housekeeping::in_use_because(&sessions, path).is_some();
    let git = workspace.git();
    let mut repos = Vec::new();
    for (key, info) in worktrees.iter().copied() {
        let unsaved = workspace.unsaved(&info.path).ok()?;
        let branch = git.tree(&info.path, &["branch", "--show-current"]).ok()?;
        // One that does not fit is sent blank.
        let branch = if reportable_branch(&branch) {
            branch
        } else {
            String::new()
        };
        let pulls = workspace.pull_requests(&info.path).unwrap_or_default();
        add_repo(
            &mut repos,
            WorktreeRepo {
                repo: repo_name(key, info)?,
                branch,
                uncommitted: unsaved.uncommitted,
                unpushed_commits: u64::from(unsaved.unpushed).min(MAX_UNPUSHED),
                merged_pull_request: pulls.merged,
                open_pull_request: pulls.open,
            },
        );
    }
    // Anything else at the folder's root is held nowhere else: the folder
    // is not saved, and says so under its own name.
    if !Workspace::other_files(path, &listed).is_empty() {
        add_repo(
            &mut repos,
            WorktreeRepo {
                repo: name.to_string(),
                branch: String::new(),
                uncommitted: true,
                unpushed_commits: 0,
                merged_pull_request: None,
                open_pull_request: None,
            },
        );
    }
    if repos.len() > MAX_REPOS {
        return None;
    }
    let paths: Vec<&Path> = worktrees
        .iter()
        .map(|(_, info)| info.path.as_path())
        .collect();
    Some(Worktree {
        folder: name.to_string(),
        in_use,
        last_change_at: last_change(git, &paths)
            .map(|ms| rfc3339(UNIX_EPOCH + Duration::from_millis(ms))),
        repos,
    })
}

/// Every session folder of a nucleus, at most [`MAX_FOLDERS`] of them, by
/// name. `sharing` names the environments whose nucleus it is: an agent
/// session of any of them working in a folder puts it in use.
pub fn report(workspace: &Workspace, store: &Store, sharing: &[String]) -> Vec<Worktree> {
    workspace
        .sessions()
        .into_iter()
        .filter(|(name, _)| valid_folder(name))
        .filter_map(|(name, path)| folder(workspace, store, sharing, &name, &path))
        .take(MAX_FOLDERS)
        .collect()
}

/// As [`report`], for a nucleus a container writes, from what the container
/// said of it: the folders it told of in full, at most [`MAX_FOLDERS`],
/// GitHub asked here about the commits it names, and whether one is in use
/// from this box's own store and processes.
pub(crate) fn report_answer(
    answer: &repos::answer::Answer,
    host: &Workspace,
    store: &Store,
    sharing: &[String],
    sessions: &Path,
) -> Vec<Worktree> {
    answer
        .folders
        .iter()
        .filter(|told| !told.gone && !told.unreadable)
        .filter_map(|told| folder_of_answer(told, host, store, sharing, sessions))
        .take(MAX_FOLDERS)
        .collect()
}

/// One folder a container told of, by [`folder`]'s rules: nothing when one
/// of its worktrees could not be read or it holds more repos than Watch
/// takes.
fn folder_of_answer(
    told: &repos::answer::Folder,
    host: &Workspace,
    store: &Store,
    sharing: &[String],
    sessions: &Path,
) -> Option<Worktree> {
    let agent_sessions = housekeeping::agent_sessions(store, sharing, &told.folder);
    let path = sessions.join(&told.folder);
    let in_use = housekeeping::in_use_because(&agent_sessions, &path).is_some();
    let mut repos = Vec::new();
    for worktree in &told.worktrees {
        let facts = worktree.facts.as_ref()?.taken();
        let unsaved = Unsaved::verdict(&facts, || {
            host.merged_on_github(facts.slug.as_deref(), &facts.head)
        });
        let pulls = host.pull_requests_from(&facts);
        add_repo(
            &mut repos,
            WorktreeRepo {
                repo: worktree.label.clone(),
                branch: worktree.branch.clone(),
                uncommitted: unsaved.uncommitted,
                unpushed_commits: u64::from(unsaved.unpushed).min(MAX_UNPUSHED),
                merged_pull_request: pulls.merged,
                open_pull_request: pulls.open,
            },
        );
    }
    if told.other_files {
        add_repo(
            &mut repos,
            WorktreeRepo {
                repo: told.folder.clone(),
                branch: String::new(),
                uncommitted: true,
                unpushed_commits: 0,
                merged_pull_request: None,
                open_pull_request: None,
            },
        );
    }
    if repos.len() > MAX_REPOS {
        return None;
    }
    Some(Worktree {
        folder: told.folder.clone(),
        in_use,
        last_change_at: told
            .last_change_ms
            .map(|ms| rfc3339(UNIX_EPOCH + Duration::from_millis(ms))),
        repos,
    })
}

// ---- actions and their results --------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Push,
    CleanUp,
}

/// What Watch asked the box to do with one folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub env_id: String,
    pub folder: String,
    pub kind: Kind,
    /// Clean up only: unsaved work may go, once it is backed up.
    #[serde(default)]
    pub discard: bool,
    #[serde(default)]
    pub requested_at: String,
}

/// `^wta_[0-9a-z]{16}$`
pub(crate) fn valid_id(value: &str) -> bool {
    value.strip_prefix("wta_").is_some_and(|rest| {
        rest.len() == 16
            && rest
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

impl Action {
    /// True for an action whose id, environment and folder may be used to
    /// name files and find a folder.
    fn valid(&self) -> bool {
        valid_id(&self.id) && valid_env_key(&self.env_id) && valid_folder(&self.folder)
    }
}

/// The actions in an answer to a heartbeat. What is not a list is none, and
/// an entry that is not a well-formed action is passed over: nothing here
/// can fail the answer it is part of.
pub fn actions_from(value: &Value) -> Vec<Action> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| serde_json::from_value::<Action>(entry.clone()).ok())
        .filter(Action::valid)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Refused,
    Failed,
}

/// How an action ended, for the heartbeats that follow to carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub id: String,
    pub outcome: Outcome,
    /// One short plain sentence for a person.
    pub detail: String,
    pub finished_at: String,
}

/// An action as written down when it reached the box.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Queued {
    action: Action,
    received_ms: u64,
}

fn base(state_dir: &Path) -> PathBuf {
    state_dir.join("worktree-actions")
}

fn queue_dir(state_dir: &Path) -> PathBuf {
    base(state_dir).join("queue")
}

fn results_dir(state_dir: &Path, env: &str) -> PathBuf {
    base(state_dir).join("results").join(env)
}

fn result_file(state_dir: &Path, env: &str, id: &str) -> PathBuf {
    results_dir(state_dir, env).join(format!("{id}.json"))
}

fn started_file(state_dir: &Path) -> PathBuf {
    base(state_dir).join("started.json")
}

/// The ids of the actions that have been started, each with when, written
/// down before it ran. A file that is there and cannot be read is an error,
/// never "none": read as none, it would let an action run a second time.
fn read_started(state_dir: &Path) -> anyhow::Result<BTreeMap<String, u64>> {
    match fs::read_to_string(started_file(state_dir)) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

/// Write down that an action is starting, before anything of it runs.
fn remember_started(state_dir: &Path, id: &str, now: SystemTime) -> anyhow::Result<()> {
    let now_ms = unix_ms(now);
    let mut started = read_started(state_dir)?;
    let remember = REMEMBER.as_millis() as u64;
    started.retain(|_, at| now_ms.saturating_sub(*at) < remember);
    started.insert(id.to_string(), now_ms);
    paths::write_private(
        &started_file(state_dir),
        serde_json::to_string(&started)?.as_bytes(),
    )
}

/// Write down the actions Watch has just sent, each once: one that has been
/// started, or is already waiting, is passed over, and so is one for an
/// environment this box does not have. Only the daemon's loop calls this,
/// and it runs nothing.
pub fn take(settings: &Settings, actions: &[Action], now: SystemTime) {
    if actions.is_empty() {
        return;
    }
    // Taken even when the record cannot be read: the thread that runs them
    // then refuses each, and runs none.
    let started = read_started(&settings.state_dir).unwrap_or_default();
    for action in actions.iter().filter(|action| action.valid()) {
        if started.contains_key(&action.id) || settings.environment(&action.env_id).is_none() {
            continue;
        }
        let file = queue_dir(&settings.state_dir).join(format!("{}.json", action.id));
        if file.exists() {
            continue;
        }
        let queued = Queued {
            action: action.clone(),
            received_ms: unix_ms(now),
        };
        if let Ok(text) = serde_json::to_string(&queued) {
            let _ = paths::write_private(&file, text.as_bytes());
        }
    }
}

/// The results waiting to be sent for one environment, oldest first, at most
/// [`MAX_RESULTS`]. Nothing when there are none. A result file that cannot
/// be read becomes a failed result, said so, and is sent like any other; a
/// file not named for an action is removed.
pub fn results(state_dir: &Path, env: &str) -> Option<Vec<ActionResult>> {
    if !valid_env_key(env) {
        return None;
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(results_dir(state_dir, env)).ok()?.flatten() {
        let path = entry.path();
        let read = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<ActionResult>(&text).ok())
            .filter(|result| valid_id(&result.id));
        if let Some(result) = read {
            found.push(result);
            continue;
        }
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .filter(|stem| valid_id(stem) && path.extension().is_some_and(|ext| ext == "json"));
        let Some(id) = id else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let lost = ActionResult {
            id,
            outcome: Outcome::Failed,
            detail: "The box lost this action's result.".to_string(),
            finished_at: rfc3339(SystemTime::now()),
        };
        // In its place, so that it goes once Watch has heard it.
        if let Ok(text) = serde_json::to_string(&lost) {
            let _ = paths::write_private(&path, text.as_bytes());
        }
        found.push(lost);
    }
    for result in &mut found {
        result.detail = fit(std::mem::take(&mut result.detail), String::new);
    }
    found.sort_by(|a, b| (&a.finished_at, &a.id).cmp(&(&b.finished_at, &b.id)));
    found.truncate(MAX_RESULTS);
    (!found.is_empty()).then_some(found)
}

/// Forget results that a heartbeat Watch has answered carried.
pub fn forget(state_dir: &Path, env: &str, results: &[ActionResult]) {
    if !valid_env_key(env) {
        return;
    }
    for result in results.iter().filter(|result| valid_id(&result.id)) {
        let _ = fs::remove_file(result_file(state_dir, env, &result.id));
    }
}

/// Write an action's result down, unless one is there already: a result is
/// never written over.
fn write_result(state_dir: &Path, env: &str, result: &ActionResult) -> anyhow::Result<()> {
    let file = result_file(state_dir, env, &result.id);
    if file.exists() {
        return Ok(());
    }
    paths::write_private(&file, serde_json::to_string(result)?.as_bytes())
}

// ---- running an action ----------------------------------------------------

/// What an action came to.
struct Ended {
    outcome: Outcome,
    detail: String,
}

fn ended(outcome: Outcome, detail: impl Into<String>) -> Ended {
    Ended {
        outcome,
        detail: detail.into(),
    }
}

/// `text` when it is short enough for a detail, else `shorter`, as one line
/// with no control character in it. Length is counted as Watch counts it,
/// in UTF-16 units, and anything over is cut at a character's edge.
fn fit(text: String, shorter: impl FnOnce() -> String) -> String {
    let line = |text: String| -> String {
        let mut units = 0;
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take_while(|c| {
                units += c.len_utf16();
                units <= MAX_DETAIL
            })
            .collect()
    };
    if text.encode_utf16().count() <= MAX_DETAIL {
        line(text)
    } else {
        line(shorter())
    }
}

/// `a`, `a and b`, `a, b and c`.
fn listed(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn commits(count: u32) -> String {
    match count {
        1 => "1 commit".to_string(),
        n => format!("{n} commits"),
    }
}

/// Why a push failed, from git's own words, said plainly. Nothing git said
/// is passed on: it may name the remote's address, and through it a token.
#[cfg(test)]
fn push_failure(stderr: &str, repo: &str) -> String {
    format!(
        "{repo} was not pushed: {}.",
        push_why(transfer::word(stderr))
    )
}

/// Why a push failed, in the words a result uses for its word.
fn push_why(word: Word) -> &'static str {
    match word {
        Word::TimedOut => "it took too long",
        Word::Rejected => "the branch on GitHub has commits this one does not",
        Word::SignIn => "GitHub did not accept the box's sign-in",
        // A proxy that would not go to GitHub is GitHub not being reached,
        // where a result does not say so in a sentence of its own.
        Word::Unreachable | Word::NoNetwork => "GitHub could not be reached",
        Word::Refused => "GitHub refused it",
        Word::BadBranch => "its branch has a name that cannot be pushed safely",
        Word::CannotRun => "git cannot be run there",
        _ => "git could not push it",
    }
}

/// True for a branch name that can go into a refspec as it is: git takes it
/// as a branch, and it cannot be read as an option or a forced update.
fn pushable_branch(git: &git::Git, worktree: &Path, branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with(['-', '+'])
        && git
            .try_tree(worktree, &["check-ref-format", "--branch", branch])
            .is_ok_and(|out| out.ok && out.stdout == branch)
}

/// What a Push came to in one worktree of a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushOne {
    /// The name it goes by in a result.
    pub name: String,
    /// The commits pushed.
    pub pushed: u32,
    pub uncommitted: bool,
    /// Why it was not pushed, or not read.
    pub failure: Option<Word>,
}

/// Push the branch of each worktree that holds commits on no remote, each
/// within its time.
pub(crate) fn push_each(
    workspace: &Workspace,
    worktrees: &[(String, RepoInfo)],
    each: &mut dyn FnMut(&PushOne),
) -> Vec<PushOne> {
    let mut done = Vec::new();
    for (key, info) in worktrees {
        let one = push_one(workspace, key, info);
        each(&one);
        done.push(one);
    }
    done
}

fn push_one(workspace: &Workspace, key: &str, info: &RepoInfo) -> PushOne {
    let git = workspace.git();
    let name = repo_name(key, info).unwrap_or_else(|| "a repo".to_string());
    let Ok(unsaved) = workspace.unsaved(&info.path) else {
        return PushOne {
            name,
            pushed: 0,
            uncommitted: false,
            failure: Some(Word::Unreadable),
        };
    };
    let mut one = PushOne {
        name,
        pushed: 0,
        uncommitted: unsaved.uncommitted,
        failure: None,
    };
    if unsaved.unpushed == 0 {
        return one;
    }
    // A detached HEAD is on no branch to push.
    let branch = git
        .tree(&info.path, &["branch", "--show-current"])
        .unwrap_or_default();
    if branch.is_empty() {
        return one;
    }
    if !pushable_branch(git, &info.path, &branch) {
        one.failure = Some(Word::BadBranch);
        return one;
    }
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let args = ["-q", "-u", "origin", "--", refspec.as_str()];
    match transfer::push(git, &info.path, &args, Some(PUSH_TIMEOUT)) {
        Ok(out) if out.ok => one.pushed = unsaved.unpushed,
        Ok(out) => one.failure = Some(transfer::word(&out.stderr)),
        Err(_) => one.failure = Some(Word::CannotRun),
    }
    one
}

/// What a Push came to, as one result. With `network`, a push the
/// environment's network would not take to GitHub is said so, for the
/// whole of it; `cut_short` says the rest was not done.
fn pushed_ended(done: &[PushOne], network: bool, cut_short: bool) -> Ended {
    if network && done.iter().any(|one| one.failure == Some(Word::NoNetwork)) {
        return ended(Outcome::Failed, repos::NO_NETWORK);
    }
    let mut pushed: Vec<(String, u32)> = Vec::new();
    let mut uncommitted: Vec<String> = Vec::new();
    let mut failures = Vec::new();
    for one in done {
        let name = &one.name;
        if one.uncommitted && !uncommitted.contains(name) {
            uncommitted.push(name.clone());
        }
        match one.failure {
            Some(Word::Unreadable) => failures.push(format!("{name} could not be read.")),
            Some(word) => failures.push(format!("{name} was not pushed: {}.", push_why(word))),
            None if one.pushed > 0 => match pushed.iter_mut().find(|(have, _)| have == name) {
                Some((_, count)) => *count = count.saturating_add(one.pushed),
                None => pushed.push((name.clone(), one.pushed)),
            },
            None => {}
        }
    }
    if cut_short {
        failures.push("The rest was not pushed: it took too long.".to_string());
    }
    if !failures.is_empty() {
        let count = failures.len();
        return ended(
            Outcome::Failed,
            fit(failures.join(" "), || {
                format!("{count} repos were not pushed.")
            }),
        );
    }
    if pushed.is_empty() {
        return ended(Outcome::Refused, "There is nothing to push.");
    }
    let total = pushed
        .iter()
        .fold(0u32, |sum, (_, count)| sum.saturating_add(*count));
    let repos = pushed.len();
    let parts: Vec<String> = pushed
        .iter()
        .map(|(name, count)| format!("{} in {name}", commits(*count)))
        .collect();
    let mut text = format!("Pushed {}.", listed(&parts));
    match uncommitted.as_slice() {
        [] => {}
        [one] => text.push_str(&format!(" {one} has changes that are not committed.")),
        many => text.push_str(&format!(
            " {} have changes that are not committed.",
            listed(many)
        )),
    }
    ended(
        Outcome::Done,
        fit(text, || {
            format!("Pushed {} in {repos} repos.", commits(total))
        }),
    )
}

/// What a repo holds that is not on GitHub, in the words Watch uses.
fn unsaved_words(unsaved: &Unsaved, name: &str) -> Vec<String> {
    let mut words = Vec::new();
    if unsaved.unpushed > 0 {
        words.push(format!(
            "{} not on GitHub in {name}",
            commits(unsaved.unpushed)
        ));
    }
    if unsaved.uncommitted {
        words.push(format!("changes not committed in {name}"));
    }
    words
}

/// A worktree as it stood at one moment: its `HEAD`, and the tree of all it
/// holds, uncommitted and untracked files included.
#[derive(PartialEq, Eq)]
struct Standing {
    head: String,
    tree: String,
}

fn standing(git: &git::Git, info: &RepoInfo, keys: &Path, plain: bool) -> anyhow::Result<Standing> {
    Ok(Standing {
        head: git.tree(&info.path, &["rev-parse", "HEAD"])?,
        tree: housekeeping::working_tree(git, &info.path, keys, "backup", plain)?,
    })
}

/// True when a tree holds a nested repository that `HEAD` does not: another
/// repository inside the worktree, which a commit can only point at, not
/// hold.
fn new_gitlinks(git: &git::Git, worktree: &Path, tree: &str) -> anyhow::Result<bool> {
    let links = |object: &str| -> anyhow::Result<Vec<String>> {
        let (ok, listed, error) = git.tree_raw(worktree, &["ls-tree", "-r", "-z", object], &[])?;
        if !ok {
            anyhow::bail!("git ls-tree failed: {error}");
        }
        Ok(listed
            .split(|byte| *byte == 0)
            .map(|entry| String::from_utf8_lossy(entry).to_string())
            .filter(|entry| entry.starts_with("160000 "))
            .collect())
    };
    let before = links("HEAD")?;
    Ok(links(tree)?.iter().any(|link| !before.contains(link)))
}

/// The ref a discard's backup of `branch` goes to: `refs/wip/`, the branch
/// with its slashes made hyphens, and the action's id. A git host keeps a
/// ref as a file, whose name may be at most 255 bytes, so a long branch name
/// is cut, at a character's edge, leaving room for git's `.lock` beside it;
/// the id keeps it the action's own.
fn backup_ref(branch: &str, id: &str) -> String {
    let wip = housekeeping::wip_ref(branch);
    let dashed = wip.strip_prefix("refs/wip/").unwrap_or(&wip);
    let room = 255 - ".lock".len() - id.len() - 1;
    let mut cut = dashed.len().min(room);
    while !dashed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("refs/wip/{}-{id}", &dashed[..cut])
}

/// The ref a backup went to, or why it could not be made.
enum Backup {
    Pushed(String),
    Failed(Word),
}

/// Push a backup of one worktree's unsaved work to a ref of this action's
/// own, `refs/wip/<branch>-<action id>`, that nothing else writes: its
/// commits and, as a commit of their own made on no branch, its uncommitted
/// changes. Never over anything already there. `usable` says whether a ref
/// may be pushed to.
fn back_up(
    git: &git::Git,
    info: &RepoInfo,
    seen: &Standing,
    id: &str,
    usable: &dyn Fn(&str) -> bool,
) -> anyhow::Result<Backup> {
    let current = git.tree(&info.path, &["branch", "--show-current"])?;
    let branch = if current.is_empty() {
        info.branch.clone()
    } else {
        current
    };
    let reference = backup_ref(&branch, id);
    if !pushable_branch(git, &info.path, &branch) || !usable(&reference) {
        return Ok(Backup::Failed(Word::BadBranch));
    }
    if new_gitlinks(git, &info.path, &seen.tree)? {
        return Ok(Backup::Failed(Word::NestedRepo));
    }
    let head_tree = git.tree(&info.path, &["rev-parse", "HEAD^{tree}"])?;
    let commit = if seen.tree == head_tree {
        seen.head.clone()
    } else {
        housekeeping::wip_commit(git, &info.path, &seen.tree, &branch, SystemTime::now())?
    };
    let refspec = format!("{commit}:{reference}");
    let pushed = transfer::push(
        git,
        &info.path,
        &["-q", "origin", "--", refspec.as_str()],
        Some(PUSH_TIMEOUT),
    )?;
    Ok(match (pushed.ok, transfer::word(&pushed.stderr)) {
        (true, _) => Backup::Pushed(reference),
        (false, Word::NoNetwork) => Backup::Failed(Word::NoNetwork),
        (false, _) => Backup::Failed(Word::NotPushed),
    })
}

/// What a Clean up came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cleaned {
    /// Removed, after these backups: (repo, ref).
    Removed { backed_up: Vec<(String, String)> },
    /// Kept: what its repos hold that is not on GitHub, and whether it
    /// holds files outside them.
    Holding {
        holding: Vec<(String, Unsaved)>,
        other_files: bool,
    },
    /// Not removed, for `why`, about `repo` when one, after these backups.
    Failed {
        why: Word,
        repo: Option<String>,
        backed_up: Vec<(String, String)>,
    },
    /// Its removal was not started: its time was up.
    NotStarted,
}

/// Around the one step that removes a folder: `begin` says whether it may
/// start, which it may not once the time it was given is up, and `end`
/// that it is over. A removal begun is finished.
pub(crate) struct Committing<'a> {
    pub begin: &'a dyn Fn() -> bool,
    pub end: &'a dyn Fn(),
}

/// The removal of a folder whose time never runs out.
pub(crate) const UNHURRIED: Committing<'static> = Committing {
    begin: &|| true,
    end: &|| {},
};

impl Committing<'_> {
    fn within<T>(&self, step: impl FnOnce() -> T) -> Option<T> {
        if !(self.begin)() {
            return None;
        }
        let done = step();
        (self.end)();
        Some(done)
    }
}

/// How a Clean up is done: with `discard`, the action's id, unsaved work
/// may go once it is backed up; `in_use` is asked just before anything is
/// removed; the copies of the index go to `keys`, `plain` or not; and a
/// backup may only go to a ref `usable` takes.
pub(crate) struct Cleaning<'a> {
    pub discard: Option<&'a str>,
    pub in_use: &'a dyn Fn() -> bool,
    pub keys: &'a Path,
    pub plain: bool,
    pub usable: &'a dyn Fn(&str) -> bool,
    pub committing: &'a Committing<'a>,
}

pub(crate) fn clean_up_core(workspace: &Workspace, path: &Path, how: &Cleaning) -> Cleaned {
    let git = workspace.git();
    let unreadable = || Cleaned::Failed {
        why: Word::Unreadable,
        repo: None,
        backed_up: Vec::new(),
    };
    // The session's worktrees as listed, those gone included: what the
    // removal is checked against at the end.
    let Ok(as_listed) = workspace.session_worktrees(path) else {
        return unreadable();
    };
    let other = Workspace::other_files(path, &as_listed);
    let worktrees: Vec<(String, RepoInfo)> = as_listed
        .iter()
        .filter(|(_, info)| info.path.is_dir())
        .cloned()
        .collect();
    let mut holding = Vec::new();
    for (key, info) in &worktrees {
        let name = repo_name(key, info).unwrap_or_else(|| "a repo".to_string());
        match workspace.unsaved(&info.path) {
            Ok(found) if found.is_saved() => {}
            Ok(found) => holding.push((name, info, found)),
            Err(_) => return unreadable(),
        }
    }
    if holding.is_empty() && other.is_empty() {
        // Removed only if, looked at again, it still holds nothing unsaved.
        return match how.committing.within(|| workspace.remove_session(path)) {
            None => Cleaned::NotStarted,
            Some(Ok(_)) => Cleaned::Removed {
                backed_up: Vec::new(),
            },
            Some(Err(_)) => Cleaned::Failed {
                why: Word::NotRemoved,
                repo: None,
                backed_up: Vec::new(),
            },
        };
    }
    let Some(id) = how.discard else {
        return Cleaned::Holding {
            holding: holding
                .into_iter()
                .map(|(name, _, found)| (name, found))
                .collect(),
            other_files: !other.is_empty(),
        };
    };
    // Files outside its repos are in no repo to back up to: they are not
    // dropped.
    if !other.is_empty() {
        return Cleaned::Failed {
            why: Word::OtherFiles,
            repo: None,
            backed_up: Vec::new(),
        };
    }
    // How each worktree stands before anything is pushed: what is backed up,
    // and what it is compared with just before anything is removed.
    let mut before = Vec::new();
    for (_, info) in &worktrees {
        match standing(git, info, how.keys, how.plain) {
            Ok(seen) => before.push(seen),
            Err(_) => return unreadable(),
        }
    }
    // Every backup is pushed before anything is removed, and one that fails
    // removes nothing.
    let mut backed_up = Vec::new();
    for (name, info, _) in &holding {
        let Some(at) = worktrees
            .iter()
            .position(|(_, have)| have.path == info.path)
        else {
            return unreadable();
        };
        let why = match back_up(git, info, &before[at], id, how.usable) {
            Ok(Backup::Pushed(reference)) => {
                backed_up.push((name.clone(), reference));
                continue;
            }
            Ok(Backup::Failed(word)) => word,
            Err(_) => Word::NotMade,
        };
        return Cleaned::Failed {
            why,
            repo: Some(name.clone()),
            backed_up: Vec::new(),
        };
    }
    // Looked at once more just before the removal. A session that came
    // back to it, a repo added or gone, or a commit or a change made since
    // the backup, and nothing is removed.
    let changed = (how.in_use)()
        || worktrees_of(workspace, path).map_or(true, |now| {
            now.len() != worktrees.len()
                || now.iter().zip(&worktrees).zip(&before).any(
                    |(((_, now_info), (_, then_info)), seen)| {
                        now_info.path != then_info.path
                            || standing(git, now_info, how.keys, how.plain)
                                .map_or(true, |now| &now != seen)
                    },
                )
        });
    if changed {
        return Cleaned::Failed {
            why: Word::Changed,
            repo: None,
            backed_up,
        };
    }
    match how
        .committing
        .within(|| workspace.remove_session_as_listed(path, &as_listed, true))
    {
        None => Cleaned::NotStarted,
        Some(Ok(_)) => Cleaned::Removed { backed_up },
        Some(Err(_)) => Cleaned::Failed {
            why: Word::NotRemoved,
            repo: None,
            backed_up,
        },
    }
}

/// What a Clean up came to, as one result. With `network`, a backup the
/// environment's network would not take to GitHub is said so.
fn cleaned_ended(cleaned: &Cleaned, network: bool) -> Ended {
    let backed = |backed_up: &[(String, String)]| {
        let parts: Vec<String> = backed_up
            .iter()
            .map(|(name, reference)| format!("{reference} in {name}"))
            .collect();
        let repos = parts.len();
        fit(format!("Backed up to {}.", listed(&parts)), || {
            format!("Backed up {repos} repos to GitHub.")
        })
    };
    match cleaned {
        Cleaned::Removed { backed_up } if backed_up.is_empty() => {
            ended(Outcome::Done, "Cleaned up.")
        }
        Cleaned::Removed { backed_up } => ended(Outcome::Done, backed(backed_up)),
        Cleaned::Holding {
            holding,
            other_files,
        } => {
            let mut words: Vec<String> = holding
                .iter()
                .flat_map(|(name, found)| unsaved_words(found, name))
                .collect();
            if *other_files {
                words.push("files outside its repos".to_string());
            }
            let repos = holding.len();
            ended(
                Outcome::Refused,
                fit(format!("It holds {}.", listed(&words)), || {
                    format!("It holds work that is not on GitHub in {repos} repos.")
                }),
            )
        }
        Cleaned::NotStarted => ended(Outcome::Failed, "It took too long, so nothing was removed."),
        Cleaned::Failed {
            why,
            repo,
            backed_up,
        } => {
            let name = repo.clone().unwrap_or_else(|| "a repo".to_string());
            let detail = match why {
                Word::NoNetwork if network => repos::NO_NETWORK.to_string(),
                Word::OtherFiles => "It holds files outside its repos, which cannot be backed up, \
                                     so nothing was removed."
                    .to_string(),
                Word::BadBranch => format!(
                    "The backup of {name} could not be made, as its branch has a name that \
                     cannot be used, so nothing was removed."
                ),
                Word::NestedRepo => format!(
                    "{name} holds another git repository that cannot be backed up, so nothing \
                     was removed."
                ),
                Word::NotPushed | Word::NoNetwork => {
                    format!("The backup of {name} could not be pushed, so nothing was removed.")
                }
                Word::NotMade => {
                    format!("The backup of {name} could not be made, so nothing was removed.")
                }
                Word::Changed => {
                    return ended(
                        Outcome::Failed,
                        fit(
                            format!(
                                "{} It changed while it was backed up, so nothing was removed.",
                                backed(backed_up)
                            ),
                            || {
                                "It changed while it was backed up, so nothing was removed."
                                    .to_string()
                            },
                        ),
                    )
                }
                Word::NotRemoved if backed_up.is_empty() => "It could not be removed.".to_string(),
                Word::NotRemoved => {
                    return ended(
                        Outcome::Failed,
                        fit(
                            format!("{} It could not be removed.", backed(backed_up)),
                            || "Backed up, but it could not be removed.".to_string(),
                        ),
                    )
                }
                _ => "A repo in it could not be read, so nothing was removed.".to_string(),
            };
            ended(Outcome::Failed, fit(detail, String::new))
        }
    }
}

/// Run one action in one environment. For one in a container, git runs
/// inside the container through `inside`.
fn run(
    settings: &Settings,
    environment: &Environment,
    action: &Action,
    inside: Option<&Inside>,
) -> Ended {
    // The box's owner turns actions on; until then each is refused unrun.
    if !settings.worktree_actions {
        return ended(Outcome::Refused, "Actions are off on this box.");
    }
    if environment.driver_owns_worktrees() {
        return ended(Outcome::Refused, "The box leaves this folder alone.");
    }
    let nucleus = housekeeping::workspace(settings, environment)
        .leaving_alone(driver::protected_roots(settings))
        .remembering_merges(housekeeping::merges_file(&settings.state_dir));
    let workspace = match nucleus {
        Nucleus::Host(workspace) => workspace,
        Nucleus::Contained(contained) => {
            return match inside {
                Some(inside) => run_contained(settings, environment, &contained, inside, action),
                None => ended(Outcome::Failed, could_not_know(Condition::Absent)),
            }
        }
    };
    let path = environment.root.join(".worktrees").join(&action.folder);
    if !path.exists() {
        return match action.kind {
            Kind::Push => ended(Outcome::Refused, "There is nothing to push."),
            Kind::CleanUp => ended(Outcome::Done, "It was already cleaned up."),
        };
    }
    if Workspace::load_manifest(&path).is_err() {
        return ended(Outcome::Refused, "It is not a session folder.");
    }
    let Ok(worktrees) = worktrees_of(&workspace, &path) else {
        return ended(Outcome::Failed, "A repo in it could not be read.");
    };
    let left_alone = workspace.leaves_alone(&path)
        || worktrees
            .iter()
            .any(|(_, info)| workspace.leaves_alone(&info.path));
    if left_alone {
        return ended(Outcome::Refused, "The box leaves this folder alone.");
    }
    match action.kind {
        Kind::Push => pushed_ended(
            &push_each(&workspace, &worktrees, &mut |_| {}),
            false,
            false,
        ),
        Kind::CleanUp => {
            let store = Store::new(&settings.state_dir);
            let sharing = housekeeping::sharing_nucleus(settings, environment);
            let in_use = || {
                let sessions = housekeeping::agent_sessions(&store, &sharing, &action.folder);
                housekeeping::in_use_because(&sessions, &path).is_some()
            };
            if in_use() {
                return ended(Outcome::Refused, "A session is still using it.");
            }
            let keys = base(&settings.state_dir).join("index");
            let how = Cleaning {
                discard: action.discard.then_some(action.id.as_str()),
                in_use: &in_use,
                keys: &keys,
                plain: false,
                usable: &|_| true,
                committing: &UNHURRIED,
            };
            cleaned_ended(&clean_up_core(&workspace, &path, &how), false)
        }
    }
}

/// One action in an environment whose folders a container writes: the host
/// reads the folder inside the container, asks GitHub about the commits it
/// names and its own store whether a session uses it, and has the push or
/// the removal done inside, then says what came of it in the same words as
/// for a folder on the host.
fn run_contained(
    settings: &Settings,
    environment: &Environment,
    contained: &Contained,
    inside: &Inside,
    action: &Action,
) -> Ended {
    let not_known = |condition| ended(Outcome::Failed, could_not_know(condition));
    let census = match contained.census(inside, settings, Some(&action.folder), false) {
        Ok(census) => census,
        Err(condition) => return not_known(condition),
    };
    let Some(folder) = census.folders.first() else {
        return not_known(Condition::BadReply);
    };
    if folder.gone {
        return match action.kind {
            Kind::Push => ended(Outcome::Refused, "There is nothing to push."),
            Kind::CleanUp => ended(Outcome::Done, "It was already cleaned up."),
        };
    }
    let (false, Some(stamp)) = (folder.unreadable, folder.stamp.clone()) else {
        return ended(Outcome::Failed, "A repo in it could not be read.");
    };
    let merged = contained.merged_among(folder.worktrees.iter().filter_map(|w| w.facts.as_ref()));
    let refused_for = |why: Option<Word>| match why {
        Some(Word::Gone) => match action.kind {
            Kind::Push => Some(ended(Outcome::Refused, "There is nothing to push.")),
            Kind::CleanUp => Some(ended(Outcome::Done, "It was already cleaned up.")),
        },
        Some(Word::NotASession) => Some(ended(Outcome::Refused, "It is not a session folder.")),
        Some(Word::LeftAlone) => Some(ended(Outcome::Refused, "The box leaves this folder alone.")),
        Some(Word::InUse) => Some(ended(Outcome::Refused, "A session is still using it.")),
        Some(Word::Changed) => Some(ended(
            Outcome::Failed,
            "It changed while it was looked at, so nothing was done.",
        )),
        Some(Word::Unreadable) => Some(ended(Outcome::Failed, "A repo in it could not be read.")),
        _ => None,
    };
    match action.kind {
        Kind::Push => {
            let answer = match contained.push(inside, settings, &action.folder, &merged) {
                Ok(answer) => answer,
                Err(condition) => return not_known(condition),
            };
            if let Some(refused) = refused_for(answer.why) {
                return refused;
            }
            let done: Vec<PushOne> = answer
                .pushes
                .iter()
                .map(|one| PushOne {
                    name: one.repo.clone(),
                    pushed: one.pushed,
                    uncommitted: one.uncommitted,
                    failure: one.failure,
                })
                .collect();
            pushed_ended(&done, true, !answer.complete)
        }
        Kind::CleanUp => {
            let store = Store::new(&settings.state_dir);
            let sharing = housekeeping::sharing_nucleus(settings, environment);
            let sessions = housekeeping::agent_sessions(&store, &sharing, &action.folder);
            let path = environment.root.join(".worktrees").join(&action.folder);
            if housekeeping::in_use_because(&sessions, &path).is_some() {
                return ended(Outcome::Refused, "A session is still using it.");
            }
            let discard = action.discard.then_some(action.id.as_str());
            let folders = [(action.folder.clone(), stamp)];
            let answer = match contained.remove(inside, settings, &folders, &merged, discard) {
                Ok(answer) => answer,
                Err(condition) => return not_known(condition),
            };
            let Some(removal) = answer.removals.first() else {
                return cleaned_ended(&Cleaned::NotStarted, true);
            };
            cleaned_ended(&cleaned_of(removal), true)
        }
    }
}

/// What a removal inside a container came to, as a Clean up's outcome.
fn cleaned_of(removal: &repos::answer::Removal) -> Cleaned {
    use repos::answer::Outcome as Came;
    let backed_up: Vec<(String, String)> = removal
        .backed_up
        .iter()
        .map(|backup| (backup.repo.clone(), backup.reference.clone()))
        .collect();
    match removal.outcome {
        Came::Removed | Came::Gone => Cleaned::Removed { backed_up },
        Came::Kept if matches!(removal.why, Some(Word::Unsaved | Word::OtherFiles)) => {
            Cleaned::Holding {
                holding: removal
                    .holding
                    .iter()
                    .map(|held| {
                        let found = Unsaved {
                            uncommitted: held.uncommitted,
                            unpushed: held.unpushed,
                            tracked: false,
                        };
                        (held.repo.clone(), found)
                    })
                    .collect(),
                other_files: removal.other_files,
            }
        }
        Came::Kept | Came::Failed => Cleaned::Failed {
            why: removal.why.unwrap_or(Word::Unreadable),
            repo: removal.repo.clone(),
            backed_up,
        },
    }
}

/// True for an action asked for longer ago than Watch delivers one, by this
/// box's clock, less a little for the two clocks being apart. One whose time
/// cannot be read is judged by when it reached the box alone.
fn too_old(requested_at: &str, now: SystemTime) -> bool {
    usage::parse_time(requested_at)
        .and_then(|at| now.duration_since(at).ok())
        .is_some_and(|age| age > EXPIRES + SKEW)
}

/// Run every action that is waiting, each once, and write down its result.
/// Returns how many ran. While the record of what was started cannot be
/// read, nothing runs: each is refused, and Watch hears why.
pub fn run_queued(settings: &Settings) -> usize {
    run_queued_with(settings, Inside::of_this_box().as_ref())
}

/// As [`run_queued`], with git for a container run through `inside`.
pub fn run_queued_with(settings: &Settings, inside: Option<&Inside>) -> usize {
    let state_dir = &settings.state_dir;
    let Ok(entries) = fs::read_dir(queue_dir(state_dir)) else {
        return 0;
    };
    let mut queued: Vec<(PathBuf, Option<Queued>)> = entries
        .flatten()
        .map(|entry| {
            let read = fs::read_to_string(entry.path())
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok());
            (entry.path(), read)
        })
        .collect();
    queued.sort_by_key(|(_, queued)| queued.as_ref().map(|queued| queued.received_ms));
    let mut ran = 0;
    for (file, queued) in queued {
        let Some(Queued {
            action,
            received_ms,
        }) = queued.filter(|queued| queued.action.valid())
        else {
            let _ = fs::remove_file(&file);
            continue;
        };
        // Each action is judged by the time it starts, not the batch's.
        let now = SystemTime::now();
        let Ok(started) = read_started(state_dir) else {
            // Without it, nothing can say whether this one ran before. It
            // is not run, and Watch hears why.
            eprintln!("nucleated: the record of worktree actions cannot be read; running none");
            let refused = ActionResult {
                id: action.id.clone(),
                outcome: Outcome::Refused,
                detail: "The box cannot read its record of earlier actions.".to_string(),
                finished_at: rfc3339(now),
            };
            if write_result(state_dir, &action.env_id, &refused).is_ok() {
                let _ = fs::remove_file(&file);
            }
            continue;
        };
        if started.contains_key(&action.id) {
            // Started before and never finished: the box stopped while it
            // ran. It is not run again, and Watch hears so.
            let stopped = ActionResult {
                id: action.id.clone(),
                outcome: Outcome::Failed,
                detail: "The box stopped while it ran, and it was not run again.".to_string(),
                finished_at: rfc3339(now),
            };
            if write_result(state_dir, &action.env_id, &stopped).is_ok() {
                let _ = fs::remove_file(&file);
            }
            continue;
        }
        let late = unix_ms(now).saturating_sub(received_ms) > EXPIRES.as_millis() as u64
            || too_old(&action.requested_at, now);
        let Some(environment) = settings.environment(&action.env_id).filter(|_| !late) else {
            let _ = fs::remove_file(&file);
            continue;
        };
        if remember_started(state_dir, &action.id, now).is_err() {
            eprintln!(
                "nucleated: worktree action {} not started: its start could not be written down",
                action.id
            );
            continue;
        }
        let end = run(settings, environment, &action, inside);
        let result = ActionResult {
            id: action.id.clone(),
            outcome: end.outcome,
            detail: end.detail,
            finished_at: rfc3339(SystemTime::now().max(now)),
        };
        if write_result(state_dir, &action.env_id, &result).is_ok() {
            let _ = fs::remove_file(&file);
        }
        eprintln!(
            "nucleated: worktree action {} in {}: {:?}",
            action.id, action.env_id, result.outcome
        );
        ran += 1;
        // What the folder holds has changed: the next report says so.
        reporter::ask_recount(state_dir);
    }
    ran
}

/// The thread that runs Watch's actions: every second, whatever is waiting.
/// The config is read afresh each time.
pub fn run_schedule(config: &Path, stop: &AtomicBool) {
    loop {
        if let Ok(settings) = settings::load_from(config) {
            run_queued(&settings);
        }
        if housekeeping::wait(stop, Duration::from_secs(1)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_action_is_read_leniently_and_kept_only_when_well_formed() {
        let sent = json!([
            { "id": "wta_abcdefghijklmnop", "env_id": "client-a", "folder": "fix-login",
              "kind": "clean_up", "discard": true, "requested_at": "2026-11-19T12:00:00Z",
              "something_new": 1 },
            { "id": "wta_abcdefghijklmnoq", "env_id": "client-a", "folder": "fix-login",
              "kind": "push", "requested_at": "2026-11-19T12:00:00Z" },
            { "id": "wta_ABCDEFGHIJKLMNOP", "env_id": "client-a", "folder": "x", "kind": "push" },
            { "id": "wta_abcdefghijklmnop", "env_id": "client-a", "folder": "../etc", "kind": "push" },
            { "id": "wta_abcdefghijklmnop", "env_id": "client-a", "folder": "x", "kind": "rebase" },
            "not an action",
        ]);
        let actions = actions_from(&sent);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].kind, Kind::CleanUp);
        assert!(actions[0].discard);
        assert_eq!(actions[1].kind, Kind::Push);
        assert!(!actions[1].discard);
        assert!(actions_from(&json!({ "not": "a list" })).is_empty());
        assert!(actions_from(&Value::Null).is_empty());
    }

    #[test]
    fn a_detail_lists_plainly_and_stays_short() {
        assert_eq!(listed(&["a".into()]), "a");
        assert_eq!(listed(&["a".into(), "b".into()]), "a and b");
        assert_eq!(listed(&["a".into(), "b".into(), "c".into()]), "a, b and c");
        assert_eq!(commits(1), "1 commit");
        assert_eq!(commits(3), "3 commits");
        let long = "x".repeat(400);
        assert_eq!(fit(long, || "short".to_string()), "short");
    }

    #[test]
    fn a_failed_push_is_told_in_plain_words_and_never_in_gits() {
        let said = "To https://x-access-token:ghs_secret@github.com/octo/api.git\n \
                    ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs";
        let detail = push_failure(said, "octo/api");
        assert_eq!(
            detail,
            "octo/api was not pushed: the branch on GitHub has commits this one does not."
        );
        let denied = push_failure("remote: Permission to octo/api.git denied", "octo/api");
        assert!(denied.ends_with("did not accept the box's sign-in."));
        let other = push_failure(
            "fatal: '/srv/repos/api' does not appear to be a git repository",
            "api",
        );
        assert_eq!(other, "api was not pushed: git could not push it.");
        for detail in [detail, denied, other] {
            assert!(
                !detail.contains("ghs_") && !detail.contains('/') || detail.starts_with("octo/")
            );
        }
    }

    #[test]
    fn names_that_go_into_a_path_are_checked() {
        assert!(valid_folder("bridge-cse_011degiv5WhcKi3H99uKguPC"));
        assert!(!valid_folder(""));
        assert!(!valid_folder("a/b"));
        assert!(!valid_folder(&"a".repeat(129)));
        assert!(!valid_folder("."));
        assert!(!valid_folder(".."));
        assert!(valid_folder("..."));
        assert!(valid_id("wta_abcdefghijklmn27"));
        assert!(valid_id("wta_0123456789abcdef"));
        assert!(!valid_id("wta_abcdefghijklmn2_"));
        assert!(!valid_id("wta_short"));
        assert!(valid_repo("octo/api"));
        assert!(!valid_repo("octo api"));
    }

    #[test]
    fn a_branch_is_measured_as_watch_measures_it() {
        assert!(reportable_branch("workspace/fix-login"));
        assert!(reportable_branch(""));
        // A character outside the basic plane is two UTF-16 units.
        assert!(!reportable_branch(&"\u{1F600}".repeat(200)));
        assert!(reportable_branch(&"\u{1F600}".repeat(127)));
        assert!(!reportable_branch("has space"));
        assert!(!reportable_branch("tab\there"));
    }

    #[test]
    fn a_backup_ref_fits_in_a_file_name() {
        let id = "wta_abcdefghijk23456";
        assert_eq!(
            backup_ref("workspace/fix-login", id),
            "refs/wip/workspace-fix-login-wta_abcdefghijk23456"
        );
        let long = backup_ref(&format!("workspace/{}", "\u{1F600}".repeat(80)), id);
        let name = long.strip_prefix("refs/wip/").unwrap();
        assert!(name.len() + ".lock".len() <= 255, "{}", name.len());
        assert!(name.ends_with(id));
    }

    #[test]
    fn a_subagents_worktree_of_a_repo_is_folded_into_it() {
        let repo = |name: &str, uncommitted, unpushed| WorktreeRepo {
            repo: name.to_string(),
            branch: "b".to_string(),
            uncommitted,
            unpushed_commits: unpushed,
            merged_pull_request: None,
            open_pull_request: None,
        };
        let mut repos = Vec::new();
        add_repo(&mut repos, repo("octo/web", false, 1));
        add_repo(&mut repos, repo("octo/api", false, 0));
        add_repo(&mut repos, repo("octo/web", true, MAX_UNPUSHED));
        assert_eq!(repos.len(), 2);
        assert!(repos[0].uncommitted);
        assert_eq!(repos[0].unpushed_commits, MAX_UNPUSHED);
    }
}
