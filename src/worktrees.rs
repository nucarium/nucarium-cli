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

use crate::envelope::{rfc3339, unix_ms, valid_env_key};
use crate::housekeeping;
use crate::reporter;
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
fn valid_repo(value: &str) -> bool {
    (1..=140).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}

/// A branch as Watch takes one: at most 255 UTF-16 units, with no space or
/// control character in it.
fn reportable_branch(value: &str) -> bool {
    value.encode_utf16().count() <= MAX_BRANCH
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
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
fn worktrees_of(workspace: &Workspace, path: &Path) -> anyhow::Result<Vec<(String, RepoInfo)>> {
    Ok(workspace
        .session_worktrees(path)?
        .into_iter()
        .filter(|(_, info)| info.path.is_dir())
        .collect())
}

/// The newest modification time, in Unix milliseconds, of any file that git
/// tracks or would track in these worktrees. Nothing when git cannot list
/// them, or when there are too many to look at.
fn last_change(git: &git::Git, worktrees: &[&Path]) -> Option<u64> {
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
    let worktrees = worktrees_of(workspace, path).ok()?;
    let sessions = housekeeping::agent_sessions(store, sharing, name);
    let in_use = housekeeping::in_use_because(&sessions, path).is_some();
    let git = workspace.git();
    let mut repos = Vec::new();
    for (key, info) in &worktrees {
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
fn valid_id(value: &str) -> bool {
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
    let Ok(started) = read_started(&settings.state_dir) else {
        eprintln!("nucleated: the record of worktree actions cannot be read; taking none");
        return;
    };
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
/// [`MAX_RESULTS`]. Nothing when there are none.
pub fn results(state_dir: &Path, env: &str) -> Option<Vec<ActionResult>> {
    if !valid_env_key(env) {
        return None;
    }
    let mut found: Vec<ActionResult> = fs::read_dir(results_dir(state_dir, env))
        .ok()?
        .flatten()
        .filter_map(|entry| serde_json::from_str(&fs::read_to_string(entry.path()).ok()?).ok())
        .collect();
    found.retain(|result| valid_id(&result.id));
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
/// with no control character in it.
fn fit(text: String, shorter: impl FnOnce() -> String) -> String {
    let line = |text: String| -> String {
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_DETAIL)
            .collect()
    };
    if text.chars().count() <= MAX_DETAIL {
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
fn push_failure(stderr: &str, repo: &str) -> String {
    let said = stderr.to_lowercase();
    let why = if said == "timed out" {
        "it took too long"
    } else if ["non-fast-forward", "[rejected]", "fetch first"]
        .iter()
        .any(|word| said.contains(word))
    {
        "the branch on GitHub has commits this one does not"
    } else if [
        "authentication failed",
        "could not read username",
        "permission",
        "denied",
        "403",
    ]
    .iter()
    .any(|word| said.contains(word))
    {
        "GitHub did not accept the box's sign-in"
    } else if [
        "could not resolve host",
        "unable to access",
        "connection",
        "timed out",
    ]
    .iter()
    .any(|word| said.contains(word))
    {
        "GitHub could not be reached"
    } else if ["protected branch", "declined"]
        .iter()
        .any(|word| said.contains(word))
    {
        "GitHub refused it"
    } else {
        "git could not push it"
    };
    format!("{repo} was not pushed: {why}.")
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

fn push(workspace: &Workspace, worktrees: &[(String, RepoInfo)]) -> Ended {
    let git = workspace.git();
    let mut pushed: Vec<(String, u32)> = Vec::new();
    let mut uncommitted: Vec<String> = Vec::new();
    let mut failures = Vec::new();
    for (key, info) in worktrees {
        let name = repo_name(key, info).unwrap_or_else(|| "a repo".to_string());
        let Ok(unsaved) = workspace.unsaved(&info.path) else {
            failures.push(format!("{name} could not be read."));
            continue;
        };
        if unsaved.uncommitted && !uncommitted.contains(&name) {
            uncommitted.push(name.clone());
        }
        if unsaved.unpushed == 0 {
            continue;
        }
        // A detached HEAD is on no branch to push.
        let branch = git
            .tree(&info.path, &["branch", "--show-current"])
            .unwrap_or_default();
        if branch.is_empty() {
            continue;
        }
        if !pushable_branch(git, &info.path, &branch) {
            failures.push(format!(
                "{name} was not pushed: its branch has a name that cannot be pushed safely."
            ));
            continue;
        }
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let args = ["push", "-q", "-u", "origin", "--", refspec.as_str()];
        match git.try_tree_within(&info.path, &args, PUSH_TIMEOUT) {
            Ok(out) if out.ok => match pushed.iter_mut().find(|(have, _)| *have == name) {
                Some((_, count)) => *count = count.saturating_add(unsaved.unpushed),
                None => pushed.push((name, unsaved.unpushed)),
            },
            Ok(out) => failures.push(push_failure(&out.stderr, &name)),
            Err(_) => failures.push(format!("{name} was not pushed: git cannot be run there.")),
        }
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

fn standing(git: &git::Git, info: &RepoInfo, keys: &Path) -> anyhow::Result<Standing> {
    Ok(Standing {
        head: git.tree(&info.path, &["rev-parse", "HEAD"])?,
        tree: housekeeping::working_tree(git, &info.path, keys, "backup")?,
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

/// The ref a backup went to, or why it could not be made.
enum Backup {
    Pushed(String),
    Failed(String),
}

/// Push a backup of one worktree's unsaved work to a ref of this action's
/// own, `refs/wip/<branch>-<action id>`, that nothing else writes: its
/// commits and, as a commit of their own made on no branch, its uncommitted
/// changes. Never over anything already there.
fn back_up(
    git: &git::Git,
    name: &str,
    info: &RepoInfo,
    seen: &Standing,
    action: &Action,
) -> anyhow::Result<Backup> {
    let current = git.tree(&info.path, &["branch", "--show-current"])?;
    let branch = if current.is_empty() {
        info.branch.clone()
    } else {
        current
    };
    if !pushable_branch(git, &info.path, &branch) {
        return Ok(Backup::Failed(format!(
            "The backup of {name} could not be made, as its branch has a name that cannot be \
             used, so nothing was removed."
        )));
    }
    if new_gitlinks(git, &info.path, &seen.tree)? {
        return Ok(Backup::Failed(format!(
            "{name} holds another git repository that cannot be backed up, so nothing was \
             removed."
        )));
    }
    let head_tree = git.tree(&info.path, &["rev-parse", "HEAD^{tree}"])?;
    let commit = if seen.tree == head_tree {
        seen.head.clone()
    } else {
        housekeeping::wip_commit(git, &info.path, &seen.tree, &branch, SystemTime::now())?
    };
    let reference = format!("{}-{}", housekeeping::wip_ref(&branch), action.id);
    let refspec = format!("{commit}:{reference}");
    let pushed = git.try_tree_within(
        &info.path,
        &["push", "-q", "origin", "--", refspec.as_str()],
        PUSH_TIMEOUT,
    )?;
    Ok(match pushed.ok {
        true => Backup::Pushed(reference),
        false => Backup::Failed(format!(
            "The backup of {name} could not be pushed, so nothing was removed."
        )),
    })
}

fn clean_up(
    workspace: &Workspace,
    path: &Path,
    action: &Action,
    in_use: &dyn Fn() -> bool,
    keys: &Path,
) -> Ended {
    let git = workspace.git();
    let unreadable = || {
        ended(
            Outcome::Failed,
            "A repo in it could not be read, so nothing was removed.",
        )
    };
    let Ok(worktrees) = worktrees_of(workspace, path) else {
        return unreadable();
    };
    let mut holding = Vec::new();
    for (key, info) in &worktrees {
        let name = repo_name(key, info).unwrap_or_else(|| "a repo".to_string());
        match workspace.unsaved(&info.path) {
            Ok(found) if found.is_saved() => {}
            Ok(found) => holding.push((name, info, found)),
            Err(_) => return unreadable(),
        }
    }
    if holding.is_empty() {
        // Removed only if, looked at again, it still holds nothing unsaved.
        return match workspace.remove_session(path) {
            Ok(_) => ended(Outcome::Done, "Cleaned up."),
            Err(_) => ended(Outcome::Failed, "It could not be removed."),
        };
    }
    if !action.discard {
        let words: Vec<String> = holding
            .iter()
            .flat_map(|(name, _, found)| unsaved_words(found, name))
            .collect();
        let repos = holding.len();
        return ended(
            Outcome::Refused,
            fit(format!("It holds {}.", listed(&words)), || {
                format!("It holds work that is not on GitHub in {repos} repos.")
            }),
        );
    }
    // How each worktree stands before anything is pushed: what is backed up,
    // and what it is compared with just before anything is removed.
    let mut before = Vec::new();
    for (_, info) in &worktrees {
        match standing(git, info, keys) {
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
        match back_up(git, name, info, &before[at], action) {
            Ok(Backup::Pushed(reference)) => backed_up.push(format!("{reference} in {name}")),
            Ok(Backup::Failed(detail)) => return ended(Outcome::Failed, fit(detail, String::new)),
            Err(_) => {
                return ended(
                    Outcome::Failed,
                    format!("The backup of {name} could not be made, so nothing was removed."),
                )
            }
        }
    }
    let repos = backed_up.len();
    let backed = fit(format!("Backed up to {}.", listed(&backed_up)), || {
        format!("Backed up {repos} repos to GitHub.")
    });
    // Looked at once more just before the removal. A session that came
    // back to it, a repo added or gone, or a commit or a change made since
    // the backup, and nothing is removed.
    let changed = in_use()
        || worktrees_of(workspace, path).map_or(true, |now| {
            now.len() != worktrees.len()
                || now.iter().zip(&worktrees).zip(&before).any(
                    |(((_, now_info), (_, then_info)), seen)| {
                        now_info.path != then_info.path
                            || standing(git, now_info, keys).map_or(true, |now| &now != seen)
                    },
                )
        });
    if changed {
        return ended(
            Outcome::Failed,
            fit(
                format!("{backed} It changed while it was backed up, so nothing was removed."),
                || "It changed while it was backed up, so nothing was removed.".to_string(),
            ),
        );
    }
    match workspace.remove_session_with(path, true) {
        Ok(_) => ended(Outcome::Done, backed),
        Err(_) => ended(
            Outcome::Failed,
            fit(format!("{backed} It could not be removed."), || {
                "Backed up, but it could not be removed.".to_string()
            }),
        ),
    }
}

/// Run one action in one environment.
fn run(settings: &Settings, environment: &Environment, action: &Action) -> Ended {
    // The box's owner turns actions on; until then each is refused unrun.
    if !settings.worktree_actions {
        return ended(Outcome::Refused, "Actions are off on this box.");
    }
    if environment.driver_owns_worktrees() {
        return ended(Outcome::Refused, "The box leaves this folder alone.");
    }
    let workspace = housekeeping::workspace(settings, environment)
        .leaving_alone(driver::protected_roots(settings))
        .remembering_merges(housekeeping::merges_file(&settings.state_dir));
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
        Kind::Push => push(&workspace, &worktrees),
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
            clean_up(&workspace, &path, action, &in_use, &keys)
        }
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
/// Returns how many ran. Nothing runs while the record of what was started
/// cannot be read.
pub fn run_queued(settings: &Settings) -> usize {
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
            eprintln!("nucleated: the record of worktree actions cannot be read; running none");
            return ran;
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
        let end = run(settings, environment, &action);
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
