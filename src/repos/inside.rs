//! `nucleated repos`: git for the host, from inside an environment's
//! container.
//!
//! The daemon on the host runs no git in a folder a container writes. It
//! runs the box's own `nucleated`, mounted read-only at the same path in
//! every container, with one `podman exec` into the environment's
//! container, and this is what runs there. Git reads the container's repos
//! here, guarded as the daemon guards it, and what crosses to the host is one
//! answer (see [`super::answer`]) that holds only what may.
//!
//! It ends within its own time, whatever git does: when the time is up it
//! stops every git it started, prints what it has so far with `timeout`,
//! and leaves, so the `podman exec` that started it returns by itself. The
//! host keeps a deadline of its own as well, a little longer, and ends from
//! the box a call the container held past it (see
//! [`crate::driver::cut_off`]).

use super::answer::{
    valid_label, valid_name, Answer, Facts, Folder, Op, Repo, Worktree, MAX_ANSWER, MAX_FOLDERS,
    MAX_FOLDER_REPOS, MAX_LABELS, MAX_WORKTREES,
};
use crate::driver::{self, Condition};
use crate::envelope::{unix_ms, valid_branch};
use crate::reporter::commit_id;
use crate::settings::{self, Environment, Settings};
use crate::workspace::{Manifest, RepoInfo, Workspace, SUBWORKTREES};
use crate::worktrees::{last_change, reportable_branch, valid_folder};
use crate::{bounded, git, housekeeping, paths};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The least time a call is given, whatever it is asked for.
const LEAST: Duration = Duration::from_millis(100);
/// The most time a call is given, whatever it is asked for.
pub const MOST: Duration = Duration::from_secs(590);

/// The answer as far as it has got: what is printed when the time is up.
static ANSWER: Mutex<Option<Answer>> = Mutex::new(None);
/// Whether an answer has been printed. One answer, and only one, is printed.
static SAID: Mutex<bool> = Mutex::new(false);

/// Print `answer` unless one has been printed already.
fn say(answer: &Answer) {
    let mut said = SAID.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *said {
        return;
    }
    if let Ok(text) = serde_json::to_string(answer) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }
    *said = true;
}

fn answer_so_far() -> std::sync::MutexGuard<'static, Option<Answer>> {
    ANSWER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Add to the answer as far as it has got.
fn building(change: impl FnOnce(&mut Answer)) {
    if let Some(answer) = answer_so_far().as_mut() {
        change(answer);
    }
}

/// The answer cut, from its end, until it is within what the host reads:
/// what is left out makes it incomplete.
fn fit(mut answer: Answer) -> Answer {
    let size = |answer: &Answer| serde_json::to_vec(answer).map_or(u64::MAX, |t| t.len() as u64);
    while size(&answer) >= MAX_ANSWER {
        answer.complete = false;
        // A sixteenth at a time, from the end: few passes, little lost.
        let loose = answer.loose.len();
        let folders = answer.folders.len();
        if loose > 0 {
            answer.loose.truncate(loose - loose.div_ceil(16));
        } else if folders > 0 {
            answer.folders.truncate(folders - folders.div_ceil(16));
        } else {
            return Answer::nothing(answer.op, Condition::BadReply);
        }
    }
    answer
}

/// What the host asked of this call.
pub struct Asked<'a> {
    /// The environment whose container this is, by its config name.
    pub env: Option<&'a str>,
    /// What to do: one of [`Op`]'s words.
    pub op: Option<&'a str>,
    /// How long it may take, in milliseconds.
    pub within_ms: Option<u64>,
    /// The host's clock, in Unix milliseconds.
    pub now_ms: Option<u64>,
    /// The daemon's vetting mode: `enforce` or `report`. The config in a
    /// container has none of its own.
    pub vetting: Option<&'a str>,
    /// The one session folder asked about.
    pub folder: Option<&'a str>,
    /// A driver's worktrees are asked about, not a nucleus's.
    pub driver: bool,
}

/// Run one call and print its answer. Never fails, always prints one answer,
/// always exits 0 within the time it was given.
pub fn run(asked: &Asked) {
    let within = asked
        .within_ms
        .map(Duration::from_millis)
        .unwrap_or(MOST)
        .clamp(LEAST, MOST);
    let Some(op) = asked.op.and_then(Op::parse) else {
        say(&Answer::nothing(Op::Census, Condition::Unsupported));
        return;
    };
    *answer_so_far() = Some(Answer::nothing(op, Condition::Ok));
    std::thread::spawn(move || {
        std::thread::sleep(within);
        bounded::stop_all();
        let partial = answer_so_far().take().map(|mut answer| {
            answer.condition = Condition::Timeout;
            answer.complete = false;
            fit(answer)
        });
        say(&partial.unwrap_or_else(|| Answer::nothing(op, Condition::Timeout)));
        std::process::exit(0);
    });
    std::panic::set_hook(Box::new(|_| {}));
    let condition = std::panic::catch_unwind(|| one(asked, op)).unwrap_or(Condition::BadReply);
    let Some(mut answer) = answer_so_far().take() else {
        // The time is up, and what there is is being printed.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    };
    if condition != Condition::Ok {
        answer = Answer::nothing(op, condition);
    }
    say(&fit(answer));
}

fn one(asked: &Asked, op: Op) -> Condition {
    let Ok(mut settings) = settings::load() else {
        return Condition::Absent;
    };
    settings.git_vetting = match asked.vetting {
        Some("report") => git::Vetting::Report,
        _ => git::Vetting::Enforce,
    };
    let Some(environment) = asked.env.and_then(|name| settings.environment(name)) else {
        return Condition::Absent;
    };
    match op {
        Op::Census => census(&settings, environment, asked.folder, asked.driver),
    }
}

/// The folders an environment keeps work in, resolved: nothing outside them
/// is read.
fn held_to(environment: &Environment) -> Vec<PathBuf> {
    environment
        .own_folders()
        .iter()
        .filter_map(|folder| folder.canonicalize().ok())
        .collect()
}

/// Whether `path` is in one of `held`. A path that cannot be resolved is in
/// none of them.
fn inside(held: &[PathBuf], path: &Path) -> bool {
    path.canonicalize()
        .is_ok_and(|path| held.iter().any(|folder| path.starts_with(folder)))
}

fn modified_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(unix_ms)
        .unwrap_or(0)
}

/// The name a repo's folder in `.repos` gives it.
fn repo_of(bare: &Path) -> String {
    let name = bare
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    name.strip_suffix(".git").unwrap_or(&name).to_string()
}

/// Read every worktree of the nucleus, or one session folder's, or a
/// driver's worktrees, into the answer.
fn census(
    settings: &Settings,
    environment: &Environment,
    folder: Option<&str>,
    driver: bool,
) -> Condition {
    let workspace = housekeeping::workspace_inside(settings, environment)
        .leaving_alone(driver::protected_roots(settings));
    let held = held_to(environment);
    if driver {
        return census_driver(&workspace, environment, &held);
    }
    if let Some(name) = folder {
        if !valid_folder(name) {
            return Condition::BadReply;
        }
        let entry = one_folder(&workspace, environment, name);
        building(|answer| {
            answer.folders.push(entry);
            answer.complete = true;
        });
        return Condition::Ok;
    }
    let git = workspace.git();
    let available = workspace.available().unwrap_or_else(|_| {
        building(|answer| answer.unreadable = true);
        Vec::new()
    });
    let mut listed = Vec::new();
    for item in &available {
        if !inside(&held, &item.bare) {
            building(|answer| answer.left_out = true);
            continue;
        }
        if !valid_name(&item.repo) {
            building(|answer| answer.unreadable = true);
            continue;
        }
        let refused = git.vet_bare(&item.bare).map(|refused| refused.reason);
        building(|answer| {
            answer.repos.push(Repo {
                repo: item.repo.clone(),
                refused,
            })
        });
        if refused.is_none() || !git.enforcing() {
            listed.push(item);
        }
    }
    let mut room = MAX_WORKTREES;
    let mut complete = true;
    // The session folders. A worktree told of in one is not told of again.
    let mut told: BTreeSet<PathBuf> = BTreeSet::new();
    let mut folders = 0;
    for (name, path) in workspace.sessions() {
        if !valid_folder(&name) {
            continue;
        }
        if folders == MAX_FOLDERS {
            complete = false;
            break;
        }
        let (entry, paths) = folder_entry(&workspace, &name, &path, None);
        if entry.worktrees.len() > room {
            complete = false;
            break;
        }
        room -= entry.worktrees.len();
        told.extend(paths);
        folders += 1;
        building(|answer| answer.folders.push(entry));
    }
    // Then every other worktree of the nucleus's repos.
    'repos: for item in listed {
        let Ok(worktrees) = workspace.worktrees(&item.bare) else {
            building(|answer| answer.unreadable = true);
            continue;
        };
        let label = Some(item.label.clone()).filter(|label| valid_label(label));
        for path in worktrees {
            // One in a driver's own folders is the driver's, counted there.
            if !path.is_dir() || workspace.under_a_driver(&path) {
                continue;
            }
            if !inside(&held, &path) {
                building(|answer| answer.left_out = true);
                continue;
            }
            if path.canonicalize().is_ok_and(|real| told.contains(&real)) {
                continue;
            }
            if room == 0 {
                complete = false;
                break 'repos;
            }
            let label = label.clone().unwrap_or_else(|| item.repo.clone());
            match worktree_entry(&workspace, None, &item.repo, &label, &path, None) {
                Some((entry, _)) => {
                    room -= 1;
                    building(|answer| answer.loose.push(entry));
                }
                None => building(|answer| answer.unreadable = true),
            }
        }
    }
    building(|answer| answer.complete = complete);
    Condition::Ok
}

/// A driver's worktrees, as its adapter finds them in its folders.
fn census_driver(workspace: &Workspace, environment: &Environment, held: &[PathBuf]) -> Condition {
    let Some(adapter) = environment.driver_adapter() else {
        return Condition::Absent;
    };
    let Some(worktrees) = adapter.worktrees(environment) else {
        building(|answer| answer.unreadable = true);
        return Condition::Ok;
    };
    let mut complete = true;
    let mut room = MAX_WORKTREES;
    for path in worktrees.iter().filter(|path| path.is_dir()) {
        // A worktree in the driver's own folders, not at the end of a link
        // out of them.
        if !inside(held, path) {
            building(|answer| answer.left_out = true);
            continue;
        }
        if room == 0 {
            complete = false;
            break;
        }
        let name = Some(repo_of(path))
            .filter(|name| valid_name(name))
            .unwrap_or_else(|| "worktree".to_string());
        match worktree_entry(workspace, None, &name, &name, path, None) {
            Some((entry, _)) => {
                room -= 1;
                building(|answer| answer.loose.push(entry));
            }
            None => building(|answer| answer.unreadable = true),
        }
    }
    building(|answer| answer.complete = complete);
    Condition::Ok
}

/// One worktree, and the number made from what `git status` said of it.
/// Nothing when git could not say what it holds. A worktree the daemon will
/// not run git in is told of with why, and, when enforcing, nothing else.
fn worktree_entry(
    workspace: &Workspace,
    key: Option<&str>,
    repo: &str,
    label: &str,
    path: &Path,
    base: Option<&str>,
) -> Option<(Worktree, u64)> {
    let git = workspace.git();
    let mut entry = Worktree {
        key: key.map(str::to_string),
        repo: repo.to_string(),
        label: label.to_string(),
        refused: git.vet_tree(path).map(|refused| refused.reason),
        ..Worktree::default()
    };
    if entry.refused.is_some() && git.enforcing() {
        return Some((entry, 0));
    }
    let facts = workspace.facts(path).ok()?;
    let branch = git.tree(path, &["branch", "--show-current"]).ok()?;
    if reportable_branch(&branch) {
        entry.branch = branch;
    }
    entry.facts = Some(Facts::of(&facts));
    entry.commits = base.filter(|base| commit_id(base)).and_then(|base| {
        let range = format!("{base}..HEAD");
        let made = git
            .try_tree(path, &["rev-list", "--count", &range])
            .ok()
            .filter(|out| out.ok)?;
        made.stdout
            .parse::<u32>()
            .ok()
            .map(|made| made.min(super::answer::MAX_COUNT))
    });
    entry.last_commit_s = git
        .tree(path, &["log", "-1", "--format=%ct"])
        .ok()
        .and_then(|seconds| seconds.parse().ok());
    Some((entry, facts.changes))
}

/// The one session folder `name`, read as an outcome reads it: the
/// sessions' folder and the session's are each held open, never through a
/// link, and the manifest read from the session's folder held.
fn one_folder(workspace: &Workspace, environment: &Environment, name: &str) -> Folder {
    let unreadable = Folder {
        folder: name.to_string(),
        unreadable: true,
        ..Folder::default()
    };
    let sessions = environment.root.join(".worktrees");
    let Ok(held) = paths::Dir::open(&sessions, false) else {
        return unreadable;
    };
    let path = sessions.join(name);
    let session = match held.at(name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Folder {
                folder: name.to_string(),
                gone: true,
                ..Folder::default()
            }
        }
        Err(_) => return unreadable,
        Ok(session) => session,
    };
    let Ok(manifest) = Workspace::load_manifest_in(&session, &path) else {
        return unreadable;
    };
    folder_entry(workspace, name, &path, Some(manifest)).0
}

/// One session folder, and the resolved paths of the worktrees told of in
/// it. One that cannot be read in full is told of as unreadable, and with
/// nothing else.
fn folder_entry(
    workspace: &Workspace,
    name: &str,
    path: &Path,
    manifest: Option<Manifest>,
) -> (Folder, Vec<PathBuf>) {
    let unreadable = || {
        (
            Folder {
                folder: name.to_string(),
                unreadable: true,
                ..Folder::default()
            },
            Vec::new(),
        )
    };
    let manifest = match manifest {
        Some(manifest) => manifest,
        None => match Workspace::load_manifest(path) {
            Ok(manifest) => manifest,
            Err(_) => return unreadable(),
        },
    };
    let Ok(listed) = workspace.session_worktrees(path) else {
        return unreadable();
    };
    let mut folder = Folder {
        folder: name.to_string(),
        other_files: !Workspace::other_files(path, &listed).is_empty(),
        touched_ms: Some(modified_ms(path).max(modified_ms(&path.join(".workspace.json")))),
        branch: Some(manifest.branch.clone()).filter(|branch| valid_branch(branch)),
        labels: manifest
            .repos
            .values()
            .map(|info| info.label.clone())
            .filter(|label| valid_label(label))
            .take(MAX_LABELS)
            .collect(),
        ..Folder::default()
    };
    let present: Vec<&(String, RepoInfo)> = listed
        .iter()
        .filter(|(_, info)| info.path.is_dir())
        .collect();
    if present.is_empty() {
        let subs = path.join(SUBWORKTREES);
        let finished = std::fs::symlink_metadata(&subs).is_err()
            || std::fs::read_dir(&subs).is_ok_and(|mut inside| inside.next().is_none());
        folder.leftovers = !finished;
    }
    // The stamp: what is at the folder's root, and what each worktree holds.
    let mut stamped: Vec<u8> = Vec::new();
    let mut names: Vec<Vec<u8>> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().as_encoded_bytes().to_vec())
        .collect();
    names.sort();
    for name in names {
        stamped.extend(name);
        stamped.push(0);
    }
    let mut told = Vec::new();
    for (key, info) in present.iter().copied() {
        let repo = repo_of(&info.bare);
        // Its repo's label, else its key, as the report names it.
        let label = [info.label.as_str(), key.as_str()]
            .into_iter()
            .find(|name| valid_label(name))
            .map(str::to_string);
        let (Some(label), true) = (label, valid_name(&repo)) else {
            return unreadable();
        };
        let base = (!key.starts_with(SUBWORKTREES)).then_some(info.base.as_str());
        let Some((entry, changes)) =
            worktree_entry(workspace, Some(key), &repo, &label, &info.path, base)
        else {
            return unreadable();
        };
        let head = entry.facts.as_ref().map(|facts| facts.head.clone());
        stamped.extend(format!("{key}\0{}\0{changes:016x}\0", head.unwrap_or_default()).bytes());
        if let Ok(real) = info.path.canonicalize() {
            told.push(real);
        }
        folder.worktrees.push(entry);
    }
    let named: BTreeSet<&str> = folder.worktrees.iter().map(|w| w.label.as_str()).collect();
    if named.len() > MAX_FOLDER_REPOS {
        return unreadable();
    }
    let paths: Vec<&Path> = present
        .iter()
        .map(|(_, info)| info.path.as_path())
        .collect();
    folder.last_change_ms = last_change(workspace.git(), &paths);
    folder.stamp = Some(format!("{:016x}", driver::fnv(&stamped)));
    (folder, told)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_too_big_for_the_host_is_cut_and_says_so() {
        let mut answer = Answer::nothing(Op::Census, Condition::Ok);
        answer.complete = true;
        let worktree = Worktree {
            repo: "web".into(),
            label: "web".into(),
            branch: "b".repeat(250),
            ..Worktree::default()
        };
        answer.loose = vec![worktree; 10_000];
        let fitted = fit(answer);
        assert!(!fitted.complete);
        assert!(!fitted.loose.is_empty());
        assert!((serde_json::to_vec(&fitted).unwrap().len() as u64) < MAX_ANSWER);
    }
}
