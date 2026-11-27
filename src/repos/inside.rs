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
    valid_label, valid_name, valid_record, valid_wip_ref, Answer, Backup, Facts, Fetched, Folder,
    Holding, Op, Outcome, Pushed, Removal, Repo, SnapAction, Snapshot, Word, Worktree, MAX_ANSWER,
    MAX_COUNT, MAX_FOLDERS, MAX_FOLDER_REPOS, MAX_LABELS, MAX_WORKTREES,
};
use super::transfer;
use crate::driver::{self, Condition};
use crate::envelope::{unix_ms, valid_branch};
use crate::housekeeping::Snapped;
use crate::reporter::commit_id;
use crate::settings::{self, Environment, Settings};
use crate::workspace::{Manifest, RepoInfo, Workspace, SUBWORKTREES};
use crate::worktrees::{clean_up_core, Cleaned, Cleaning, Committing};
use crate::worktrees::{last_change, reportable_branch, valid_folder};
use crate::{bounded, git, housekeeping, paths};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
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
#[derive(Default)]
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
    /// The session folders asked about: the one a census or a Push reads,
    /// or each a removal removes.
    pub folders: &'a [String],
    /// For a removal, each folder's stamp as the host last saw it.
    pub expect: &'a [String],
    /// A driver's worktrees are asked about, not a nucleus's.
    pub driver: bool,
    /// Report what would be done, and do none of it.
    pub dry_run: bool,
    /// A refresh that only moves repos to their owner folders.
    pub move_only: bool,
    /// The trees the host has recorded as snapshotted.
    pub pushed: &'a [String],
    /// The commits GitHub said were merged, as the host asked it.
    pub merged: &'a [String],
    /// A removal that may drop unsaved work once it is backed up.
    pub discard: bool,
    /// The Clean up whose backups those are.
    pub id: Option<&'a str>,
}

/// Whether the time is up, and whether a removal is under way, which the
/// time's being up waits for.
#[derive(Default)]
struct Clock {
    up: bool,
    committing: bool,
}

static CLOCK: Mutex<Clock> = Mutex::new(Clock {
    up: false,
    committing: false,
});
static TURN: Condvar = Condvar::new();
/// The longest the time's being up waits for a removal under way.
const COMMIT_GRACE: Duration = Duration::from_secs(8);

fn clock() -> std::sync::MutexGuard<'static, Clock> {
    CLOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start a removal, unless the time is up.
fn begin() -> bool {
    let mut clock = clock();
    if clock.up {
        return false;
    }
    clock.committing = true;
    true
}

/// A removal begun is over, and what came of it is in the answer.
fn release() {
    clock().committing = false;
    TURN.notify_all();
}

fn time_is_up() -> bool {
    clock().up
}

/// Run one call and print its answer. Never fails, always prints one answer,
/// always exits 0 within the time it was given, and a little more for a
/// removal under way.
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
        let mut held = clock();
        held.up = true;
        let waited = std::time::Instant::now();
        while held.committing && waited.elapsed() < COMMIT_GRACE {
            held = TURN
                .wait_timeout(held, COMMIT_GRACE)
                .map(|(held, _)| held)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
        drop(held);
        // What it did is taken before anything is stopped, so that what
        // stopping does is not taken for something it did.
        let partial = answer_so_far().take().map(|mut answer| {
            answer.condition = Condition::Timeout;
            answer.complete = false;
            fit(answer)
        });
        bounded::stop_all();
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
    let folder = asked.folders.first().map(String::as_str);
    match op {
        Op::Census => census(&settings, environment, folder, asked.driver),
        Op::Refresh => refresh(&settings, environment, asked.dry_run, asked.move_only),
        Op::Snapshot => snapshot(&settings, environment, asked.dry_run, asked.pushed),
        Op::Push => match folder {
            Some(folder) if valid_folder(folder) => {
                push(&settings, environment, folder, asked.merged)
            }
            _ => Condition::BadReply,
        },
        Op::Remove => {
            let discard = match (asked.discard, asked.id) {
                (false, _) => None,
                (true, Some(id)) if crate::worktrees::valid_id(id) => Some(id),
                (true, _) => return Condition::BadReply,
            };
            let fits = asked.folders.len() == asked.expect.len()
                && asked.folders.iter().all(|folder| valid_folder(folder));
            if !fits {
                return Condition::BadReply;
            }
            remove(&settings, environment, asked, discard)
        }
    }
}

/// The workspace of an environment as `nucleated repos` sees it, inside
/// the environment's container: git run guarded, as the daemon runs it on
/// the host, and stopped with the call when its time is up.
fn workspace_inside(settings: &Settings, environment: &Environment) -> Workspace {
    super::guarded(settings, environment, true)
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
    let workspace =
        workspace_inside(settings, environment).leaving_alone(driver::protected_roots(settings));
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

// ---- the writes -----------------------------------------------------------

/// A private folder of this call's own, for copies of indexes: made new,
/// never one that was there, and removed when it is dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Option<Scratch> {
        use std::os::unix::fs::DirBuilderExt;
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).ok()?;
        let name = format!("nucleated-repos-{:016x}", u64::from_be_bytes(bytes));
        let path = std::env::temp_dir().join(name);
        std::fs::DirBuilder::new().mode(0o700).create(&path).ok()?;
        Some(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A word for what failed in a call that is a refusal by the vetting, or
/// `otherwise`.
fn word_of(error: &anyhow::Error, otherwise: Word) -> Word {
    git::refusal(error).map_or(otherwise, |refused| Word::of(refused.reason))
}

/// Move the nucleus's repos to their owner folders, unless `dry_run`; then,
/// unless `move_only`, fetch each one.
fn refresh(
    settings: &Settings,
    environment: &Environment,
    dry_run: bool,
    move_only: bool,
) -> Condition {
    let workspace =
        workspace_inside(settings, environment).leaving_alone(driver::protected_roots(settings));
    if !dry_run {
        match workspace.move_to_owner_folders() {
            Ok(moved) => building(|answer| {
                answer.moved = u32::try_from(moved.moved.len())
                    .unwrap_or(MAX_COUNT)
                    .min(MAX_COUNT)
            }),
            Err(_) => building(|answer| answer.why = Some(Word::Git)),
        }
    }
    if move_only {
        building(|answer| answer.complete = true);
        return Condition::Ok;
    }
    let git = workspace.git();
    let available = workspace.available().unwrap_or_else(|_| {
        building(|answer| answer.unreadable = true);
        Vec::new()
    });
    for item in available {
        if !valid_name(&item.repo) {
            building(|answer| answer.unreadable = true);
            continue;
        }
        let refused = git.enforcing().then(|| git.vet_bare(&item.bare)).flatten();
        let fetched = match refused {
            Some(refused) => Fetched {
                repo: item.repo.clone(),
                ok: false,
                failure: None,
                refused: Some(refused.reason),
            },
            None => {
                let failure = match transfer::fetch(git, &item.bare) {
                    Ok(Ok(())) => None,
                    Ok(Err(word)) => Some(word),
                    Err(error) => Some(word_of(&error, Word::CannotRun)),
                };
                Fetched {
                    repo: item.repo.clone(),
                    ok: failure.is_none(),
                    failure,
                    refused: None,
                }
            }
        };
        building(|answer| answer.fetched.push(fetched));
    }
    building(|answer| answer.complete = true);
    Condition::Ok
}

/// The session folder a worktree is in, if it is in one of the nucleus's.
fn folder_of(sessions: &Path, worktree: &Path) -> Option<String> {
    let real = worktree.canonicalize().ok()?;
    let below = real.strip_prefix(sessions).ok()?;
    let first = below.components().next()?.as_os_str().to_str()?;
    valid_folder(first).then(|| first.to_string())
}

/// Snapshot every worktree of the nucleus's own, as the host's snapshot job
/// does, but with the copies of indexes in a folder of this call's own, and
/// no tree the host has recorded as pushed pushed again.
fn snapshot(
    settings: &Settings,
    environment: &Environment,
    dry_run: bool,
    pushed: &[String],
) -> Condition {
    let workspace =
        workspace_inside(settings, environment).leaving_alone(driver::protected_roots(settings));
    let Some(scratch) = Scratch::new() else {
        return Condition::Error;
    };
    // What the host recorded as pushed, by repo and branch.
    let pushed: std::collections::BTreeMap<&str, &str> = pushed
        .iter()
        .filter_map(|pair| pair.rsplit_once('='))
        .filter(|(name, tree)| valid_record(name) && commit_id(tree))
        .collect();
    let repo_now = std::cell::RefCell::new(String::new());
    let already = |branch: &str, tree: &str| {
        let name = format!("{}-{}", repo_now.borrow(), branch.replace('/', "-"));
        pushed.get(name.as_str()) == Some(&tree)
    };
    let how = housekeeping::Snapping {
        index_dir: &scratch.0,
        plain_index: true,
        already: &already,
        usable: &valid_wip_ref,
        dry_run,
        now: std::time::SystemTime::now(),
    };
    let sessions = environment
        .root
        .join(".worktrees")
        .canonicalize()
        .unwrap_or_default();
    let git = workspace.git();
    let available = workspace.available().unwrap_or_else(|_| {
        building(|answer| answer.unreadable = true);
        Vec::new()
    });
    let mut room = MAX_WORKTREES;
    let mut complete = true;
    let entry = |repo: &str, folder: Option<String>, action, reason, branch: String| Snapshot {
        repo: repo.to_string(),
        folder,
        branch: Some(branch)
            .filter(|b| reportable_branch(b))
            .unwrap_or_default(),
        action,
        reason,
        tree: None,
        reference: None,
    };
    'repos: for item in available {
        if !valid_name(&item.repo) {
            building(|answer| answer.unreadable = true);
            continue;
        }
        *repo_now.borrow_mut() = item.repo.clone();
        let worktrees = match workspace.worktrees(&item.bare) {
            Ok(worktrees) => worktrees,
            Err(error) => {
                if let Some(refused) = git::refusal(&error) {
                    let one = entry(
                        &item.repo,
                        None,
                        SnapAction::Refused,
                        Some(Word::of(refused.reason)),
                        String::new(),
                    );
                    building(|answer| answer.snapshots.push(one));
                }
                continue;
            }
        };
        for worktree in worktrees.iter().filter(|worktree| worktree.is_dir()) {
            if room == 0 {
                complete = false;
                break 'repos;
            }
            room -= 1;
            let folder = folder_of(&sessions, worktree);
            // A snapshot writes objects into the repository and pushes a
            // ref: in a worktree that is not the nucleus's own, it is not
            // made.
            if workspace.leaves_alone(worktree) {
                let word = match workspace.under_a_driver(worktree) {
                    true => Word::OwnedByADriver,
                    false => Word::NotOurs,
                };
                let one = entry(
                    &item.repo,
                    folder,
                    SnapAction::Skipped,
                    Some(word),
                    String::new(),
                );
                building(|answer| answer.snapshots.push(one));
                continue;
            }
            let one = match housekeeping::snapshot_core(git, worktree, &how) {
                Ok(Snapped::Skipped(word)) => entry(
                    &item.repo,
                    folder,
                    SnapAction::Skipped,
                    Some(word),
                    String::new(),
                ),
                Ok(Snapped::Would {
                    branch,
                    reference,
                    tree,
                }) => Snapshot {
                    tree: Some(tree),
                    reference: Some(reference),
                    ..entry(&item.repo, folder, SnapAction::WouldSnapshot, None, branch)
                },
                Ok(Snapped::Done {
                    branch,
                    reference,
                    tree,
                }) => Snapshot {
                    tree: Some(tree),
                    reference: Some(reference),
                    ..entry(&item.repo, folder, SnapAction::Snapshot, None, branch)
                },
                Ok(Snapped::Failed { branch, stderr }) => entry(
                    &item.repo,
                    folder,
                    SnapAction::Failed,
                    Some(transfer::word(&stderr)),
                    branch,
                ),
                Err(error) => match git::refusal(&error) {
                    Some(refused) => entry(
                        &item.repo,
                        folder,
                        SnapAction::Refused,
                        Some(Word::of(refused.reason)),
                        String::new(),
                    ),
                    None => entry(
                        &item.repo,
                        folder,
                        SnapAction::Failed,
                        Some(Word::Git),
                        String::new(),
                    ),
                },
            };
            building(|answer| answer.snapshots.push(one));
        }
    }
    building(|answer| answer.complete = complete);
    Condition::Ok
}

/// The session folder `name`, held open as an outcome holds it: the folder
/// itself, or the word for why it cannot be had.
fn session_folder(environment: &Environment, name: &str) -> Result<(paths::Dir, PathBuf), Word> {
    let sessions = environment.root.join(".worktrees");
    let held = paths::Dir::open(&sessions, false).map_err(|_| Word::Unreadable)?;
    match held.at(name) {
        Ok(session) => Ok((session, sessions.join(name))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Word::Gone),
        Err(_) => Err(Word::Unreadable),
    }
}

/// The name a repo goes by in a write's answer.
fn reported(name: &str) -> String {
    match valid_label(name) {
        true => name.to_string(),
        false => "repo".to_string(),
    }
}

/// Watch's Push for one session folder.
fn push(
    settings: &Settings,
    environment: &Environment,
    folder: &str,
    merged: &[String],
) -> Condition {
    let workspace = workspace_inside(settings, environment)
        .leaving_alone(driver::protected_roots(settings))
        .knowing_merged(merged);
    let why = |word: Word| {
        building(|answer| {
            answer.why = Some(word);
            answer.complete = true;
        });
        Condition::Ok
    };
    let (session, path) = match session_folder(environment, folder) {
        Ok(found) => found,
        Err(word) => return why(word),
    };
    if Workspace::load_manifest_in(&session, &path).is_err() {
        return why(Word::NotASession);
    }
    let Ok(worktrees) = crate::worktrees::worktrees_of(&workspace, &path) else {
        return why(Word::Unreadable);
    };
    let left_alone = workspace.leaves_alone(&path)
        || worktrees
            .iter()
            .any(|(_, info)| workspace.leaves_alone(&info.path));
    if left_alone {
        return why(Word::LeftAlone);
    }
    crate::worktrees::push_each(&workspace, &worktrees, &mut |one| {
        let pushed = Pushed {
            repo: reported(&one.name),
            pushed: one.pushed.min(MAX_COUNT),
            uncommitted: one.uncommitted,
            failure: one.failure,
        };
        building(|answer| answer.pushes.push(pushed));
    });
    building(|answer| answer.complete = true);
    Condition::Ok
}

/// Remove each folder asked about, as cleanup and Watch's Clean up do,
/// each only if it still stands as the host saw it, and none once the time
/// is up. One begun is finished.
fn remove(
    settings: &Settings,
    environment: &Environment,
    asked: &Asked,
    discard: Option<&str>,
) -> Condition {
    let workspace = workspace_inside(settings, environment)
        .leaving_alone(driver::protected_roots(settings))
        .knowing_merged(asked.merged);
    let Some(scratch) = Scratch::new() else {
        return Condition::Error;
    };
    for (name, expect) in asked.folders.iter().zip(asked.expect) {
        if time_is_up() {
            return Condition::Ok;
        }
        let removal = remove_one(&workspace, environment, name, expect, discard, &scratch.0);
        let Some(removal) = removal else {
            // Its removal was not started: the time was up.
            release();
            return Condition::Ok;
        };
        building(|answer| answer.removals.push(removal));
        release();
    }
    if !time_is_up() {
        let _ = workspace.prune_worktrees();
        building(|answer| answer.complete = true);
    }
    Condition::Ok
}

fn remove_one(
    workspace: &Workspace,
    environment: &Environment,
    name: &str,
    expect: &str,
    discard: Option<&str>,
    keys: &Path,
) -> Option<Removal> {
    let came = |outcome, why| Removal {
        folder: name.to_string(),
        outcome,
        why,
        repo: None,
        holding: Vec::new(),
        other_files: false,
        backed_up: Vec::new(),
    };
    let (session, path) = match session_folder(environment, name) {
        Ok(found) => found,
        Err(Word::Gone) => return Some(came(Outcome::Gone, None)),
        Err(word) => return Some(came(Outcome::Failed, Some(word))),
    };
    let Ok(manifest) = Workspace::load_manifest_in(&session, &path) else {
        return Some(came(Outcome::Kept, Some(Word::NotASession)));
    };
    let (now, _) = folder_entry(workspace, name, &path, Some(manifest));
    if now.unreadable {
        return Some(came(Outcome::Failed, Some(Word::Unreadable)));
    }
    if now.stamp.as_deref() != Some(expect) {
        return Some(came(Outcome::Kept, Some(Word::Changed)));
    }
    let Ok(worktrees) = crate::worktrees::worktrees_of(workspace, &path) else {
        return Some(came(Outcome::Failed, Some(Word::Unreadable)));
    };
    let left_alone = workspace.leaves_alone(&path)
        || worktrees
            .iter()
            .any(|(_, info)| workspace.leaves_alone(&info.path));
    if left_alone {
        return Some(came(Outcome::Kept, Some(Word::LeftAlone)));
    }
    // A process in the container working in it, or a container that cannot
    // say, keeps it.
    let in_use = || housekeeping::in_use(&path) != Some(false);
    if in_use() {
        return Some(came(Outcome::Kept, Some(Word::InUse)));
    }
    let committing = Committing {
        begin: &begin,
        end: &|| {},
    };
    let how = Cleaning {
        discard,
        in_use: &in_use,
        keys,
        plain: true,
        usable: &valid_wip_ref,
        committing: &committing,
    };
    let pairs = |list: Vec<(String, String)>| -> Vec<Backup> {
        list.into_iter()
            .map(|(repo, reference)| Backup {
                repo: reported(&repo),
                reference,
            })
            .collect()
    };
    Some(match clean_up_core(workspace, &path, &how) {
        Cleaned::NotStarted => return None,
        Cleaned::Removed { backed_up } => Removal {
            backed_up: pairs(backed_up),
            ..came(Outcome::Removed, None)
        },
        Cleaned::Holding {
            holding,
            other_files,
        } => {
            let why = match holding.is_empty() {
                true => Word::OtherFiles,
                false => Word::Unsaved,
            };
            Removal {
                holding: holding
                    .into_iter()
                    .take(MAX_FOLDER_REPOS)
                    .map(|(repo, found)| Holding {
                        repo: reported(&repo),
                        unpushed: found.unpushed.min(MAX_COUNT),
                        uncommitted: found.uncommitted,
                    })
                    .collect(),
                other_files,
                ..came(Outcome::Kept, Some(why))
            }
        }
        Cleaned::Failed {
            why,
            repo,
            backed_up,
        } => Removal {
            repo: repo.map(|repo| reported(&repo)),
            backed_up: pairs(backed_up),
            ..came(Outcome::Failed, Some(why))
        },
    })
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
