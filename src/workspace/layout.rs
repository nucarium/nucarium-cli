//! Where a bare repo lives in `.repos`, and moving a nucleus from the flat
//! layout (`.repos/<name>.git`) to owner folders (`.repos/<owner>/<name>.git`).
//!
//! A GitHub repo lives under its owner. Any other, from another host or a
//! path on this machine, lives under [`NO_OWNER`]: GitHub allows only
//! letters, digits and `-` in an owner's name, so `_` is never one, and an
//! "owner" read from another host's path would follow no rule at all. Every
//! repo is then two levels down, and a `*.git` directly in `.repos` is always
//! one not moved yet.

use super::{valid_name, Manifest, Workspace};
use crate::{git, github};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// The folder for repos with no GitHub owner.
pub const NO_OWNER: &str = "_";

/// Held while repos are moved, so that two runs never move the same one.
const LOCK: &str = ".layout.lock";

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
/// only one of that name. Nothing for a path that is not a flat repo's.
pub(crate) fn relocated(bare: &Path, label: &str) -> Option<PathBuf> {
    let repos = bare.parent()?;
    if repos.file_name()? != ".repos" {
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

/// True when a worktree registered in `bare` still points at the place the
/// repo had before it moved, `.repos/<name>.git`, and that place is gone: a
/// move that stopped before its repair. Read from git's own files, without
/// running git. A pointer anywhere else is not this routine's to change: one
/// a session planted is the vetting's to refuse and report.
fn needs_repair(bare: &Path) -> bool {
    let Some(file) = bare.file_name() else {
        return false;
    };
    let Ok(entries) = fs::read_dir(bare.join("worktrees")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Ok(gitdir) = git::read_small(&entry.path().join("gitdir"), MAX_POINTER) else {
            return false;
        };
        let pointer = PathBuf::from(gitdir.trim_end_matches(['\n', '\r']));
        // A worktree that is gone is git's to prune, not ours to repair.
        let Ok(text) = git::read_small(&pointer, MAX_POINTER) else {
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
            && target.file_name() == Some(&entry.file_name())
            && target.parent().and_then(Path::file_name) == Some("worktrees".as_ref())
            && old_repo.and_then(Path::file_name) == Some(file)
            && old_repo.and_then(Path::parent).and_then(Path::file_name) == Some(".repos".as_ref())
    })
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
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        // SAFETY: flock on a descriptor this function owns; released when
        // `lock` is dropped at the end of the call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", lock_path.display()));
        }

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
        for (_, _, bare) in nested(&self.repos_dir) {
            if moved.repaired.contains(&bare) || !needs_repair(&bare) {
                continue;
            }
            match self.git.repair_worktrees(&bare) {
                Ok(()) => moved.repaired.push(bare),
                Err(error) => moved.left.push((bare, format!("{error:#}"))),
            }
        }

        for (_, session) in self.sessions() {
            let Ok(text) = fs::read_to_string(session.join(super::MANIFEST)) else {
                continue;
            };
            let Ok(mut manifest) = serde_json::from_str::<Manifest>(&text) else {
                continue;
            };
            if relocate_all(&mut manifest) {
                Self::save_manifest(&session, &manifest)?;
                moved.manifests.push(session.join(super::MANIFEST));
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
        fs::create_dir_all(&owner)
            .map_err(|error| format!("creating {}: {error}", owner.display()))?;
        fs::rename(from, &to).map_err(|error| format!("moving to {}: {error}", to.display()))?;
        self.git
            .repair_worktrees(&to)
            .map_err(|error| format!("moved to {}, but {error:#}", to.display()))?;
        Ok(to)
    }
}

/// Point every repo of a manifest that names a place no longer there at
/// where it is now. True if anything changed.
pub(crate) fn relocate_all(manifest: &mut Manifest) -> bool {
    let mut changed = false;
    for info in manifest.repos.values_mut() {
        if info.bare.exists() {
            continue;
        }
        if let Some(now) = relocated(&info.bare, &info.label) {
            info.bare = now;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::{label, owner_of, valid_owner, NO_OWNER};

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
