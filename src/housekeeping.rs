//! Housekeeping: the jobs that keep a nucleus healthy while nobody watches.
//!
//! - `refresh`: fetch every bare repo and drop branches deleted upstream;
//! - `cleanup`: remove sessions that are finished and hold nothing unsaved,
//!   at once when their work was merged;
//! - `snapshot`: push uncommitted work to a hidden ref, so a dead disk costs
//!   at most an hour.
//!
//! Each calls the same code `nucleate` does. `cleanup` in particular removes
//! a session through `nucleate`'s own all-or-nothing rule and adds conditions
//! of its own on top; it never takes a shortcut around it.

use crate::driver::{self, Condition};
use crate::envelope::{rfc3339, unix_ms};
use crate::git;
use crate::paths;
use crate::repos::answer::Word;
use crate::repos::{self, could_not_know, transfer, Contained, Inside, Nucleus};
use crate::sessions::{Session, Status, Store};
use crate::settings::{self, Environment, Settings};
use crate::workspace::{RepoInfo, Unsaved, Workspace};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// A worktree with a file changed this recently is being written to, and is
/// left for the next snapshot.
const SETTLE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Refresh,
    Cleanup,
    Snapshot,
}

impl Job {
    pub const ALL: [Job; 3] = [Job::Refresh, Job::Cleanup, Job::Snapshot];

    pub fn name(self) -> &'static str {
        match self {
            Job::Refresh => "refresh",
            Job::Cleanup => "cleanup",
            Job::Snapshot => "snapshot",
        }
    }

    pub fn parse(name: &str) -> Option<Job> {
        Job::ALL.into_iter().find(|job| job.name() == name)
    }

    fn interval(self, settings: &Settings) -> Option<Duration> {
        match self {
            Job::Refresh => settings.housekeeping.refresh,
            Job::Cleanup => settings.housekeeping.cleanup,
            Job::Snapshot => settings.housekeeping.snapshot,
        }
    }
}

fn same_folder(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolve(a) == resolve(b)
}

/// The names of the environments whose nucleus is this one's, itself included.
pub(crate) fn sharing_nucleus(settings: &Settings, environment: &Environment) -> Vec<String> {
    settings
        .environments
        .iter()
        .filter(|other| same_folder(&other.root, &environment.root))
        .map(|other| other.name.clone())
        .collect()
}

/// The container git for an environment's repositories runs in, and the
/// environment it runs for there: its own, or, for an environment on the
/// host that shares its nucleus with one in a container, the first such.
/// Nothing when no container can write its nucleus.
pub(crate) fn container_of(
    settings: &Settings,
    environment: &Environment,
) -> Option<(String, String)> {
    if let Some(container) = environment.container() {
        return Some((container.to_string(), environment.name.clone()));
    }
    sharing_nucleus(settings, environment)
        .iter()
        .filter_map(|name| settings.environment(name))
        .find_map(|other| {
            other
                .container()
                .map(|container| (container.to_string(), other.name.clone()))
        })
}

/// One environment per nucleus: where two logins share a nucleus, a job over
/// it runs once, under the first of them. An environment whose driver owns
/// its worktrees has no nucleus, and no job ever runs over it.
pub(crate) fn one_per_nucleus(settings: &Settings) -> Vec<&Environment> {
    let mut chosen: Vec<&Environment> = Vec::new();
    for environment in &settings.environments {
        if environment.driver_owns_worktrees() {
            continue;
        }
        if !chosen
            .iter()
            .any(|seen| same_folder(&seen.root, &environment.root))
        {
            chosen.push(environment);
        }
    }
    chosen
}

/// The repositories of one environment as the daemon may get at them: on
/// the host, git run guarded; or, where a container can write them, only
/// through that container, and never on the host (see [`repos::nucleus`]).
pub(crate) fn workspace(settings: &Settings, environment: &Environment) -> Nucleus {
    repos::nucleus(settings, environment)
}

/// Where GitHub's answers about merged commits are remembered, for cleanup
/// and the reporter alike.
pub(crate) fn merges_file(state_dir: &Path) -> PathBuf {
    state_dir.join("merged.json")
}

/// Run one job for one environment and report what it did.
pub fn run_job(
    settings: &Settings,
    environment: &Environment,
    job: Job,
    dry_run: bool,
) -> Result<Value> {
    run_job_with(
        settings,
        environment,
        job,
        dry_run,
        Inside::of_this_box().as_ref(),
    )
}

/// As [`run_job`], with git for a container run through `inside`.
pub fn run_job_with(
    settings: &Settings,
    environment: &Environment,
    job: Job,
    dry_run: bool,
    inside: Option<&Inside>,
) -> Result<Value> {
    // Asked for by hand or not, no job runs where a driver owns the worktrees.
    if let Some(owner) = environment
        .driver_adapter()
        .filter(|adapter| adapter.owns_worktrees())
    {
        bail!(
            "{} owns the worktrees of {}; housekeeping leaves them alone",
            owner.name(),
            environment.name
        );
    }
    if !environment.root.is_dir() {
        bail!("{} is not a folder", environment.root.display());
    }
    // And no job reaches into a driver's folders from a nucleus beside them.
    let nucleus = workspace(settings, environment)
        .leaving_alone(driver::protected_roots(settings))
        .remembering_merges(merges_file(&settings.state_dir));
    let workspace = match nucleus {
        Nucleus::Host(workspace) => *workspace,
        Nucleus::Contained(contained) => {
            return run_contained(settings, environment, &contained, inside, job, dry_run)
        }
    };
    let mut report = match job {
        Job::Refresh => {
            // A nucleus made before owner folders moves to them first.
            // One that fails says so, and the repos are fetched as they are.
            let moved = match dry_run {
                true => Vec::new(),
                false => match workspace.move_to_owner_folders() {
                    Ok(moved) => moved.lines(),
                    Err(error) => vec![format!("moving repos to owner folders: {error:#}")],
                },
            };
            let mut report = json!({ "repos": workspace.refresh()? });
            if !moved.is_empty() {
                report["moved"] = json!(moved);
            }
            report
        }
        Job::Cleanup => {
            let store = Store::new(&settings.state_dir);
            let sharing = sharing_nucleus(settings, environment);
            let min_age = settings.housekeeping.cleanup_min_age;
            json!({ "sessions": cleanup(&workspace, &sharing, &store, min_age, dry_run)? })
        }
        Job::Snapshot => {
            json!({ "worktrees": snapshot(&workspace, environment, &settings.state_dir, dry_run)? })
        }
    };
    report["job"] = json!(job.name());
    report["environment"] = json!(environment.name);
    report["dry_run"] = json!(dry_run);
    Ok(report)
}

/// What a job for a container says when the container did not finish in
/// its time: what it did not list was not done.
const TOO_LONG: &str = "The environment took too long, so the rest was not done.";
/// What it says when the container could not read, or did not look at,
/// some of the nucleus: what it did not list was not done.
const NOT_ALL: &str = "The environment could not read all of its repos, so the rest was not done.";

/// A write's answer that did not reach everything it was asked to: an
/// error, once what it did do has been recorded, so the run is not good.
fn all_done(answer: &repos::answer::Answer) -> Result<()> {
    if !answer.complete {
        bail!("{TOO_LONG}");
    }
    if answer.unreadable || answer.left_out {
        bail!("{NOT_ALL}");
    }
    Ok(())
}

/// Run one job for an environment whose nucleus a container writes. Git
/// runs only inside the container; what the host decides, it decides from
/// the container's answer, its own store and `/proc`, and what GitHub says.
/// The report names repos, folders and fixed words, never a path or git's
/// own.
fn run_contained(
    settings: &Settings,
    environment: &Environment,
    contained: &Contained,
    inside: Option<&Inside>,
    job: Job,
    dry_run: bool,
) -> Result<Value> {
    let inside = inside.ok_or_else(|| anyhow!("{}", could_not_know(Condition::Absent)))?;
    let not_known = |condition| anyhow!("{}", could_not_know(condition));
    let mut report = match job {
        Job::Refresh => {
            let answer = contained
                .refresh(inside, settings, dry_run, false)
                .map_err(not_known)?;
            let mut report = json!({ "repos": fetched_report(&answer) });
            if answer.why.is_some() {
                report["moved"] = json!("moving repos to owner folders failed");
            } else if answer.moved > 0 {
                report["moved"] = json!(answer.moved);
            }
            all_done(&answer)?;
            report
        }
        Job::Snapshot => {
            let keys = settings.state_dir.join("snapshots").join(&environment.name);
            let answer = contained
                .snapshot(inside, settings, dry_run, &pushed_trees(&keys))
                .map_err(not_known)?;
            let report = json!({ "worktrees": snapshot_report(&answer, &keys)? });
            all_done(&answer)?;
            report
        }
        Job::Cleanup => {
            let census = contained
                .census(inside, settings, None, false)
                .map_err(not_known)?;
            let store = Store::new(&settings.state_dir);
            let sharing = sharing_nucleus(settings, environment);
            let sessions = environment.root.join(".worktrees");
            let min_age = settings.housekeeping.cleanup_min_age;
            let mut report = Vec::new();
            let mut going = Vec::new();
            let mut facts = Vec::new();
            for folder in census.folders.iter().filter(|folder| !folder.gone) {
                let kept =
                    keep_reason_contained(contained, &sharing, &store, &sessions, folder, min_age);
                let mut entry = json!({ "session": folder.folder });
                match (kept, folder.stamp.clone()) {
                    (Some(reason), _) => {
                        entry["action"] = json!("kept");
                        entry["reason"] = json!(reason);
                    }
                    (None, _) if dry_run => entry["action"] = json!("would_remove"),
                    (None, Some(stamp)) => {
                        going.push((folder.folder.clone(), stamp));
                        facts.extend(folder.worktrees.iter().filter_map(|w| w.facts.as_ref()));
                        continue;
                    }
                    (None, None) => {
                        entry["action"] = json!("kept");
                        entry["reason"] = json!("a repo in it could not be read");
                    }
                }
                report.push(entry);
            }
            // The last word is the container's: each folder only if it
            // still stands as it was looked at, all of it or none of it.
            if !going.is_empty() {
                let merged = contained.merged_among(facts);
                let answer = contained
                    .remove(inside, settings, &going, &merged, None)
                    .map_err(not_known)?;
                for (folder, _) in &going {
                    let removal = answer.removals.iter().find(|r| &r.folder == folder);
                    report.push(removal_entry(folder, removal));
                }
            }
            json!({ "sessions": report })
        }
    };
    report["job"] = json!(job.name());
    report["environment"] = json!(environment.name);
    report["dry_run"] = json!(dry_run);
    Ok(report)
}

/// Why a fetch failed, in a few plain words.
fn fetch_failure(word: Word) -> &'static str {
    match word {
        Word::NoNetwork => repos::NO_NETWORK,
        Word::TimedOut => "the fetch took too long",
        Word::SignIn => "GitHub did not accept the box's sign-in",
        Word::Unreachable => "GitHub could not be reached",
        Word::Refused => "GitHub refused it",
        _ => "git could not fetch it",
    }
}

/// A refresh's repos, as the report says them.
fn fetched_report(answer: &repos::answer::Answer) -> Vec<Value> {
    answer
        .fetched
        .iter()
        .map(|fetched| match fetched.refused {
            Some(refused) => json!({
                "repo": fetched.repo, "ok": false,
                "action": "refused", "reason": refused.as_str(),
            }),
            None => {
                let mut entry = json!({ "repo": fetched.repo, "ok": fetched.ok });
                if !fetched.ok {
                    entry["error"] = json!(fetch_failure(fetched.failure.unwrap_or(Word::Git)));
                }
                entry
            }
        })
        .collect()
}

/// The trees the host has recorded as snapshotted, in its keys files.
fn pushed_trees(keys: &Path) -> Vec<String> {
    let mut trees: Vec<String> = fs::read_dir(keys)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| paths::read_plain(&entry.path(), 256).ok())
        .map(|text| text.trim().to_string())
        .filter(|tree| crate::reporter::commit_id(tree))
        .collect();
    trees.sort();
    trees.dedup();
    trees.truncate(repos::answer::MAX_WORKTREES);
    trees
}

/// A snapshot's worktrees, as the report says them, with each tree pushed
/// recorded in its keys file.
fn snapshot_report(answer: &repos::answer::Answer, keys: &Path) -> Result<Vec<Value>> {
    use repos::answer::SnapAction;
    let mut report = Vec::new();
    for snapped in &answer.snapshots {
        let action = match snapped.action {
            SnapAction::Snapshot => "snapshot",
            SnapAction::WouldSnapshot => "would_snapshot",
            SnapAction::Skipped => "skipped",
            SnapAction::Refused => "refused",
            SnapAction::Failed => "failed",
        };
        let mut entry = json!({ "repo": snapped.repo, "action": action });
        if let Some(folder) = &snapped.folder {
            entry["folder"] = json!(folder);
        }
        if !snapped.branch.is_empty() && snapped.action != SnapAction::Skipped {
            entry["branch"] = json!(snapped.branch);
        }
        if let Some(reference) = &snapped.reference {
            entry["ref"] = json!(reference);
        }
        if let Some(word) = snapped.reason {
            entry["reason"] = json!(match (snapped.action, word.refusal()) {
                (SnapAction::Refused, Some(refused)) => refused.as_str(),
                (SnapAction::Failed, _) if word == Word::NoNetwork => repos::NO_NETWORK,
                (SnapAction::Failed, _) => "the push was refused",
                _ => skipped_because(word),
            });
        }
        if let (SnapAction::Snapshot, Some(tree)) = (snapped.action, &snapped.tree) {
            // Named as on the host: the ref's checked name holds nothing
            // but letters, digits and `._-`.
            let name = format!("{}-{}", snapped.repo, dashed(&snapped.branch));
            if name.len() <= 200 {
                paths::create_private_dir(keys)?;
                fs::write(keys.join(name), format!("{tree}\n"))?;
            }
        }
        report.push(entry);
    }
    Ok(report)
}

/// How a removal inside the container came out, for one folder cleanup
/// decided could go, as the report says it.
fn removal_entry(folder: &str, removal: Option<&repos::answer::Removal>) -> Value {
    use repos::answer::Outcome;
    let entry = |action: &str, reason: Option<&str>| {
        let mut entry = json!({ "session": folder, "action": action });
        if let Some(reason) = reason {
            entry["reason"] = json!(reason);
        }
        entry
    };
    let Some(removal) = removal else {
        return entry("failed", Some("it took too long, so it was not removed"));
    };
    let why = match removal.why {
        Some(Word::InUse) => "a process is working in it",
        Some(Word::Changed) => "it changed since it was looked at",
        Some(Word::Unsaved) => "it holds work that is not on GitHub",
        Some(Word::OtherFiles) => "it holds other files",
        Some(Word::LeftAlone) => "the box leaves it alone",
        Some(Word::NotASession) => "it is not a session folder",
        Some(Word::NotRemoved) => "it could not be removed",
        _ => "a repo in it could not be read",
    };
    match removal.outcome {
        Outcome::Removed => entry("removed", None),
        Outcome::Gone => entry("kept", Some("it is gone")),
        Outcome::Kept => entry("kept", Some(why)),
        Outcome::Failed => entry("failed", Some(why)),
    }
}

/// Why a session folder a container writes must be kept, or nothing if it
/// can go: [`keep_reason`]'s rule, from what the container said of it.
fn keep_reason_contained(
    contained: &Contained,
    sharing: &[String],
    store: &Store,
    sessions: &Path,
    folder: &repos::answer::Folder,
    min_age: Duration,
) -> Option<String> {
    let host = &contained.host;
    let agent_sessions = agent_sessions(store, sharing, &folder.folder);
    if let Some(why) = in_use_because(&agent_sessions, &sessions.join(&folder.folder)) {
        return Some(why.to_string());
    }
    if folder.unreadable {
        return Some("a repo in it could not be read".to_string());
    }
    let key = |worktree: &repos::answer::Worktree| worktree.key.clone().unwrap_or_default();
    // A repo the daemon will not run git in cannot be told saved.
    for worktree in &folder.worktrees {
        if let (Some(reason), true) = (worktree.refused, contained.enforcing()) {
            let refused = git::Refused {
                repo: worktree.repo.clone(),
                reason,
            };
            return Some(format!("{}: {refused}", key(worktree)));
        }
    }
    let mut facts = Vec::new();
    for worktree in &folder.worktrees {
        let Some(found) = &worktree.facts else {
            return Some("a repo in it could not be read".to_string());
        };
        facts.push((worktree, found.taken()));
    }
    let merged =
        |found: &crate::workspace::Facts| host.merged_on_github(found.slug.as_deref(), &found.head);
    let mut unsaved = Vec::new();
    for (worktree, found) in &facts {
        let reasons = Unsaved::verdict(found, || merged(found)).reasons();
        if !reasons.is_empty() {
            unsaved.push(format!("{}: {}", key(worktree), reasons.join("; ")));
        }
    }
    if folder.other_files {
        unsaved.push("it holds other files".to_string());
    }
    // As `merged_and_done`: every worktree on the default branch or merged,
    // and one merged at least; with none, nothing left in `.subworktrees`.
    let done = match facts.is_empty() {
        true => !folder.leftovers,
        false => {
            let mut merged_one = false;
            let mut done = true;
            for (_, found) in &facts {
                if !found.on_default && merged(found) {
                    merged_one = true;
                } else if !found.on_default {
                    done = false;
                    break;
                }
            }
            done && merged_one
        }
    };
    if !(unsaved.is_empty() && done) {
        let mut last_change = folder.touched_ms.unwrap_or(0);
        for session in &agent_sessions {
            last_change = last_change.max(session.last_event_ms);
        }
        for (worktree, _) in &facts {
            let committed = worktree.last_commit_s.unwrap_or(u64::MAX / 1000);
            last_change = last_change.max(committed.saturating_mul(1000));
        }
        let age_ms = unix_ms(SystemTime::now()).saturating_sub(last_change);
        if age_ms < min_age.as_millis() as u64 {
            return Some("it changed too recently".to_string());
        }
    }
    unsaved.into_iter().next()
}

// ---- cleanup --------------------------------------------------------------

/// Whether any process has a working directory inside `dir`. `None` where the
/// system cannot say, which callers must read as "it might".
pub(crate) fn in_use(dir: &Path) -> Option<bool> {
    let entries = fs::read_dir("/proc").ok()?;
    let mut saw_a_process = false;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        saw_a_process = true;
        if fs::read_link(entry.path().join("cwd")).is_ok_and(|cwd| cwd.starts_with(dir)) {
            return Some(true);
        }
    }
    saw_a_process.then_some(false)
}

fn modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(unix_ms)
        .unwrap_or(0)
}

/// The agent sessions, of any of the logins sharing a nucleus, that work in
/// the session folder `name`.
pub(crate) fn agent_sessions(store: &Store, sharing: &[String], name: &str) -> Vec<Session> {
    sharing
        .iter()
        .flat_map(|environment| store.list(environment))
        .filter(|session| session.workspace.as_deref() == Some(name))
        .collect()
}

/// Why a session folder is in use, or nothing when it is not: an agent
/// session that has not ended names it, or a process has its working
/// directory inside it. Where the system cannot say, it might be.
pub(crate) fn in_use_because(agent_sessions: &[Session], path: &Path) -> Option<&'static str> {
    // Two logins may share one nucleus, so the folder is live while an agent
    // session of any of them is working in it.
    if agent_sessions
        .iter()
        .any(|session| session.status != Status::Ended)
    {
        return Some("its agent session has not ended");
    }
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    match in_use(&resolved) {
        Some(false) => None,
        Some(true) => Some("a process is working in it"),
        None => Some("this system cannot say whether a process is working in it"),
    }
}

/// Why a session must be kept, or nothing if it can go.
fn keep_reason(
    workspace: &Workspace,
    sharing: &[String],
    store: &Store,
    name: &str,
    path: &Path,
    min_age: Duration,
) -> Result<Option<String>> {
    let agent_sessions = agent_sessions(store, sharing, name);
    if let Some(why) = in_use_because(&agent_sessions, path) {
        return Ok(Some(why.to_string()));
    }
    // Every worktree in it, its subagents' included, and anything else at
    // its root that nucleate did not put there.
    let listed = workspace.session_worktrees(path)?;
    let other = Workspace::other_files(path, &listed);
    let worktrees: Vec<_> = listed
        .into_iter()
        .filter(|(_, info)| info.path.exists())
        .collect();
    // A repo the daemon will not run git in cannot be told saved.
    for (repo, info) in &worktrees {
        if !workspace.git().enforcing() {
            break;
        }
        if let Some(refused) = workspace.git().vet_tree(&info.path) {
            return Ok(Some(format!("{repo}: {refused}")));
        }
    }
    // What it holds that exists nowhere else, asked once.
    let mut unsaved = Vec::new();
    for (repo, info) in &worktrees {
        let reasons = workspace.unsafe_reasons(&info.path)?;
        if !reasons.is_empty() {
            unsaved.push(format!("{repo}: {}", reasons.join("; ")));
        }
    }
    if !other.is_empty() {
        unsaved.push(format!("it holds other files: {}", other.join(", ")));
    }
    if !(unsaved.is_empty() && merged_and_done(workspace, path, &worktrees)?) {
        let mut last_change = modified_ms(path).max(modified_ms(&path.join(".workspace.json")));
        for session in &agent_sessions {
            last_change = last_change.max(session.last_event_ms);
        }
        for (_, info) in &worktrees {
            let committed = workspace
                .git()
                .tree(&info.path, &["log", "-1", "--format=%ct"])
                .ok()
                .and_then(|seconds| seconds.parse::<u64>().ok())
                .unwrap_or(u64::MAX / 1000);
            last_change = last_change.max(committed.saturating_mul(1000));
        }
        let age_ms = unix_ms(SystemTime::now()).saturating_sub(last_change);
        if age_ms < min_age.as_millis() as u64 {
            return Ok(Some("it changed too recently".to_string()));
        }
    }
    Ok(unsaved.into_iter().next())
}

/// True when a folder that holds nothing unsaved, nor any file nucleate did
/// not put there, is finished with, so that it need not wait out the age
/// limit: each worktree in it holds nothing beyond the remote's default
/// branch or has its `HEAD` in a merged pull request, and at least one was
/// merged. A folder with no worktree and an empty `.subworktrees` is
/// finished with too. Work that was only pushed is not finished: a branch
/// on GitHub may still be worked on, and waits as before.
fn merged_and_done(
    workspace: &Workspace,
    path: &Path,
    worktrees: &[(String, RepoInfo)],
) -> Result<bool> {
    if worktrees.is_empty() {
        let subs = path.join(".subworktrees");
        return Ok(fs::symlink_metadata(&subs).is_err()
            || fs::read_dir(&subs).is_ok_and(|mut inside| inside.next().is_none()));
    }
    let mut merged = false;
    for (_, info) in worktrees {
        if workspace.head_merged(&info.path)? {
            merged = true;
        } else if !workspace.on_default_branch(&info.path)? {
            return Ok(false);
        }
    }
    Ok(merged)
}

fn cleanup(
    workspace: &Workspace,
    sharing: &[String],
    store: &Store,
    min_age: Duration,
    dry_run: bool,
) -> Result<Vec<Value>> {
    let mut report = Vec::new();
    for (name, path) in workspace.sessions() {
        let entry = |action: &str, reason: Option<String>| {
            let mut entry = json!({ "session": name, "action": action });
            if let Some(reason) = reason {
                entry["reason"] = json!(reason);
            }
            entry
        };
        let verdict = keep_reason(workspace, sharing, store, &name, &path, min_age);
        report.push(match verdict {
            Ok(Some(reason)) => entry("kept", Some(reason)),
            Err(error) => entry("kept", Some(format!("{error:#}"))),
            Ok(None) if dry_run => entry("would_remove", None),
            // The last word is nucleate's: all of the session, or none of it.
            Ok(None) => match workspace.remove_session(&path) {
                Ok(_) => entry("removed", None),
                Err(error) => entry("kept", Some(format!("{error:#}"))),
            },
        });
    }
    if !dry_run {
        workspace.prune_worktrees()?;
    }
    Ok(report)
}

// ---- snapshot -------------------------------------------------------------

/// The paths `git status` names as changed or untracked.
fn changed_paths(porcelain_z: &[u8]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut fields = porcelain_z
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        // A rename or copy is followed by the path it came from.
        if matches!(field[0], b'R' | b'C') || matches!(field[1], b'R' | b'C') {
            fields.next();
        }
        paths.push(PathBuf::from(
            String::from_utf8_lossy(&field[3..]).to_string(),
        ));
    }
    paths
}

/// True if `path`, or anything under it, was modified within `SETTLE`.
fn recently_written(path: &Path, now: SystemTime, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    let fresh = meta
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age < SETTLE);
    if fresh && !meta.is_dir() {
        return true;
    }
    meta.is_dir()
        && fs::read_dir(path).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| recently_written(&entry.path(), now, budget))
        })
}

fn dashed(branch: &str) -> String {
    branch.replace('/', "-")
}

/// What a snapshot came to for one worktree.
pub(crate) enum Snapped {
    /// Nothing was pushed, for this reason.
    Skipped(Word),
    /// It would have pushed `tree` to `reference`.
    Would {
        branch: String,
        reference: String,
        tree: String,
    },
    /// It pushed `tree` to `reference`.
    Done {
        branch: String,
        reference: String,
        tree: String,
    },
    /// The push failed, and git said this, which goes no further than a
    /// word made from it.
    Failed { branch: String, stderr: String },
}

/// How a snapshot is taken: where its copy of the index goes, which trees
/// are pushed already, and which refs may be pushed to.
pub(crate) struct Snapping<'a> {
    /// A private folder for the copy of a worktree's index.
    pub index_dir: &'a Path,
    /// Copy the index only when it is a plain file, never through a link.
    pub plain_index: bool,
    /// Whether a tree was pushed already.
    pub already: &'a dyn Fn(&str) -> bool,
    /// Whether a ref may be pushed to.
    pub usable: &'a dyn Fn(&str) -> bool,
    pub dry_run: bool,
    pub now: SystemTime,
}

/// Snapshot one worktree: push what it has not committed, as a commit on
/// no branch, to its hidden ref, unless that is nothing, it is being
/// written to, or it is pushed already.
pub(crate) fn snapshot_core(git: &git::Git, worktree: &Path, how: &Snapping) -> Result<Snapped> {
    let branch = git.tree(worktree, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Ok(Snapped::Skipped(Word::NotOnABranch));
    }
    let (ok, status, error) = git.tree_raw(worktree, &["status", "--porcelain", "-z"], &[])?;
    if !ok {
        bail!("git status failed: {error}");
    }
    let changed = changed_paths(&status);
    if changed.is_empty() {
        return Ok(Snapped::Skipped(Word::NothingUncommitted));
    }
    let mut budget = 20_000;
    if changed
        .iter()
        .any(|path| recently_written(&worktree.join(path), how.now, &mut budget))
    {
        return Ok(Snapped::Skipped(Word::BeingWrittenTo));
    }
    let reference = wip_ref(&branch);
    if !(how.usable)(&reference) {
        return Ok(Snapped::Skipped(Word::BadBranch));
    }
    let tree = working_tree(git, worktree, how.index_dir, &branch, how.plain_index)?;
    if tree == git.tree(worktree, &["rev-parse", "HEAD^{tree}"])? {
        return Ok(Snapped::Skipped(Word::NothingUncommitted));
    }
    if (how.already)(&tree) {
        return Ok(Snapped::Skipped(Word::AlreadySnapshotted));
    }
    if how.dry_run {
        return Ok(Snapped::Would {
            branch,
            reference,
            tree,
        });
    }
    let commit = wip_commit(git, worktree, &tree, &branch, how.now)?;
    let refspec = format!("{commit}:{reference}");
    let pushed = transfer::push(git, worktree, &["-q", "--force", "origin", &refspec], None)?;
    if !pushed.ok {
        return Ok(Snapped::Failed {
            branch,
            stderr: pushed.stderr,
        });
    }
    Ok(Snapped::Done {
        branch,
        reference,
        tree,
    })
}

/// The words a snapshot's report has used for why it skipped a worktree.
pub(crate) fn skipped_because(word: Word) -> &'static str {
    match word {
        Word::NotOnABranch => "not on a branch",
        Word::NothingUncommitted => "nothing uncommitted",
        Word::BeingWrittenTo => "being written to",
        Word::AlreadySnapshotted => "already snapshotted",
        Word::OwnedByADriver => "owned by a driver",
        Word::NotOurs => "not under this nucleus's .worktrees",
        Word::BadBranch => "its branch has a name that cannot be used",
        _ => "it could not be read",
    }
}

fn snapshot_one(
    git: &git::Git,
    worktree: &Path,
    repo: &str,
    keys: &Path,
    dry_run: bool,
    now: SystemTime,
) -> Result<Value> {
    let entry = |action: &str, detail: Value| {
        let mut entry = json!({ "repo": repo, "worktree": worktree, "action": action });
        if let (Some(entry), Some(detail)) = (entry.as_object_mut(), detail.as_object()) {
            entry.extend(detail.clone());
        }
        entry
    };
    paths::create_private_dir(keys)?;
    let key = |branch: &str| keys.join(format!("{repo}-{}", dashed(branch)));
    let branch_now = git.tree(worktree, &["branch", "--show-current"])?;
    let already =
        |tree: &str| fs::read_to_string(key(&branch_now)).is_ok_and(|pushed| pushed.trim() == tree);
    let how = Snapping {
        index_dir: keys,
        plain_index: false,
        already: &already,
        usable: &|_| true,
        dry_run,
        now,
    };
    Ok(match snapshot_core(git, worktree, &how)? {
        Snapped::Skipped(word) => entry("skipped", json!({ "reason": skipped_because(word) })),
        Snapped::Would {
            branch, reference, ..
        } => entry(
            "would_snapshot",
            json!({ "branch": branch, "ref": reference }),
        ),
        Snapped::Failed { branch, .. } => entry(
            "failed",
            json!({ "branch": branch, "reason": "the push was refused" }),
        ),
        Snapped::Done {
            branch,
            reference,
            tree,
        } => {
            fs::write(key(&branch), format!("{tree}\n"))?;
            entry("snapshot", json!({ "branch": branch, "ref": reference }))
        }
    })
}

/// Where uncommitted work on `branch` is pushed: a ref that is not a branch.
pub(crate) fn wip_ref(branch: &str) -> String {
    format!("refs/wip/{}", dashed(branch))
}

/// The tree of a worktree as it stands, its uncommitted and untracked files
/// included and its ignored ones not. It is built in a copy of the index, in
/// `keys`, so neither the session's index nor its branch is touched.
///
/// With `plain`, the index is copied only when it is a plain file, read
/// never through a link: inside a container, whose worktree's git folder
/// the container writes.
pub(crate) fn working_tree(
    git: &git::Git,
    worktree: &Path,
    keys: &Path,
    branch: &str,
    plain: bool,
) -> Result<String> {
    let git_dir = PathBuf::from(git.tree(worktree, &["rev-parse", "--absolute-git-dir"])?);
    paths::create_private_dir(keys)?;
    let index = keys.join(format!(".index.{}.{}", std::process::id(), dashed(branch)));
    if plain {
        copy_plain(&git_dir.join("index"), &index)?;
    } else if git_dir.join("index").is_file() {
        fs::copy(git_dir.join("index"), &index)?;
    }
    let with_index: [(&str, &OsStr); 1] = [("GIT_INDEX_FILE", index.as_os_str())];
    let built = (|| -> Result<String> {
        let (ok, _, error) = git.tree_raw(worktree, &["add", "-A"], &with_index)?;
        if !ok {
            bail!("git add failed: {error}");
        }
        let (ok, tree, error) = git.tree_raw(worktree, &["write-tree"], &with_index)?;
        if !ok {
            bail!("git write-tree failed: {error}");
        }
        Ok(String::from_utf8_lossy(&tree).trim().to_string())
    })();
    let _ = fs::remove_file(&index);
    built
}

/// Copy `from`, when it is a plain file, to a new file `to`, reading it
/// never through a link and never a pipe or a device. Nothing is copied
/// when there is no such file.
fn copy_plain(from: &Path, to: &Path) -> Result<()> {
    let Ok(looked) = paths::look_regular(from) else {
        return Ok(());
    };
    let mut source = paths::open_looked(from, &looked)?;
    let mut copy = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    std::io::copy(&mut source, &mut copy)?;
    Ok(())
}

/// A commit of `tree` on top of `HEAD`, made without moving the branch.
pub(crate) fn wip_commit(
    git: &git::Git,
    worktree: &Path,
    tree: &str,
    branch: &str,
    now: SystemTime,
) -> Result<String> {
    // commit-tree needs an author; a box with none configured still snapshots.
    let unnamed = git
        .try_tree(worktree, &["config", "user.email"])
        .map_or(true, |o| o.stdout.is_empty());
    let identity: Vec<(&str, &OsStr)> = if unnamed {
        ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"]
            .into_iter()
            .map(|key| (key, OsStr::new("nucleated")))
            .chain(
                ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"]
                    .into_iter()
                    .map(|key| (key, OsStr::new("nucleated@localhost"))),
            )
            .collect()
    } else {
        Vec::new()
    };
    let message = format!("wip snapshot {} {branch}", rfc3339(now));
    let (ok, commit, error) = git.tree_raw(
        worktree,
        &["commit-tree", tree, "-p", "HEAD", "-m", &message],
        &identity,
    )?;
    if !ok {
        bail!("git commit-tree failed: {error}");
    }
    Ok(String::from_utf8_lossy(&commit).trim().to_string())
}

fn snapshot(
    workspace: &Workspace,
    environment: &Environment,
    state_dir: &Path,
    dry_run: bool,
) -> Result<Vec<Value>> {
    let keys = state_dir.join("snapshots").join(&environment.name);
    let now = SystemTime::now();
    let mut report = Vec::new();
    let refused = |repo: &str, worktree: Option<&Path>, refused: &git::Refused| {
        json!({
            "repo": repo, "worktree": worktree,
            "action": "refused", "reason": refused.reason.as_str(),
        })
    };
    for item in workspace.available()? {
        let worktrees = match workspace.worktrees(&item.bare) {
            Ok(worktrees) => worktrees,
            Err(error) => {
                if let Some(why) = git::refusal(&error) {
                    report.push(refused(&item.repo, None, why));
                }
                Vec::new()
            }
        };
        for worktree in worktrees {
            if !worktree.is_dir() {
                continue;
            }
            // A snapshot writes objects into the repository and pushes a
            // ref. In a worktree that is not the nucleus's own, a driver's
            // or one made by hand, that is a change, so it is not made.
            if let Some(why) = workspace.left_alone_because(&worktree) {
                report.push(json!({
                    "repo": item.repo, "worktree": worktree,
                    "action": "skipped", "reason": why,
                }));
                continue;
            }
            report.push(
                snapshot_one(workspace.git(), &worktree, &item.repo, &keys, dry_run, now)
                    .unwrap_or_else(|error| match git::refusal(&error) {
                        Some(why) => refused(&item.repo, Some(&worktree), why),
                        None => json!({
                            "repo": item.repo, "worktree": worktree,
                            "action": "failed", "reason": format!("{error:#}"),
                        }),
                    }),
            );
        }
    }
    Ok(report)
}

// ---- the schedule ---------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    pub at: String,
    pub at_s: u64,
    pub ok: bool,
    /// A count of what the job did, or the error that stopped it.
    pub summary: String,
    /// When the job last finished with nothing failed: no error, and no repo
    /// or worktree it could not deal with. Carried over a run that was not.
    #[serde(default)]
    pub good_at: Option<String>,
    #[serde(default)]
    pub good_at_s: Option<u64>,
}

/// When each job last ran for each environment, keyed `<environment>/<job>`.
pub type Ledger = BTreeMap<String, LastRun>;

pub fn ledger_file(state_dir: &Path) -> PathBuf {
    state_dir.join("housekeeping.json")
}

pub fn read_ledger(state_dir: &Path) -> Ledger {
    fs::read_to_string(ledger_file(state_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn summarize(report: &Value) -> String {
    let none = Vec::new();
    let (list, key) = ["repos", "sessions", "worktrees"]
        .iter()
        .find_map(|name| report[name].as_array().map(|list| (list, *name)))
        .unwrap_or((&none, "items"));
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for item in list {
        let action = match item["action"].as_str() {
            Some(action) => action.to_string(),
            None if item["ok"] == true => "fetched".to_string(),
            None => "failed".to_string(),
        };
        *counts.entry(action).or_default() += 1;
    }
    if counts.is_empty() {
        return format!("no {key}");
    }
    counts
        .iter()
        .map(|(action, count)| format!("{count} {action}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// True when a job's report holds nothing it failed at.
fn nothing_failed(report: &Value) -> bool {
    ["repos", "sessions", "worktrees"]
        .iter()
        .filter_map(|name| report[name].as_array())
        .flatten()
        .all(|item| match item["action"].as_str() {
            // A repo git was not run in was not dealt with.
            Some(action) => action != "failed" && action != "refused",
            None => item["ok"] == true,
        })
}

/// Run every job that is due, once.
pub fn run_due(settings: &Settings, now: SystemTime) -> Result<()> {
    let now_s = unix_ms(now) / 1000;
    let mut ledger = read_ledger(&settings.state_dir);
    for environment in one_per_nucleus(settings) {
        for job in Job::ALL {
            let Some(interval) = job.interval(settings) else {
                continue;
            };
            let key = format!("{}/{}", environment.name, job.name());
            let due = ledger
                .get(&key)
                .is_none_or(|last| now_s.saturating_sub(last.at_s) >= interval.as_secs());
            if !due {
                continue;
            }
            let outcome = run_job(settings, environment, job, false);
            let (ok, summary) = match &outcome {
                Ok(report) => (true, summarize(report)),
                Err(error) => (false, format!("{error:#}")),
            };
            let good = outcome.as_ref().is_ok_and(nothing_failed);
            let before = ledger.get(&key);
            let (good_at, good_at_s) = match good {
                true => (Some(rfc3339(now)), Some(now_s)),
                false => (
                    before.and_then(|last| last.good_at.clone()),
                    before.and_then(|last| last.good_at_s),
                ),
            };
            eprintln!(
                "nucleated: {} for {}: {summary}",
                job.name(),
                environment.name
            );
            ledger.insert(
                key,
                LastRun {
                    at: rfc3339(now),
                    at_s: now_s,
                    ok,
                    summary,
                    good_at,
                    good_at_s,
                },
            );
            // Written after every job, so a stop in the middle loses nothing.
            let text = serde_json::to_string_pretty(&ledger)?;
            paths::write_private(&ledger_file(&settings.state_dir), text.as_bytes())?;
        }
    }
    Ok(())
}

pub(crate) fn wait(stop: &AtomicBool, duration: Duration) -> bool {
    let mut left = duration;
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let step = left.min(Duration::from_millis(250));
        std::thread::sleep(step);
        left -= step;
    }
    stop.load(Ordering::SeqCst)
}

/// The housekeeping thread: a minute after start, then every half minute,
/// run whatever is due. The config is read afresh each time.
pub fn run_schedule(config: &Path, stop: &AtomicBool) {
    if wait(stop, Duration::from_secs(60)) {
        return;
    }
    loop {
        if let Ok(settings) = settings::load_from(config) {
            if let Err(error) = run_due(&settings, SystemTime::now()) {
                eprintln!("nucleated: housekeeping: {error:#}");
            }
        }
        if wait(stop, Duration::from_secs(30)) {
            return;
        }
    }
}

/// `nucleated housekeeping run <job>`: run one job now, for one environment
/// or for all of them.
pub fn run_now(
    settings: &Settings,
    job: &str,
    env: Option<&str>,
    dry_run: bool,
) -> Result<(Value, bool)> {
    let job = Job::parse(job)
        .ok_or_else(|| anyhow!("no such job: {job}; the jobs are refresh, cleanup and snapshot"))?;
    let environments: Vec<&Environment> = match env {
        Some(name) => vec![settings.environment(name).ok_or_else(|| {
            anyhow!("no environment named {name} in {}", settings.path.display())
        })?],
        None => one_per_nucleus(settings),
    };
    let mut ok = true;
    let reports: Vec<Value> = environments
        .into_iter()
        .map(|environment| {
            run_job(settings, environment, job, dry_run).unwrap_or_else(|error| {
                ok = false;
                json!({ "job": job.name(), "environment": environment.name, "error": format!("{error:#}") })
            })
        })
        .collect();
    Ok((Value::Array(reports), ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_paths_are_read_from_porcelain_output() {
        let status = b" M src/a.rs\0?? notes/\0R  new name.txt\0old name.txt\0A  b.rs\0";
        assert_eq!(
            changed_paths(status),
            ["src/a.rs", "notes/", "new name.txt", "b.rs"].map(PathBuf::from)
        );
        assert!(changed_paths(b"").is_empty());
    }

    #[test]
    fn branch_names_become_ref_names() {
        assert_eq!(dashed("workspace/fix-login"), "workspace-fix-login");
    }

    #[test]
    fn a_report_is_summed_up_by_what_was_done() {
        let report = json!({ "sessions": [
            { "action": "kept" }, { "action": "removed" }, { "action": "kept" },
        ] });
        assert_eq!(summarize(&report), "2 kept, 1 removed");
        assert_eq!(
            summarize(&json!({ "repos": [{ "ok": true }, { "ok": false }] })),
            "1 failed, 1 fetched"
        );
        assert_eq!(summarize(&json!({ "worktrees": [] })), "no worktrees");
    }

    #[test]
    fn a_run_is_good_when_nothing_in_it_failed() {
        assert!(nothing_failed(&json!({ "worktrees": [] })));
        assert!(nothing_failed(&json!({ "worktrees": [
            { "action": "snapshot" }, { "action": "skipped" },
        ] })));
        assert!(!nothing_failed(&json!({ "worktrees": [
            { "action": "snapshot" }, { "action": "failed" },
        ] })));
        assert!(nothing_failed(&json!({ "repos": [{ "ok": true }] })));
        assert!(!nothing_failed(
            &json!({ "repos": [{ "ok": true }, { "ok": false }] })
        ));
    }
}
