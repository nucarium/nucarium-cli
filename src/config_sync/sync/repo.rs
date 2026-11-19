//! What git is asked, all of it.
//!
//! One bare clone per box, in the cache folder. Nothing is ever checked out
//! of it: files are read straight from a commit's tree, so no hook, filter
//! or attribute of the repository runs on the box. Every call has a time
//! limit, takes nothing on standard input and may not prompt.
//!
//! What git says when it fails is dropped here. It can quote the
//! repository's address, a signer's name or a path, so it is not logged, not
//! kept and not shown, not even to the person running `nucleated config sync`
//! at a terminal: a caller learns only that the call did not succeed, and of
//! a fetch which of three fixed ways it failed. An owner who wants git's own
//! words runs the same fetch with git.

use crate::bounded::{self, Failure};
use crate::envelope::valid_commit_id;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// For a call that reaches the repository's host.
const NETWORK: Duration = Duration::from_secs(60);
/// For a call that reads the clone on this box.
const LOCAL: Duration = Duration::from_secs(10);
/// The most a listing of a tree may run to.
const MAX_LISTING: u64 = 8 * 1024 * 1024;

/// One line of a commit's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Git's mode for the entry: `100644`, `100755`, `120000` for a
    /// symbolic link, `160000` for a submodule.
    pub mode: String,
    pub id: String,
    /// A file's size. Nothing for what is not a file.
    pub size: Option<u64>,
    pub path: String,
}

impl Entry {
    /// A plain file, the only kind a sync writes.
    pub fn is_file(&self) -> bool {
        matches!(self.mode.as_str(), "100644" | "100755") && self.size.is_some()
    }

    pub fn executable(&self) -> bool {
        self.mode == "100755"
    }
}

/// Why a fetch did not bring the pinned branch up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchFailure {
    /// There is no git on the box to fetch with.
    NoGit,
    /// The fetch ran over its time and was stopped.
    Late,
    /// Git ran and said no: the repository is not there, will not let this
    /// box in, or has no such branch.
    Refused,
}

pub struct Repo {
    dir: PathBuf,
}

impl Repo {
    pub fn at(dir: PathBuf) -> Repo {
        Repo { dir }
    }

    pub fn exists(&self) -> bool {
        self.dir.join("HEAD").is_file()
    }

    /// Forget the clone, when the repository it was made from is no longer
    /// the one the config names.
    pub fn forget(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }

    fn git(&self) -> Command {
        let mut command = Command::new("git");
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        command
    }

    /// A call that reads the clone and nothing else. The agent user's own
    /// git config and the system's have no say in it.
    fn local(&self) -> Command {
        let mut command = self.git();
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_COUNT")
            .arg("--git-dir")
            .arg(&self.dir);
        command
    }

    fn succeeded(command: &mut Command, limit: Duration) -> bool {
        bounded::output(command, limit, MAX_LISTING).is_some_and(|out| out.ok)
    }

    /// A call that reaches the repository's host, and which way it failed.
    fn reached(command: &mut Command) -> Result<(), FetchFailure> {
        match bounded::run(command, NETWORK, MAX_LISTING) {
            Ok(out) if out.ok => Ok(()),
            Err(Failure::Absent) => Err(FetchFailure::NoGit),
            Err(Failure::Late) => Err(FetchFailure::Late),
            Ok(_) | Err(_) => Err(FetchFailure::Refused),
        }
    }

    /// Bring the one pinned branch up to date, cloning first when there is
    /// no clone yet. It uses whatever access to the repository the agent
    /// user's git already has, and adds none.
    pub fn fetch(&self, repository: &str, branch: &str) -> Result<(), FetchFailure> {
        if self.exists() {
            let spec = format!("+refs/heads/{branch}:refs/heads/{branch}");
            return Self::reached(
                self.git()
                    .arg("--git-dir")
                    .arg(&self.dir)
                    .args(["fetch", "--quiet", "--no-tags", "--"])
                    .arg(repository)
                    .arg(spec),
            );
        }
        let parent = self.dir.parent().ok_or(FetchFailure::Refused)?;
        crate::paths::create_private_dir(parent).map_err(|_| FetchFailure::Refused)?;
        // Cloned beside its place and moved in whole, so a clone that was
        // cut short is never taken for one.
        let fresh = parent.join("repo.git.new");
        let _ = fs::remove_dir_all(&fresh);
        let cloned = Self::reached(
            self.git()
                .args(["clone", "--quiet", "--bare", "--no-tags", "--single-branch"])
                .arg("--template=")
                .args(["--branch", branch, "--"])
                .arg(repository)
                .arg(&fresh),
        )
        .and_then(|()| fs::rename(&fresh, &self.dir).map_err(|_| FetchFailure::Refused));
        if cloned.is_err() {
            let _ = fs::remove_dir_all(&fresh);
        }
        cloned
    }

    /// The commit the pinned branch is at in the clone.
    pub fn tip(&self, branch: &str) -> Option<String> {
        let out = bounded::output(
            self.local()
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(format!("refs/heads/{branch}^{{commit}}")),
            LOCAL,
            4096,
        )?;
        let id = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.ok && valid_commit_id(&id)).then_some(id)
    }

    /// Whether a commit carries an SSH signature made by a key the owner's
    /// allowed-signers file names. Only that file decides: git's other
    /// signature formats are switched off for the call, because a key that
    /// merely sits in a keyring on the box would pass them.
    pub fn verified(&self, commit: &str, allowed_signers: &Path) -> bool {
        let mut signers = std::ffi::OsString::from("gpg.ssh.allowedSignersFile=");
        signers.push(allowed_signers);
        Self::succeeded(
            self.local()
                .arg("-c")
                .arg(signers)
                .args(["-c", "gpg.program=false"])
                .args(["-c", "gpg.openpgp.program=false"])
                .args(["-c", "gpg.x509.program=false"])
                .args(["-c", "gpg.minTrustLevel=fully"])
                .args(["verify-commit", commit]),
            LOCAL,
        )
    }

    /// Whether `newer` has `older` in its history.
    pub fn descends(&self, newer: &str, older: &str) -> bool {
        Self::succeeded(
            self.local()
                .args(["merge-base", "--is-ancestor", older, newer]),
            LOCAL,
        )
    }

    /// The entries of a commit's tree at or under the paths named, and no
    /// others. Nothing when git could not say, or said too much.
    pub fn tree(&self, commit: &str, paths: &[&str]) -> Option<Vec<Entry>> {
        let out = bounded::output(
            self.local()
                .args(["ls-tree", "-r", "-z", "-l", commit, "--"])
                .args(paths),
            LOCAL,
            MAX_LISTING,
        )?;
        if !out.ok {
            return None;
        }
        let mut entries = Vec::new();
        for line in out.stdout.split(|byte| *byte == 0) {
            if line.is_empty() {
                continue;
            }
            // A path that is not text is not one a sync writes; it is kept
            // as an entry with no usable name so the caller can refuse it.
            let line = String::from_utf8_lossy(line);
            let (about, path) = line.split_once('\t')?;
            let mut about = about.split_whitespace();
            let mode = about.next()?.to_string();
            let _kind = about.next()?;
            let id = about.next()?.to_string();
            let size = about.next().and_then(|size| size.parse().ok());
            entries.push(Entry {
                mode,
                id,
                size,
                path: path.to_string(),
            });
        }
        Some(entries)
    }

    /// One file's bytes, by its id, when it is no bigger than `max`.
    pub fn blob(&self, id: &str, max: u64) -> Option<Vec<u8>> {
        if !valid_commit_id(id) {
            return None;
        }
        let out = bounded::output(self.local().args(["cat-file", "blob", id]), LOCAL, max)?;
        out.ok.then_some(out.stdout)
    }

    /// The ids git would give the files at these paths as they are on disk
    /// now, in order. Nothing when git could not say.
    pub fn ids_of(&self, files: &[PathBuf]) -> Option<Vec<String>> {
        let mut ids = Vec::with_capacity(files.len());
        for some in files.chunks(200) {
            let out = bounded::output(
                self.local()
                    .args(["hash-object", "--no-filters", "--"])
                    .args(some),
                LOCAL,
                64 * 1024,
            )?;
            if !out.ok {
                return None;
            }
            let text = String::from_utf8(out.stdout).ok()?;
            ids.extend(text.lines().map(|line| line.trim().to_string()));
        }
        (ids.len() == files.len()).then_some(ids)
    }
}
