//! Housekeeping: the jobs that keep a nucleus healthy while nobody watches.
//!
//! - `refresh`: fetch every bare repo and drop branches deleted upstream;
//! - `cleanup`: remove sessions that are finished and hold nothing unsaved;
//! - `snapshot`: push uncommitted work to a hidden ref, so a dead disk costs
//!   at most an hour.
//!
//! Each calls the same code `nucleate` does. `cleanup` in particular removes
//! a session through `nucleate`'s own all-or-nothing rule and adds conditions
//! of its own on top; it never takes a shortcut around it.

use crate::config;
use crate::envelope::{rfc3339, unix_ms};
use crate::git;
use crate::paths;
use crate::sessions::{Status, Store};
use crate::settings::{self, Environment, Settings};
use crate::workspace::Workspace;
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

/// One environment per nucleus: where two logins share a nucleus, a job over
/// it runs once, under the first of them.
pub(crate) fn one_per_nucleus(settings: &Settings) -> Vec<&Environment> {
    let mut chosen: Vec<&Environment> = Vec::new();
    for environment in &settings.environments {
        if !chosen
            .iter()
            .any(|seen| same_folder(&seen.root, &environment.root))
        {
            chosen.push(environment);
        }
    }
    chosen
}

pub(crate) fn workspace(environment: &Environment) -> Workspace {
    Workspace::new(config::for_root(
        &environment.root,
        environment.github_token_file.clone(),
        environment.branch_prefix.clone(),
    ))
}

/// Run one job for one environment and report what it did.
pub fn run_job(
    settings: &Settings,
    environment: &Environment,
    job: Job,
    dry_run: bool,
) -> Result<Value> {
    if !environment.root.is_dir() {
        bail!("{} is not a folder", environment.root.display());
    }
    let workspace = workspace(environment);
    let mut report = match job {
        Job::Refresh => json!({ "repos": workspace.refresh()? }),
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

// ---- cleanup --------------------------------------------------------------

/// Whether any process has a working directory inside `dir`. `None` where the
/// system cannot say, which callers must read as "it might".
fn in_use(dir: &Path) -> Option<bool> {
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

/// Why a session must be kept, or nothing if it can go.
fn keep_reason(
    workspace: &Workspace,
    sharing: &[String],
    store: &Store,
    name: &str,
    path: &Path,
    min_age: Duration,
) -> Result<Option<String>> {
    // Two logins may share one nucleus, so the folder is live while an agent
    // session of any of them is working in it.
    let agent_sessions: Vec<_> = sharing
        .iter()
        .flat_map(|environment| store.list(environment))
        .filter(|session| session.workspace.as_deref() == Some(name))
        .collect();
    if agent_sessions
        .iter()
        .any(|session| session.status != Status::Ended)
    {
        return Ok(Some("its agent session has not ended".to_string()));
    }
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    match in_use(&resolved) {
        Some(false) => {}
        Some(true) => return Ok(Some("a process is working in it".to_string())),
        None => {
            return Ok(Some(
                "this system cannot say whether a process is working in it".to_string(),
            ))
        }
    }
    let manifest = Workspace::load_manifest(path)?;
    let mut last_change = modified_ms(path).max(modified_ms(&path.join(".workspace.json")));
    for session in &agent_sessions {
        last_change = last_change.max(session.last_event_ms);
    }
    for repo in manifest.repos.values().filter(|repo| repo.path.exists()) {
        let committed = git::tree(&repo.path, &["log", "-1", "--format=%ct"])
            .ok()
            .and_then(|seconds| seconds.parse::<u64>().ok())
            .unwrap_or(u64::MAX / 1000);
        last_change = last_change.max(committed.saturating_mul(1000));
    }
    let age_ms = unix_ms(SystemTime::now()).saturating_sub(last_change);
    if age_ms < min_age.as_millis() as u64 {
        return Ok(Some("it changed too recently".to_string()));
    }
    for (repo, info) in &manifest.repos {
        if info.path.exists() {
            let reasons = workspace.unsafe_reasons(&info.path)?;
            if !reasons.is_empty() {
                return Ok(Some(format!("{repo}: {}", reasons.join("; "))));
            }
        }
    }
    Ok(None)
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

fn snapshot_one(
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
    let skipped = |reason: &str| Ok(entry("skipped", json!({ "reason": reason })));
    let branch = git::tree(worktree, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return skipped("not on a branch");
    }
    let (ok, status, error) = git::tree_raw(worktree, &["status", "--porcelain", "-z"], &[])?;
    if !ok {
        bail!("git status failed: {error}");
    }
    let changed = changed_paths(&status);
    if changed.is_empty() {
        return skipped("nothing uncommitted");
    }
    let mut budget = 20_000;
    if changed
        .iter()
        .any(|path| recently_written(&worktree.join(path), now, &mut budget))
    {
        return skipped("being written to");
    }

    // Build the tree in a copy of the index, so neither the session's index
    // nor its branch is touched.
    let git_dir = PathBuf::from(git::tree(worktree, &["rev-parse", "--absolute-git-dir"])?);
    paths::create_private_dir(keys)?;
    let index = keys.join(format!(".index.{}.{}", std::process::id(), dashed(&branch)));
    if git_dir.join("index").is_file() {
        fs::copy(git_dir.join("index"), &index)?;
    }
    let with_index: [(&str, &OsStr); 1] = [("GIT_INDEX_FILE", index.as_os_str())];
    let built = (|| -> Result<String> {
        let (ok, _, error) = git::tree_raw(worktree, &["add", "-A"], &with_index)?;
        if !ok {
            bail!("git add failed: {error}");
        }
        let (ok, tree, error) = git::tree_raw(worktree, &["write-tree"], &with_index)?;
        if !ok {
            bail!("git write-tree failed: {error}");
        }
        Ok(String::from_utf8_lossy(&tree).trim().to_string())
    })();
    let _ = fs::remove_file(&index);
    let tree = built?;

    if tree == git::tree(worktree, &["rev-parse", "HEAD^{tree}"])? {
        return skipped("nothing uncommitted");
    }
    let reference = format!("refs/wip/{}", dashed(&branch));
    let key = keys.join(format!("{repo}-{}", dashed(&branch)));
    if fs::read_to_string(&key).is_ok_and(|pushed| pushed.trim() == tree) {
        return skipped("already snapshotted");
    }
    if dry_run {
        return Ok(entry(
            "would_snapshot",
            json!({ "branch": branch, "ref": reference }),
        ));
    }
    // commit-tree needs an author; a box with none configured still snapshots.
    let unnamed =
        git::try_tree(worktree, &["config", "user.email"]).map_or(true, |o| o.stdout.is_empty());
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
    let (ok, commit, error) = git::tree_raw(
        worktree,
        &["commit-tree", &tree, "-p", "HEAD", "-m", &message],
        &identity,
    )?;
    if !ok {
        bail!("git commit-tree failed: {error}");
    }
    let commit = String::from_utf8_lossy(&commit).trim().to_string();
    let pushed = git::try_tree(
        worktree,
        &[
            "push",
            "-q",
            "--force",
            "origin",
            &format!("{commit}:{reference}"),
        ],
    )?;
    if !pushed.ok {
        return Ok(entry(
            "failed",
            json!({ "branch": branch, "reason": "the push was refused" }),
        ));
    }
    fs::write(&key, format!("{tree}\n"))?;
    Ok(entry(
        "snapshot",
        json!({ "branch": branch, "ref": reference }),
    ))
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
    for item in workspace.available()? {
        for worktree in Workspace::worktrees(&item.bare).unwrap_or_default() {
            if !worktree.is_dir() {
                continue;
            }
            report.push(
                snapshot_one(&worktree, &item.repo, &keys, dry_run, now).unwrap_or_else(|error| {
                    json!({
                        "repo": item.repo, "worktree": worktree,
                        "action": "failed", "reason": format!("{error:#}"),
                    })
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
            Some(action) => action != "failed",
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
