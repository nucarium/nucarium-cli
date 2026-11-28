//! Where a bare repo lives in `.repos`, and moving a nucleus from the flat
//! layout (`.repos/<name>.git`) to owner folders (`.repos/<owner>/<name>.git`).
//!
//! A GitHub repo lives under its owner. Any other, from another host, lives
//! under [`NO_OWNER`]: GitHub allows only letters, digits and `-` in an
//! owner's name, so `_` is never one, and an "owner" read from another host's
//! path would follow no rule at all. Every repo is then two levels down, and
//! a `*.git` directly in `.repos` is always one not moved yet.

use super::{valid_name, Manifest, Workspace};
use crate::github;
use crate::paths::Dir;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The folder for repos with no GitHub owner.
pub const NO_OWNER: &str = "_";

/// Held while repos are moved, so that two runs never move the same one.
const LOCK: &str = ".layout.lock";

/// The longest a run waits for another to finish before leaving the move to
/// the next one. A daemon starting must not wait on whatever holds it.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The most a worktree pointer file is read to.
const MAX_POINTER: u64 = 4096;

/// A name GitHub could give an owner: letters, digits and `-`, not first.
pub(crate) fn valid_owner(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The owner folder for a repo fetched from `url`: its GitHub owner, or
/// [`NO_OWNER`]. A remote is whatever a repo's config says, which a session
/// can write, so nothing that is not a plain name ever becomes a folder.
pub(crate) fn owner_of(url: &str) -> String {
    github::slug(url)
        .filter(|(owner, repo)| valid_owner(owner) && valid_name(repo))
        .map(|(owner, _)| owner)
        .unwrap_or_else(|| NO_OWNER.to_string())
}

/// The label of a repo in an owner folder: `owner/name`, or the name alone
/// for one with no owner.
pub(crate) fn label(owner: &str, name: &str) -> String {
    if owner == NO_OWNER {
        name.to_string()
    } else {
        format!("{owner}/{name}")
    }
}

/// The folder in `repos` that `owner` already has, spelled as it is there,
/// or a new one spelled as given.
pub(crate) fn owner_dir(repos: &Path, owner: &str) -> PathBuf {
    fs::read_dir(repos)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .find(|have| have.eq_ignore_ascii_case(owner))
        .map_or_else(|| repos.join(owner), |have| repos.join(have))
}

/// The bare repos in owner folders: `(owner, name, path)`.
pub(crate) fn nested(repos: &Path) -> Vec<(String, String, PathBuf)> {
    let mut found = Vec::new();
    for owner in fs::read_dir(repos).into_iter().flatten().flatten() {
        let owner_name = owner.file_name().to_string_lossy().to_string();
        let is_dir = owner.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_dir || !(owner_name == NO_OWNER || valid_owner(&owner_name)) {
            continue;
        }
        for repo in fs::read_dir(owner.path()).into_iter().flatten().flatten() {
            let file = repo.file_name().to_string_lossy().to_string();
            let Some(name) = file.strip_suffix(".git").filter(|name| valid_name(name)) else {
                continue;
            };
            if repo.file_type().is_ok_and(|kind| kind.is_dir()) {
                found.push((owner_name.clone(), name.to_string(), repo.path()));
            }
        }
    }
    found
}

/// Where a repo a manifest names in the flat layout is now: the one in an
/// owner folder under the same name whose label the manifest has, else the
/// only one of that name. Nothing for a path that is not a flat repo's in
/// `repos`, this nucleus's own: a session writes its manifest, and one that
/// names a `.repos` of its own making is not sent looking through it.
pub(crate) fn relocated(repos: &Path, bare: &Path, label: &str) -> Option<PathBuf> {
    let named = bare.parent()?;
    if named != repos && named.canonicalize().ok()? != repos.canonicalize().ok()? {
        return None;
    }
    let file = bare.file_name()?.to_string_lossy().to_string();
    let name = file.strip_suffix(".git")?;
    let candidates: Vec<(String, PathBuf)> = nested(repos)
        .into_iter()
        .filter(|(_, have, _)| have == name)
        .map(|(owner, have, path)| (self::label(&owner, &have), path))
        .collect();
    if let Some((_, path)) = candidates
        .iter()
        .find(|(have, _)| have.eq_ignore_ascii_case(label))
    {
        return Some(path.clone());
    }
    match candidates.as_slice() {
        [(_, only)] => Some(only.clone()),
        _ => None,
    }
}

/// The most worktrees of one repo looked at.
const MAX_WORKTREES: usize = 10_000;

/// The worktree a pointer read from a repo's `worktrees/<id>/gitdir` names,
/// resolved, if the pointer is absolute, names that worktree's `.git`, and
/// the worktree is a folder in `sessions` (resolved). Nothing for a pointer
/// anywhere else, through a link out of `sessions` included, and nothing
/// for a relative one: git reads that from `worktrees/<id>`, and this would
/// not.
fn in_sessions(pointer: &str, sessions: &Path) -> Option<PathBuf> {
    let pointer = Path::new(pointer.trim_end_matches(['\n', '\r']));
    if !pointer.is_absolute() || pointer.file_name()? != ".git" {
        return None;
    }
    let worktree = pointer.parent()?.canonicalize().ok()?;
    (worktree != sessions && worktree.starts_with(sessions)).then_some(worktree)
}

/// Each worktree `bare` has registered, as its `worktrees/<id>/gitdir`
/// names it, read without following a link: `(id, pointer)`. An entry whose
/// pointer cannot be read that way is kept, with no pointer. At most
/// [`MAX_WORKTREES`] of them.
fn registered(root: &Path, bare: &Path) -> Vec<(String, Option<String>)> {
    let Ok(held) = Dir::beneath(root, &bare.join("worktrees")) else {
        return Vec::new();
    };
    held.names(MAX_WORKTREES)
        .into_iter()
        .map(|id| {
            let pointer = held
                .at(&id)
                .and_then(|entry| entry.read_plain("gitdir", MAX_POINTER))
                .ok();
            (id, pointer)
        })
        .collect()
}

/// Each worktree `bare` has registered, with the folder in the nucleus's
/// `sessions` it is, or why it is none of them.
fn placed(root: &Path, bare: &Path, sessions: &Path) -> Vec<(String, Result<PathBuf, String>)> {
    let registered = registered(root, bare);
    if registered.len() >= MAX_WORKTREES {
        return vec![(
            String::new(),
            Err("it has more worktrees than are looked at".to_string()),
        )];
    }
    let sessions = sessions.canonicalize().ok();
    registered
        .into_iter()
        .map(|(id, pointer)| {
            let worktree = sessions
                .as_deref()
                .zip(pointer.as_deref())
                .and_then(|(sessions, pointer)| in_sessions(pointer, sessions))
                .ok_or_else(|| format!("worktree {id} is not one of this nucleus's sessions"));
            (id, worktree)
        })
        .collect()
}

/// Whether every worktree `bare` has registered is a folder in the
/// nucleus's `sessions`, which a move needs: a worktree anywhere else could
/// not be pointed at its new place, and would be left broken.
fn all_in_sessions(root: &Path, bare: &Path, sessions: &Path) -> Result<(), String> {
    match placed(root, bare, sessions)
        .into_iter()
        .find_map(|(_, placed)| placed.err())
    {
        Some(why) => Err(format!("{why}; move it by hand")),
        None => Ok(()),
    }
}

/// Point each worktree `bare` has registered in the nucleus's `sessions` at
/// it again, as `git worktree repair` would, without running git: its
/// `.git` file is written anew, in the worktree's folder held open, reached
/// from `sessions` one folder at a time and never through a link. Nothing
/// is written anywhere else, whatever a repo's records say: a session can
/// write those, and could have git write into any folder on the box.
fn point_back(root: &Path, bare: &Path, sessions: &Path) -> Result<(), String> {
    let common = bare
        .canonicalize()
        .map_err(|error| format!("resolving {}: {error}", bare.display()))?;
    let held_to = sessions
        .canonicalize()
        .map_err(|error| format!("resolving {}: {error}", sessions.display()))?;
    let mut left = Vec::new();
    for (id, placed) in placed(root, bare, sessions) {
        let pointed = placed.and_then(|worktree| {
            let held = Dir::beneath(&held_to, &worktree)
                .map_err(|error| format!("worktree {id}: {error}"))?;
            // Only a `.git` file is replaced, or one that is missing: never
            // a folder, a link, or anything else in its place.
            match held.look(".git") {
                Ok(entry) if entry.regular => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(format!("worktree {id}: its .git is not a file")),
            }
            let pointer = format!("gitdir: {}\n", common.join("worktrees").join(&id).display());
            held.replace(".git", pointer.as_bytes())
                .map_err(|error| format!("worktree {id}: {error}"))
        });
        if let Err(why) = pointed {
            left.push(why);
        }
    }
    match left.is_empty() {
        true => Ok(()),
        false => Err(left.join("; ")),
    }
}

/// True when a worktree registered in `bare` still points at the place the
/// repo had before it moved, `.repos/<name>.git`, and that place is gone: a
/// move that stopped before its repair. Read from git's own files, without
/// running git or following a link, and only from a worktree in the
/// nucleus's `sessions`. A pointer anywhere else is not this routine's to
/// change: one a session planted is the vetting's to refuse and report.
fn needs_repair(root: &Path, bare: &Path, sessions: &Path) -> bool {
    let Some(file) = bare.file_name() else {
        return false;
    };
    let Ok(sessions) = sessions.canonicalize() else {
        return false;
    };
    registered(root, bare).into_iter().any(|(id, pointer)| {
        let Some(worktree) = pointer.as_deref().and_then(|p| in_sessions(p, &sessions)) else {
            return false;
        };
        // A worktree that is gone is git's to prune, not ours to repair.
        let Ok(text) = Dir::beneath(&sessions, &worktree)
            .and_then(|held| held.read_plain(".git", MAX_POINTER))
        else {
            return false;
        };
        let Some(target) = text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("gitdir:"))
            .map(|target| PathBuf::from(target.trim()))
        else {
            return false;
        };
        let old_repo = target.parent().and_then(Path::parent);
        !target.exists()
            && target.file_name() == Some(id.as_ref())
            && target.parent().and_then(Path::file_name) == Some("worktrees".as_ref())
            && old_repo.and_then(Path::file_name) == Some(file)
            && old_repo.and_then(Path::parent).and_then(Path::file_name) == Some(".repos".as_ref())
    })
}

/// Take the lock that lets one run at a time move repos, or nothing if
/// another holds it for longer than [`LOCK_WAIT`]. `.repos` is writable from
/// a session, so what is found there may be anything: a link is not
/// followed, a pipe is not waited on, and anything but a plain file is an
/// error.
fn take_turn(path: &Path) -> Result<Option<fs::File>> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !lock.metadata()?.is_file() {
        anyhow::bail!("{} is not a plain file", path.display());
    }
    let started = Instant::now();
    loop {
        // SAFETY: flock on a descriptor this function owns; released when
        // the file is dropped.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(lock));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(error).with_context(|| format!("locking {}", path.display()));
        }
        if started.elapsed() >= LOCK_WAIT {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What one run of the move did.
#[derive(Debug, Default, Serialize)]
pub struct Moved {
    /// Each repo moved: from, to.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Repos whose worktrees were pointed back at them.
    pub repaired: Vec<PathBuf>,
    /// Session manifests rewritten to name the new places.
    pub manifests: Vec<PathBuf>,
    /// Repos left where they are, and why.
    pub left: Vec<(PathBuf, String)>,
}

impl Moved {
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty()
            && self.repaired.is_empty()
            && self.manifests.is_empty()
            && self.left.is_empty()
    }

    /// One line per thing done or left, for a log.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (from, to) in &self.moved {
            lines.push(format!("moved {} to {}", from.display(), to.display()));
        }
        for bare in &self.repaired {
            lines.push(format!("repaired the worktrees of {}", bare.display()));
        }
        for manifest in &self.manifests {
            lines.push(format!("updated {}", manifest.display()));
        }
        for (bare, why) in &self.left {
            lines.push(format!("left {}: {why}", bare.display()));
        }
        lines
    }
}

impl Workspace {
    /// The nucleus's own folder, which `.repos` and the sessions are in.
    fn nucleus_root(&self) -> PathBuf {
        self.repos_dir
            .parent()
            .map_or_else(|| self.repos_dir.clone(), Path::to_path_buf)
    }

    /// Move every repo still in the flat layout into its owner folder, point
    /// its worktrees at it again, and have every session's manifest name it
    /// there. Also finishes a run that stopped part way. Safe to run any
    /// number of times, and by several processes at once: they take turns.
    pub fn move_to_owner_folders(&self) -> Result<Moved> {
        let mut moved = Moved::default();
        if !self.repos_dir.is_dir() {
            return Ok(moved);
        }
        let lock_path = self.repos_dir.join(LOCK);
        let Some(lock) = take_turn(&lock_path)? else {
            moved.left.push((
                self.repos_dir.clone(),
                format!(
                    "another run held {} for {}s; trying again next time",
                    lock_path.display(),
                    LOCK_WAIT.as_secs()
                ),
            ));
            return Ok(moved);
        };

        for entry in fs::read_dir(&self.repos_dir)?.flatten() {
            let from = entry.path();
            let file = entry.file_name().to_string_lossy().to_string();
            let Some(name) = file.strip_suffix(".git") else {
                continue;
            };
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            match self.move_one(&from, name) {
                Ok(to) => {
                    moved.moved.push((from, to.clone()));
                    moved.repaired.push(to);
                }
                Err(why) => moved.left.push((from, why)),
            }
        }

        // A run that stopped between moving a repo and repairing it.
        let root = self.nucleus_root();
        for (_, _, bare) in nested(&self.repos_dir) {
            if moved.repaired.contains(&bare) || !needs_repair(&root, &bare, &self.sessions_dir) {
                continue;
            }
            match point_back(&root, &bare, &self.sessions_dir) {
                Ok(()) => moved.repaired.push(bare),
                Err(why) => moved.left.push((bare, why)),
            }
        }

        // A session wrote its manifest, and can leave anything in its
        // folder: each is read and written in the folder held open, never
        // through a link, and one that cannot be is left for the next run.
        for (_, session) in self.sessions() {
            let Ok(held) = self.hold_session(&session) else {
                continue;
            };
            let Ok(mut manifest) = Self::read_manifest_in(&held, &session) else {
                continue;
            };
            if relocate_all(&mut manifest, &self.repos_dir) {
                let file = session.join(super::MANIFEST);
                match Self::save_manifest(&held, &manifest) {
                    Ok(()) => moved.manifests.push(file),
                    Err(error) => moved.left.push((file, format!("{error:#}"))),
                }
            }
        }
        drop(lock);
        Ok(moved)
    }

    /// Move one flat repo, or say why it stays.
    fn move_one(&self, from: &Path, name: &str) -> Result<PathBuf, String> {
        if !valid_name(name) {
            return Err("not a repo name nucleate gives".to_string());
        }
        // Moving needs git in the repo, which a refused repo never gets.
        if let Some(refused) = self.git.vet_bare(from) {
            return Err(format!("refused: {}", refused.reason.as_str()));
        }
        let is_bare = self
            .git
            .try_bare(from, &["rev-parse", "--is-bare-repository"])
            .map_err(|error| format!("{error:#}"))?;
        if !is_bare.ok || is_bare.stdout != "true" {
            return Err("not a bare repo".to_string());
        }
        let owner = owner_dir(&self.repos_dir, &owner_of(&self.remote_url(from)));
        let to = owner.join(format!("{name}.git"));
        if let Some(taken) = fs::read_dir(&owner)
            .into_iter()
            .flatten()
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&format!("{name}.git"))
            })
        {
            return Err(format!(
                "{} is already there; move one of them by hand",
                taken.path().display()
            ));
        }
        // Its worktrees are pointed at it again after the move, which only
        // a worktree in the nucleus's sessions can be.
        let root = self.nucleus_root();
        all_in_sessions(&root, from, &self.sessions_dir)?;
        fs::create_dir_all(&owner)
            .map_err(|error| format!("creating {}: {error}", owner.display()))?;
        fs::rename(from, &to).map_err(|error| format!("moving to {}: {error}", to.display()))?;
        point_back(&root, &to, &self.sessions_dir)
            .map_err(|why| format!("moved to {}, but {why}", to.display()))?;
        Ok(to)
    }
}

/// Point every repo of a manifest that names a place in `repos` no longer
/// there at where it is now. True if anything changed.
pub(crate) fn relocate_all(manifest: &mut Manifest, repos: &Path) -> bool {
    let mut changed = false;
    for info in manifest.repos.values_mut() {
        if info.bare.exists() {
            continue;
        }
        if let Some(now) = relocated(repos, &info.bare, &info.label) {
            info.bare = now;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::{in_sessions, label, owner_of, relocated, valid_owner, NO_OWNER};
    use std::fs;

    #[test]
    fn only_a_repo_of_this_nucleus_is_found_where_it_moved_to() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repos = root.join("nucleus").join(".repos");
        let now = repos.join("_").join("web.git");
        fs::create_dir_all(&now).unwrap();
        assert_eq!(
            relocated(&repos, &repos.join("web.git"), "web"),
            Some(now.clone())
        );
        // The same folder, named through a link.
        std::os::unix::fs::symlink(root.join("nucleus"), root.join("alias")).unwrap();
        let alias = root.join("alias").join(".repos").join("web.git");
        assert_eq!(relocated(&repos, &alias, "web"), Some(now));
        // A `.repos` a session made, laid out as a nucleus's is.
        let made = root.join("nucleus/.worktrees/s1/made/.repos");
        fs::create_dir_all(made.join("_").join("web.git")).unwrap();
        assert_eq!(relocated(&repos, &made.join("web.git"), "web"), None);
        // Not a flat repo's place at all.
        assert_eq!(relocated(&repos, &repos.join("_/web.git"), "web"), None);
        assert_eq!(relocated(&repos, &repos.join("web"), "web"), None);
    }

    #[test]
    fn only_an_absolute_pointer_to_a_sessions_worktree_is_one_to_point_back() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().canonicalize().unwrap().join(".worktrees");
        let worktree = sessions.join("s1/web");
        fs::create_dir_all(&worktree).unwrap();
        let pointer = format!("{}/.git\n", worktree.display());
        assert_eq!(in_sessions(&pointer, &sessions), Some(worktree.clone()));
        // Relative, git reads it from the repo's records, not from here.
        assert_eq!(in_sessions("s1/web/.git", &sessions), None);
        assert_eq!(in_sessions("./.git", &sessions), None);
        // Not a worktree's `.git`, the sessions folder itself, or outside.
        let other = format!("{}/HEAD", worktree.display());
        assert_eq!(in_sessions(&other, &sessions), None);
        let itself = format!("{}/.git", sessions.display());
        assert_eq!(in_sessions(&itself, &sessions), None);
        let outside = format!("{}/.git", dir.path().display());
        assert_eq!(in_sessions(&outside, &sessions), None);
        // Nor through a link out of it.
        std::os::unix::fs::symlink(dir.path(), sessions.join("out")).unwrap();
        let through = format!("{}/.git", sessions.join("out").display());
        assert_eq!(in_sessions(&through, &sessions), None);
    }

    #[test]
    fn a_github_remote_gives_its_owner() {
        assert_eq!(owner_of("https://github.com/octo/web.git"), "octo");
        assert_eq!(owner_of("git@github.com:Octo-Org/web"), "Octo-Org");
    }

    #[test]
    fn any_other_remote_has_no_owner() {
        assert_eq!(owner_of("/srv/git/web.git"), NO_OWNER);
        assert_eq!(owner_of("https://gitlab.com/group/web.git"), NO_OWNER);
        assert_eq!(owner_of(""), NO_OWNER);
    }

    #[test]
    fn a_remote_that_is_not_a_plain_name_has_no_owner() {
        assert_eq!(owner_of("https://github.com/../web.git"), NO_OWNER);
        assert_eq!(owner_of("https://github.com/a b/web.git"), NO_OWNER);
        assert_eq!(owner_of("https://github.com/x.git/web.git"), NO_OWNER);
        assert_eq!(owner_of("https://github.com/octo/../../x"), NO_OWNER);
        assert!(!valid_owner(NO_OWNER));
    }

    #[test]
    fn a_repo_with_no_owner_is_labelled_by_name() {
        assert_eq!(label("octo", "web"), "octo/web");
        assert_eq!(label(NO_OWNER, "web"), "web");
    }
}
