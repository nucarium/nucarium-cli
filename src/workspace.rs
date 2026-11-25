//! Sessions, the repositories inside them, and the rules for removing either.
//!
//! Layout under the workspace root:
//!
//! ```text
//! .repos/<owner>/<repo>.git  bare clones of GitHub repos, under their owner
//! .repos/_/<repo>.git        bare clones from anywhere else
//! .worktrees/<session>/      one folder per session
//!     .workspace.json        the session's manifest
//!     <repo>/                a linked git worktree of one bare clone
//! .seeds/<repo>/             untracked files copied into new worktrees
//! ```
//!
//! A repo is available when its clone exists. A nucleus made before owner
//! folders has `.repos/<repo>.git`; those are still read, and moved into
//! their owner folders by [`Workspace::move_to_owner_folders`].

use crate::config::Config;
use crate::{git, github, paths};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

mod layout;

pub use layout::Moved;

const MANIFEST: &str = ".workspace.json";
/// Where the worktrees a session's subagents make go, inside the session.
const SUBWORKTREES: &str = ".subworktrees";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub label: String,
    pub bare: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub branch: String,
    pub repos: BTreeMap<String, RepoInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Available {
    pub repo: String,
    pub label: String,
    pub bare: PathBuf,
}

/// What `nucleate clone` made available, or found already there.
#[derive(Debug, Clone, Serialize)]
pub struct Cloned {
    #[serde(flatten)]
    pub repo: Available,
    pub already_present: bool,
}

pub struct Workspace {
    config: Config,
    repos_dir: PathBuf,
    sessions_dir: PathBuf,
    seeds_dir: PathBuf,
    /// Where GitHub's answers to "was this commit merged?" are remembered,
    /// for a caller that asks on a schedule.
    merges: Option<PathBuf>,
    /// Folders that belong to something else, a driver that makes and
    /// removes its own worktrees. Nothing under one is removed, pruned,
    /// snapshotted or counted here. See [`Workspace::left_alone_because`]
    /// for the rule that does not depend on this list.
    protected: Vec<PathBuf>,
    /// How git is run: always guarded, in repositories vetted first. The
    /// daemon chooses its own vetting mode; `nucleate` always enforces.
    git: git::Git,
}

/// What a worktree holds that exists nowhere else. This is the one rule:
/// `nucleate` refuses to delete on it, and the daemon counts by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Unsaved {
    /// Changes that are not committed, untracked files included.
    pub uncommitted: bool,
    /// Commits on no remote: those ahead of the branch's upstream or, for a
    /// branch with none of its own, those no remote branch holds, unless
    /// this very commit is one of a merged pull request's.
    pub unpushed: u32,
    /// Whether `unpushed` was counted against an upstream.
    pub tracked: bool,
}

impl Unsaved {
    pub fn is_saved(&self) -> bool {
        !self.uncommitted && self.unpushed == 0
    }

    /// The same facts in words, for the person a removal was refused to.
    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.uncommitted {
            reasons.push("uncommitted changes".to_string());
        }
        match (self.unpushed, self.tracked) {
            (0, _) => {}
            (ahead, true) => reasons.push(format!("{ahead} unpushed commit(s)")),
            (_, false) => {
                reasons.push("HEAD is not on any remote branch or merged pull request".to_string())
            }
        }
        reasons
    }
}

/// One answer from GitHub about one commit, and when it was given: whether
/// it was merged, and the addresses of a merged pull request that holds it
/// and of an open one whose head it is.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Asked {
    merged: bool,
    at_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    merged_pull: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open_pull: Option<String>,
}

/// The pull requests a worktree's commit is in, as far as GitHub said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequests {
    /// A merged pull request that holds the commit.
    pub merged: Option<String>,
    /// An open pull request whose head is the commit.
    pub open: Option<String>,
}

/// "Not merged" is asked again after this long. "Merged" never changes.
const ASK_AGAIN_AFTER_S: u64 = 3_600;
/// An answer nobody has needed for this long is forgotten.
const FORGET_AFTER_S: u64 = 30 * 86_400;

/// Held while `merged.json` is read, changed and written back. The reporter
/// and housekeeping each ask GitHub on a thread of their own and remember
/// what they heard in the one file; without it, one's answers could be
/// written over with a copy read before them.
static MERGES: Mutex<()> = Mutex::new(());

fn read_merges(file: &Path) -> BTreeMap<String, Asked> {
    fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Add one answer to `merged.json`: read afresh, so that what another thread
/// wrote since this one asked is kept, and written back whole. Answers no one
/// has needed for a month are dropped.
fn remember_merge(file: &Path, key: String, answer: Asked) {
    let _held = MERGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now_s = answer.at_s;
    let mut asked = read_merges(file);
    asked.retain(|_, earlier| now_s.saturating_sub(earlier.at_s) < FORGET_AFTER_S);
    asked.insert(key, answer);
    if let Ok(text) = serde_json::to_string(&asked) {
        let _ = paths::write_private(file, text.as_bytes());
    }
}

fn count(text: &str) -> Result<u32> {
    text.trim()
        .parse()
        .with_context(|| format!("git printed {text:?} where a count belongs"))
}

fn valid_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn valid_branch(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        && !value.contains("..")
        && !value.ends_with('/')
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        let _ = fs::remove_file(dst);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dst)?;
        #[cfg(not(unix))]
        let _ = target;
    } else if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

impl Workspace {
    /// A workspace that runs git as `nucleate` does: guarded, with every
    /// repository's git data held to the nucleus's `.repos`, and a repository
    /// that fails the check refused, the call failing with why.
    pub fn new(config: Config) -> Self {
        let root = config.root.clone();
        let repos_dir = root.join(".repos");
        let git = git::Git::Guarded(
            git::Guarded::new(Some(repos_dir.clone()), git::Vetting::Enforce).quiet(),
        );
        Workspace {
            config,
            repos_dir,
            sessions_dir: root.join(".worktrees"),
            seeds_dir: root.join(".seeds"),
            merges: None,
            protected: Vec::new(),
            git,
        }
        .signing_in()
    }

    /// Run git the daemon's way: each repository vetted as its settings say,
    /// and refusals logged.
    pub fn guarded(mut self, git: git::Git) -> Self {
        self.git = git;
        self.signing_in()
    }

    /// Have git sign in to GitHub with the config's token file, when it
    /// names one, in place of the user's own credential helper for GitHub.
    fn signing_in(mut self) -> Self {
        if let git::Git::Guarded(guarded) = self.git {
            self.git = git::Git::Guarded(guarded.signing_in_to(
                &github::git_origin(&self.config.github_api),
                self.config.github_token_file.as_deref(),
            ));
        }
        self
    }

    pub fn git(&self) -> &git::Git {
        &self.git
    }

    /// Leave everything under these folders alone. The daemon names the
    /// folders of every driver that owns its worktrees.
    pub fn leaving_alone(mut self, folders: Vec<PathBuf>) -> Self {
        self.protected = folders;
        self
    }

    /// A path with as much of it resolved as exists. A worktree whose folder
    /// is gone still has a place: the rest is kept as written.
    fn resolved(path: &Path) -> PathBuf {
        path.ancestors()
            .find_map(|above| {
                let below = path.strip_prefix(above).ok()?;
                Some(above.canonicalize().ok()?.join(below))
            })
            .unwrap_or_else(|| path.to_path_buf())
    }

    /// True for a path under a folder this workspace was told belongs to a
    /// driver that owns its worktrees.
    pub fn under_a_driver(&self, path: &Path) -> bool {
        let resolved = Self::resolved(path);
        self.protected
            .iter()
            .any(|folder| resolved.starts_with(folder))
    }

    /// Why a worktree is not this nucleus's own to change, or nothing when
    /// it is. Ownership is decided positively: the nucleus's own worktrees
    /// are the ones under its `.worktrees/`, where `nucleate` puts every one
    /// it makes. Anything else registered in one of its bare repos was made
    /// by a driver or by hand, whether or not the config names its folder.
    pub fn left_alone_because(&self, path: &Path) -> Option<&'static str> {
        if self.under_a_driver(path) {
            return Some("owned by a driver");
        }
        let resolved = Self::resolved(path);
        let sessions = Self::resolved(&self.sessions_dir);
        (resolved == sessions || !resolved.starts_with(&sessions))
            .then_some("not under this nucleus's .worktrees")
    }

    /// True for a worktree that is not this nucleus's own: it is never
    /// removed or snapshotted, and git's record of it is never pruned.
    pub fn leaves_alone(&self, path: &Path) -> bool {
        self.left_alone_because(path).is_some()
    }

    /// True when every worktree registered in a bare repo is this nucleus's
    /// own, so that pruning its records can drop nobody else's. Where git
    /// cannot say, it might not be.
    fn holds_only_its_own(&self, bare: &Path) -> bool {
        self.worktrees(bare).is_ok_and(|registered| {
            registered
                .iter()
                .all(|worktree| !self.leaves_alone(worktree))
        })
    }

    /// Remember GitHub's answers in `file`, so that counting unsaved work on
    /// a schedule asks about each commit once an hour at most, and never
    /// again once it was merged.
    pub fn remembering_merges(mut self, file: PathBuf) -> Self {
        self.merges = Some(file);
        self
    }

    /// Ask a stand-in on this machine in place of GitHub, for its API and
    /// for the repositories git signs in to. Any other address is ignored.
    pub fn asking_github_at(mut self, url: &str) -> Self {
        if let Some(url) = github::stand_in(url) {
            self.config.github_api = url;
        }
        self.signing_in()
    }

    pub fn github_api(&self) -> &str {
        &self.config.github_api
    }

    pub fn github_token_file(&self) -> Option<&Path> {
        self.config.github_token_file.as_deref()
    }

    // ---- repositories -------------------------------------------------

    pub fn remote_url(&self, bare: &Path) -> String {
        self.git
            .try_bare(bare, &["config", "--get", "remote.origin.url"])
            .map(|o| o.stdout)
            .unwrap_or_default()
    }

    pub fn available(&self) -> Result<Vec<Available>> {
        let mut found = Vec::new();
        // In an owner folder, a repo is labelled by where it is.
        for (owner, name, path) in layout::nested(&self.repos_dir) {
            if self.listed(&path)? {
                found.push(Available {
                    label: layout::label(&owner, &name),
                    repo: name,
                    bare: path,
                });
            }
        }
        // Not moved to an owner folder yet: labelled by its remote, as before.
        for entry in fs::read_dir(&self.repos_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let file = entry.file_name().to_string_lossy().to_string();
            let Some(name) = file.strip_suffix(".git") else {
                continue;
            };
            if !self.listed(&path)? {
                continue;
            }
            let refused = self.git.enforcing() && self.git.vet_bare(&path).is_some();
            let label = match refused {
                true => None,
                false => github::slug(&self.remote_url(&path)),
            }
            .map(|(owner, repo)| format!("{owner}/{repo}"))
            .unwrap_or_else(|| name.to_string());
            found.push(Available {
                repo: name.to_string(),
                label,
                bare: path,
            });
        }
        found.sort_by(|a, b| (&a.repo, &a.label).cmp(&(&b.repo, &b.label)));
        Ok(found)
    }

    /// Whether a folder in `.repos` is listed as a repo: a bare repo is, and
    /// so is one the daemon refuses, which is still one of the nucleus's,
    /// with every call in it refused.
    fn listed(&self, path: &Path) -> Result<bool> {
        if self.git.enforcing() && self.git.vet_bare(path).is_some() {
            return Ok(true);
        }
        let is_bare = self
            .git
            .try_bare(path, &["rev-parse", "--is-bare-repository"])?;
        Ok(is_bare.ok && is_bare.stdout == "true")
    }

    /// Find a repo by its label (`octo/demo`, or `demo` for one with no
    /// owner), or by its name alone (`demo`) when only one owner has a repo
    /// of that name. Case does not matter.
    fn lookup(&self, value: &str) -> Result<Available> {
        let available = self.available()?;
        if let Some(hit) = available
            .iter()
            .find(|a| a.label.eq_ignore_ascii_case(value))
        {
            return Ok(hit.clone());
        }
        let short = value.rsplit('/').next().unwrap_or(value);
        if !valid_name(short) {
            bail!("invalid repo name: {value:?}");
        }
        let not_here = || {
            anyhow!(
                "repo not available in {}: {value}",
                self.repos_dir.display()
            )
        };
        if value.contains('/') {
            return Err(not_here());
        }
        let mut named: Vec<Available> = available
            .into_iter()
            .filter(|a| a.repo.eq_ignore_ascii_case(value))
            .collect();
        match named.len() {
            0 => Err(not_here()),
            1 => Ok(named.remove(0)),
            _ => {
                let labels: Vec<&str> = named.iter().map(|a| a.label.as_str()).collect();
                bail!(
                    "{value} is more than one repo here: {}; name the one you mean",
                    labels.join(", ")
                )
            }
        }
    }

    /// Find a repo to add. One named `owner/name` that this nucleus does not
    /// have yet is fetched from GitHub first, with the box's GitHub sign-in,
    /// into its owner's folder.
    fn find_or_fetch(&self, value: &str) -> Result<Available> {
        let Some((owner, name)) = value.split_once('/') else {
            return self.lookup(value);
        };
        if !layout::valid_owner(owner) || !valid_name(name) {
            return self.lookup(value);
        }
        if let Some(hit) = self
            .available()?
            .into_iter()
            .find(|a| a.label.eq_ignore_ascii_case(value))
        {
            return Ok(hit);
        }
        let url = format!(
            "{}/{owner}/{name}.git",
            github::git_origin(&self.config.github_api)
        );
        match self.clone_into(&url, None, Some(owner)) {
            Ok(fetched) => Ok(fetched.repo),
            // Another session fetched the same repo at the same moment.
            Err(error) => self
                .available()?
                .into_iter()
                .find(|a| a.label.eq_ignore_ascii_case(value))
                .ok_or_else(|| error.context(format!("cannot fetch {value} from GitHub"))),
        }
    }

    /// `nucleate clone`: make a repo available, in its owner's folder. The
    /// bare repo fetches into remote-tracking refs, so a fetch never rewrites
    /// a branch a session has checked out, and records the remote's default
    /// branch. A repo already here is found, not cloned again.
    pub fn clone(&self, url: &str, name: Option<&str>) -> Result<Cloned> {
        self.clone_into(url, name, None)
    }

    /// As [`Workspace::clone`], into `owner`'s folder when it is given, else
    /// that of the URL's GitHub owner, else the folder for repos with none.
    fn clone_into(&self, url: &str, name: Option<&str>, owner: Option<&str>) -> Result<Cloned> {
        let name = match name {
            Some(name) => name.to_string(),
            None => {
                // The last part of the path, never the host.
                let path = match url.split_once("://") {
                    Some((_, rest)) => rest.split_once('/').map_or("", |(_, path)| path),
                    None => url,
                };
                let path = path.trim_end_matches('/');
                let path = path.strip_suffix(".git").unwrap_or(path);
                path.rsplit(['/', ':'])
                    .next()
                    .unwrap_or_default()
                    .to_string()
            }
        };
        if !valid_name(&name) {
            bail!("cannot name a repo {name:?} from {url:?}; pass --name");
        }
        let owner = owner.map_or_else(|| layout::owner_of(url), str::to_string);
        // A path on this machine is fetched by its full name, so the clone
        // does not depend on the folder the command was run from.
        let local = Path::new(url);
        let source = match local.canonicalize() {
            Ok(path) if !url.contains("://") => path.to_string_lossy().to_string(),
            _ => url.to_string(),
        };
        let label = layout::label(&owner, &name);
        if let Some(have) = self
            .available()?
            .into_iter()
            .find(|a| a.label.eq_ignore_ascii_case(&label))
        {
            // A GitHub repo is the same repo by its owner and name. One with
            // no owner has only its name, so it must come from the same place.
            if owner != layout::NO_OWNER || self.remote_url(&have.bare) == source {
                return Ok(Cloned {
                    repo: have,
                    already_present: true,
                });
            }
            bail!("repo already available: {}", have.bare.display());
        }
        let owner_dir = layout::owner_dir(&self.repos_dir, &owner);
        let bare = owner_dir.join(format!("{name}.git"));
        if bare.exists() {
            bail!("repo already available: {}", bare.display());
        }
        let new_owner = !owner_dir.exists();
        fs::create_dir_all(&owner_dir)?;
        let made = self.git.init_bare(&bare);
        if made.is_err() && new_owner {
            let _ = fs::remove_dir(&owner_dir);
        }
        made?;
        let fetched = self
            .git
            .bare(&bare, &["remote", "add", "origin", &source])
            .and_then(|_| {
                self.git.bare(
                    &bare,
                    &[
                        "config",
                        "remote.origin.fetch",
                        "+refs/heads/*:refs/remotes/origin/*",
                    ],
                )
            })
            .and_then(|_| self.git.bare(&bare, &["fetch", "-q", "origin"]));
        if let Err(error) = fetched {
            let _ = fs::remove_dir_all(&bare);
            // Only if it is empty: another repo may be arriving in it.
            let _ = fs::remove_dir(&owner_dir);
            return Err(error);
        }
        // An empty remote has no head to record; sessions then start from "main".
        let _ = self
            .git
            .try_bare(&bare, &["remote", "set-head", "origin", "--auto"]);
        let head = format!("refs/heads/{}", self.default_branch(&bare));
        let _ = self.git.try_bare(&bare, &["symbolic-ref", "HEAD", &head]);
        let owner = owner_dir
            .file_name()
            .map_or(owner, |have| have.to_string_lossy().to_string());
        Ok(Cloned {
            repo: Available {
                label: layout::label(&owner, &name),
                repo: name,
                bare,
            },
            already_present: false,
        })
    }

    fn default_branch(&self, bare: &Path) -> String {
        let head = self
            .git
            .try_bare(
                bare,
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
            .map(|o| o.stdout)
            .unwrap_or_default();
        match head.strip_prefix("origin/") {
            Some(branch) if !branch.is_empty() => branch.to_string(),
            _ => "main".to_string(),
        }
    }

    /// Fetch every bare repo and drop branches deleted on the remote.
    pub fn refresh(&self) -> Result<Value> {
        let mut results = Vec::new();
        for item in self.available()? {
            let refused = self.git.enforcing().then(|| self.git.vet_bare(&item.bare));
            if let Some(refused) = refused.flatten() {
                results.push(json!({
                    "repo": item.repo, "ok": false,
                    "action": "refused", "reason": refused.reason.as_str(),
                }));
                continue;
            }
            let fetched = self
                .git
                .try_bare(&item.bare, &["fetch", "-q", "--prune", "origin"])?;
            let _ = self
                .git
                .try_bare(&item.bare, &["remote", "set-head", "origin", "--auto"]);
            let mut entry = json!({ "repo": item.repo, "ok": fetched.ok });
            if !fetched.ok {
                entry["error"] = json!(fetched.stderr);
            }
            results.push(entry);
        }
        Ok(Value::Array(results))
    }

    // ---- sessions -----------------------------------------------------

    /// Every session folder in the nucleus: its name and its path.
    pub fn sessions(&self) -> Vec<(String, PathBuf)> {
        let Ok(entries) = fs::read_dir(&self.sessions_dir) else {
            return Vec::new();
        };
        let mut found: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter(|entry| entry.path().join(MANIFEST).is_file())
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().to_string(),
                    entry.path(),
                )
            })
            .collect();
        found.sort();
        found
    }

    /// Drop git's records of worktrees whose folders are gone. A prune
    /// drops every such record in a repo, so a repo that also holds a
    /// worktree that is not this nucleus's own is not pruned at all.
    pub fn prune_worktrees(&self) -> Result<()> {
        for item in self.available()? {
            if self.holds_only_its_own(&item.bare) {
                let _ = self.git.try_bare(&item.bare, &["worktree", "prune"]);
            }
        }
        Ok(())
    }

    /// The checked-out worktrees of one bare repo.
    pub fn worktrees(&self, bare: &Path) -> Result<Vec<PathBuf>> {
        let listing = self.git.bare(bare, &["worktree", "list", "--porcelain"])?;
        // Entries are separated by blank lines; the bare repo itself is one of them.
        Ok(listing
            .split("\n\n")
            .filter(|entry| !entry.lines().any(|line| line == "bare"))
            .filter_map(|entry| {
                entry
                    .lines()
                    .find_map(|line| line.strip_prefix("worktree "))
            })
            .map(PathBuf::from)
            .collect())
    }

    pub fn load_manifest(session: &Path) -> Result<Manifest> {
        let path = session.join(MANIFEST);
        if !path.is_file() {
            bail!(
                "not a workspace session (no {MANIFEST}): {}",
                session.display()
            );
        }
        let text = fs::read_to_string(&path)?;
        let mut manifest: Manifest =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // A repo moved to its owner folder since this was written.
        layout::relocate_all(&mut manifest);
        Ok(manifest)
    }

    fn save_manifest(session: &Path, manifest: &Manifest) -> Result<()> {
        let tmp = session.join(format!("{MANIFEST}.tmp"));
        fs::write(&tmp, serde_json::to_string_pretty(manifest)? + "\n")?;
        fs::rename(&tmp, session.join(MANIFEST))?;
        Ok(())
    }

    fn write_agents(session: &Path, manifest: &Manifest) -> Result<()> {
        fs::write(session.join("AGENTS.md"), Self::agents_text(manifest))?;
        Ok(())
    }

    fn agents_text(manifest: &Manifest) -> String {
        let mut text = format!(
            "# Workspace session\n\nEach subdirectory is an independent git worktree on branch \
             `{}`. Work only inside them; never edit the bare repos under `../../.repos/`. Use \
             the nucleate MCP tools (`list_repos`, `add_repo`) to expand this session.\n\n",
            manifest.branch
        );
        for (name, info) in &manifest.repos {
            text.push_str(&format!(
                "- `{name}/` — {} on `{}`\n",
                info.label, info.branch
            ));
        }
        text
    }

    fn inside_sessions(&self, path: &Path) -> bool {
        path != self.sessions_dir && path.starts_with(&self.sessions_dir)
    }

    /// The session that `start` is inside, found by walking up to its manifest.
    pub fn find_session_root(&self, start: &Path) -> Result<PathBuf> {
        let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        start
            .ancestors()
            .find(|dir| dir.join(MANIFEST).is_file() && self.inside_sessions(dir))
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                anyhow!(
                    "not inside a workspace session under {}",
                    self.sessions_dir.display()
                )
            })
    }

    /// Resolve a session given by name or by path.
    pub fn session_dir(&self, value: &str) -> Result<PathBuf> {
        let given = Path::new(value);
        let candidate = if given.is_absolute() {
            given.to_path_buf()
        } else {
            let relative = std::env::current_dir()?.join(given);
            if relative.join(MANIFEST).is_file() {
                relative
            } else {
                self.sessions_dir.join(value)
            }
        };
        let resolved = candidate.canonicalize().unwrap_or(candidate);
        if !self.inside_sessions(&resolved) {
            bail!(
                "session must be under {}: {}",
                self.sessions_dir.display(),
                resolved.display()
            );
        }
        Ok(resolved)
    }

    fn copy_includes(&self, repo: &str, child: &Path) -> Result<()> {
        let include = child.join(".worktreeinclude");
        let seed_root = self.seeds_dir.join(repo);
        if !include.is_file() || !seed_root.is_dir() {
            return Ok(());
        }
        for raw in fs::read_to_string(&include)?.lines() {
            let pattern = raw.trim();
            let escapes = Path::new(pattern)
                .components()
                .any(|c| c == Component::ParentDir);
            if pattern.is_empty() || pattern.starts_with('#') || pattern.starts_with('/') || escapes
            {
                continue;
            }
            let full = format!(
                "{}/{}",
                glob::Pattern::escape(&seed_root.to_string_lossy()),
                pattern
            );
            for source in glob::glob(&full)?.flatten() {
                let relative = source.strip_prefix(&seed_root)?;
                copy_tree(&source, &child.join(relative))?;
            }
        }
        Ok(())
    }

    pub fn add_repo(&self, session: &Path, repo: &str, branch: Option<&str>) -> Result<Value> {
        let mut manifest = Self::load_manifest(session)?;
        let item = self.find_or_fetch(repo)?;
        if let Some(existing) = manifest.repos.get(&item.repo) {
            let same = existing.label.eq_ignore_ascii_case(&item.label)
                || Self::resolved(&existing.bare) == Self::resolved(&item.bare);
            if !same {
                bail!(
                    "this session already has {} from {}; {} cannot be added beside it",
                    item.repo,
                    existing.label,
                    item.label
                );
            }
            return Ok(json!({
                "repo": item.repo, "path": existing.path, "branch": existing.branch,
                "already_present": true,
            }));
        }
        let child = session.join(&item.repo);
        if child.exists() {
            bail!(
                "{} already exists but is not in the manifest",
                child.display()
            );
        }
        let branch = branch.unwrap_or(&manifest.branch).to_string();
        if !valid_branch(&branch) {
            bail!("invalid branch: {branch:?}");
        }
        let default = self.default_branch(&item.bare);
        let refspec = |b: &str| format!("+refs/heads/{b}:refs/remotes/origin/{b}");
        self.git
            .bare(&item.bare, &["fetch", "-q", "origin", &refspec(&default)])?;
        // Also fetch the requested branch, so one pushed from elsewhere is
        // resumed rather than silently recreated from the default branch.
        let _ = self
            .git
            .try_bare(&item.bare, &["fetch", "-q", "origin", &refspec(&branch)]);

        let has = |reference: String| -> Result<bool> {
            Ok(self
                .git
                .try_bare(&item.bare, &["show-ref", "--verify", "-q", &reference])?
                .ok)
        };
        let child_arg = child.to_string_lossy().to_string();
        if has(format!("refs/heads/{branch}"))? {
            self.git
                .bare(&item.bare, &["worktree", "add", &child_arg, &branch])?;
        } else if has(format!("refs/remotes/origin/{branch}"))? {
            let upstream = format!("origin/{branch}");
            self.git.bare(
                &item.bare,
                &[
                    "worktree", "add", "-b", &branch, "--track", &child_arg, &upstream,
                ],
            )?;
        } else {
            let start = format!("origin/{default}");
            self.git.bare(
                &item.bare,
                &["worktree", "add", "-b", &branch, &child_arg, &start],
            )?;
        }
        self.copy_includes(&item.repo, &child)?;

        let info = RepoInfo {
            label: item.label.clone(),
            bare: item.bare.clone(),
            path: child.clone(),
            branch,
            base: self.git.tree(&child, &["rev-parse", "HEAD"])?,
        };
        manifest.repos.insert(item.repo.clone(), info.clone());
        Self::save_manifest(session, &manifest)?;
        Self::write_agents(session, &manifest)?;
        let mut out = serde_json::to_value(&info)?;
        out["repo"] = json!(item.repo);
        out["already_present"] = json!(false);
        Ok(out)
    }

    pub fn create_session(&self, name: &str, defaults: &[String]) -> Result<PathBuf> {
        if !valid_name(name) {
            bail!("invalid session name: {name:?}");
        }
        let session = self.sessions_dir.join(name);
        if session.exists() {
            bail!("session already exists: {}", session.display());
        }
        fs::create_dir_all(&session)?;
        let manifest = Manifest {
            name: name.to_string(),
            branch: format!("{}/{name}", self.config.branch_prefix),
            repos: BTreeMap::new(),
        };
        Self::save_manifest(&session, &manifest)?;
        Self::write_agents(&session, &manifest)?;
        for repo in defaults {
            if let Err(error) = self.add_repo(&session, repo, None) {
                if let Ok(partial) = Self::load_manifest(&session) {
                    for info in partial.repos.values() {
                        let path = info.path.to_string_lossy().to_string();
                        let _ = self
                            .git
                            .try_bare(&info.bare, &["worktree", "remove", "--force", &path]);
                    }
                }
                let _ = fs::remove_dir_all(&session);
                return Err(error);
            }
        }
        Ok(session)
    }

    /// What a worktree holds that exists nowhere else.
    pub fn unsaved(&self, child: &Path) -> Result<Unsaved> {
        // The daemon asks this on a schedule, beside an agent that is using
        // the same worktree. Without the optional lock, asking never makes
        // the agent's own `git add` or `git commit` find the index locked.
        let status = self
            .git
            .tree(child, &["--no-optional-locks", "status", "--porcelain"])?;
        let uncommitted = !status.is_empty();
        let common = self.git.tree(
            child,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = Path::new(&common);
        if self.has_own_upstream(child, common)? {
            let ahead = self
                .git
                .tree(child, &["rev-list", "--count", "@{upstream}..HEAD"])?;
            return Ok(Unsaved {
                uncommitted,
                unpushed: count(&ahead)?,
                tracked: true,
            });
        }
        let mut unpushed = 0;
        if self
            .git
            .tree(child, &["branch", "-r", "--contains", "HEAD"])?
            .is_empty()
        {
            let head = self.git.tree(child, &["rev-parse", "HEAD"])?;
            if !self.merged(common, &head) {
                let alone = self.git.tree(
                    child,
                    &["rev-list", "--count", "HEAD", "--not", "--remotes"],
                )?;
                // HEAD is on no remote branch, so it is one of them at least.
                unpushed = count(&alone)?.max(1);
            }
        }
        Ok(Unsaved {
            uncommitted,
            unpushed,
            tracked: false,
        })
    }

    /// True when a worktree's branch tracks a branch of its own on a remote,
    /// one that still exists: the commits ahead of it are then the ones not
    /// pushed. The remote's default branch is not such a branch. A session's
    /// work is merged into it, often squashed, so counting against it would
    /// count finished work as unpushed for ever.
    fn has_own_upstream(&self, child: &Path, common: &Path) -> Result<bool> {
        let upstream = self
            .git
            .try_tree(child, &["rev-parse", "--symbolic-full-name", "@{upstream}"])?;
        // No upstream, or one whose remote branch is gone, fails here.
        if !upstream.ok || !upstream.stdout.starts_with("refs/remotes/") {
            return Ok(false);
        }
        let default = format!("refs/remotes/origin/{}", self.default_branch(common));
        Ok(upstream.stdout != default)
    }

    /// Reasons a worktree must not be deleted: anything that exists nowhere else.
    pub fn unsafe_reasons(&self, child: &Path) -> Result<Vec<String>> {
        Ok(self.unsaved(child)?.reasons())
    }

    fn merged(&self, bare: &Path, sha: &str) -> bool {
        self.asked(bare, sha).merged
    }

    /// The pull requests a worktree's commit is in: a merged one that holds
    /// it, and an open one whose head it is. GitHub is not asked about a
    /// commit already on the remote's default branch, which every pull
    /// request merged before it would answer for, nor about one in a repo
    /// that is not on GitHub, nor about one ahead of its branch's upstream.
    pub fn pull_requests(&self, child: &Path) -> Result<PullRequests> {
        let common = self.git.tree(
            child,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = Path::new(&common);
        let head = self.git.tree(child, &["rev-parse", "HEAD"])?;
        let default = format!("refs/remotes/origin/{}", self.default_branch(common));
        let on_default = self
            .git
            .try_tree(child, &["merge-base", "--is-ancestor", &head, &default])?
            .ok;
        if on_default || github::slug(&self.remote_url(common)).is_none() {
            return Ok(PullRequests::default());
        }
        // A commit ahead of the branch's own upstream was never pushed, so
        // GitHub knows nothing of it.
        let on_remote = !self
            .git
            .tree(child, &["branch", "-r", "--contains", "HEAD"])?
            .is_empty();
        if !on_remote && self.has_own_upstream(child, common)? {
            return Ok(PullRequests::default());
        }
        let asked = self.asked(common, &head);
        Ok(PullRequests {
            merged: asked.merged_pull.filter(|_| asked.merged),
            open: asked.open_pull,
        })
    }

    /// What GitHub said about a commit: remembered when the workspace was
    /// told where, asked again after an hour unless it was merged, and asked
    /// afresh each time otherwise.
    fn asked(&self, bare: &Path, sha: &str) -> Asked {
        let remote = self.remote_url(bare);
        let now_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let ask = || {
            let found = github::lookup(
                &self.config.github_api,
                &remote,
                sha,
                self.config.github_token_file.as_deref(),
            )
            .unwrap_or_default();
            Asked {
                merged: found.merged,
                at_s: now_s,
                merged_pull: found.merged_pull,
                open_pull: found.open_pull,
            }
        };
        let (Some(file), Some((owner, repo))) = (&self.merges, github::slug(&remote)) else {
            return ask();
        };
        let key = format!("{owner}/{repo}@{sha}");
        match read_merges(file).get(&key) {
            Some(answer) if answer.merged => return answer.clone(),
            Some(answer) if now_s.saturating_sub(answer.at_s) < ASK_AGAIN_AFTER_S => {
                return answer.clone()
            }
            _ => {}
        }
        let answer = ask();
        remember_merge(file, key, answer.clone());
        answer
    }

    fn remove_one(&self, name: &str, info: &RepoInfo, force: bool) -> Result<()> {
        if let Some(why) = self.left_alone_because(&info.path) {
            bail!("{name}: its worktree is {why}");
        }
        if info.path.exists() {
            let reasons = self.unsafe_reasons(&info.path)?;
            if !reasons.is_empty() && !force {
                bail!("{name}: {}", reasons.join("; "));
            }
            let path = info.path.to_string_lossy().to_string();
            let mut args = vec!["worktree", "remove"];
            if force {
                args.push("--force");
            }
            args.push(&path);
            self.git.bare(&info.bare, &args)?;
        }
        // Delete the local branch only when its tip is on a remote branch or was merged.
        let reference = format!("refs/heads/{}", info.branch);
        let tip = self
            .git
            .try_bare(&info.bare, &["rev-parse", "-q", "--verify", &reference])?;
        if tip.ok {
            let contains = self
                .git
                .try_bare(&info.bare, &["branch", "-r", "--contains", &tip.stdout])?;
            if !contains.stdout.is_empty() || self.merged(&info.bare, &tip.stdout) {
                let _ = self
                    .git
                    .try_bare(&info.bare, &["branch", "-D", &info.branch]);
            }
        }
        Ok(())
    }

    pub fn remove_repo(&self, session: &Path, repo: &str, force: bool) -> Result<Value> {
        let mut manifest = Self::load_manifest(session)?;
        // Only a worktree that is this session's own, as its manifest names.
        let worktrees = self.session_worktrees(session)?;
        // Named as the session has it: by its folder there, or its label.
        let (name, info) = worktrees
            .iter()
            .filter(|(name, _)| manifest.repos.contains_key(name))
            .find(|(name, info)| {
                name.eq_ignore_ascii_case(repo) || info.label.eq_ignore_ascii_case(repo)
            })
            .map(|(name, info)| (name.clone(), info.clone()))
            .ok_or_else(|| anyhow!("repo not in session: {repo}"))?;
        self.remove_one(&name, &info, force)?;
        manifest.repos.remove(&name);
        Self::save_manifest(session, &manifest)?;
        Self::write_agents(session, &manifest)?;
        Ok(json!({ "removed": name, "session": session }))
    }

    /// Every worktree in a session folder: the repos its manifest lists, and
    /// those its subagents made under `.subworktrees/`, which no manifest
    /// lists. Each comes with a name to say it by.
    ///
    /// The session can write its manifest and `.subworktrees`, so neither is
    /// taken at its word. A worktree is taken for one of this session's only
    /// when it is a folder right here, not a link: a manifest's repo directly
    /// in the session, a subagent's directly in `.subworktrees`. Git's
    /// pointer in it must lead to one of this nucleus's bare repos (for a
    /// manifest's repo, the one the manifest names), and that repo must have
    /// it registered under this very path. Nothing is run in it before then.
    /// Anything else is an error, and keeps the session: what it holds is
    /// nowhere else, or it is not this session's to touch. A manifest's repo
    /// whose folder is gone is listed as it is; a link among the subagents'
    /// is not listed, and removing the session removes only the link.
    pub fn session_worktrees(&self, session: &Path) -> Result<Vec<(String, RepoInfo)>> {
        let manifest = Self::load_manifest(session)?;
        let session_real = session.canonicalize()?;
        let repos: Vec<(PathBuf, Available)> = self
            .available()?
            .into_iter()
            .filter_map(|item| Some((item.bare.canonicalize().ok()?, item)))
            .collect();
        let mut found: Vec<(String, RepoInfo)> = Vec::new();
        for (name, info) in manifest.repos {
            let here = info
                .path
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .is_some_and(|parent| parent == session_real);
            if !here || info.path.file_name().is_none() {
                bail!("{name}: its worktree is not in the session");
            }
            match fs::symlink_metadata(&info.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    bail!("{name}: its worktree is not a folder")
                }
                Ok(_) => {
                    let (bare, _) = self.registered_here(&name, &info.path, &repos)?;
                    let named = info.bare.canonicalize().ok();
                    if named.as_ref() != Some(bare) {
                        bail!("{name}: its worktree is not of the repo the manifest names");
                    }
                }
            }
            found.push((name, info));
        }
        let subs_dir = session.join(SUBWORKTREES);
        // A link in its place leads somewhere else, and holds none of them.
        if fs::symlink_metadata(&subs_dir).map_or(true, |meta| !meta.is_dir()) {
            return Ok(found);
        }
        let Ok(entries) = fs::read_dir(&subs_dir) else {
            return Ok(found);
        };
        let mut subs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        subs.sort();
        for path in subs {
            let name = format!(
                "{SUBWORKTREES}/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if !meta.is_dir() {
                bail!("{name} is not a git worktree");
            }
            if fs::symlink_metadata(path.join(".git")).is_err() {
                let empty = fs::read_dir(&path).is_ok_and(|mut inside| inside.next().is_none());
                if empty {
                    continue;
                }
                bail!("{name} is not a git worktree");
            }
            let (_, item) = self.registered_here(&name, &path, &repos)?;
            let branch = self.git.tree(&path, &["branch", "--show-current"])?;
            found.push((
                name,
                RepoInfo {
                    label: item.label.clone(),
                    bare: item.bare.clone(),
                    path,
                    branch,
                    base: String::new(),
                },
            ));
        }
        Ok(found)
    }

    /// The repo of this nucleus that `path`, a folder, is a worktree of: its
    /// `.git` file points there, and the repo has it registered at exactly
    /// this path. An error otherwise.
    fn registered_here<'a>(
        &self,
        name: &str,
        path: &Path,
        repos: &'a [(PathBuf, Available)],
    ) -> Result<(&'a PathBuf, &'a Available)> {
        let real = path.canonicalize()?;
        let Some((bare, item)) = self.pointed_at(&path.join(".git"), repos) else {
            // Said as the vetting says it, where it would refuse the worktree.
            if let Some(refused) = self
                .git
                .enforcing()
                .then(|| self.git.vet_tree(path))
                .flatten()
            {
                return Err(refused.into());
            }
            bail!("{name} is not a worktree of this nucleus's repos");
        };
        let registered = self.worktrees(bare)?.iter().any(|worktree| {
            worktree
                .canonicalize()
                .is_ok_and(|worktree| worktree == real)
        });
        if !registered {
            bail!("{name} is not a worktree its repo has registered");
        }
        Ok((bare, item))
    }

    /// The entries at a session folder's root that nucleate did not put
    /// there: anything but its manifest, the `AGENTS.md` made from it
    /// unchanged, `.subworktrees`, and the folders of the repos it lists.
    /// What is in them is nowhere else, so a session that holds any is not
    /// saved. `worktrees` is the session's, as [`Workspace::session_worktrees`]
    /// gave them.
    pub fn other_files(session: &Path, worktrees: &[(String, RepoInfo)]) -> Vec<String> {
        let manifest = Self::load_manifest(session).ok();
        let ours: Vec<&std::ffi::OsStr> = worktrees
            .iter()
            .filter(|(name, _)| !name.starts_with(SUBWORKTREES))
            .filter_map(|(_, info)| info.path.file_name())
            .collect();
        let mut other: Vec<String> = fs::read_dir(session)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let kind = entry.file_type().ok();
                let file = kind.is_some_and(|kind| kind.is_file());
                let dir = kind.is_some_and(|kind| kind.is_dir());
                let known = match name.to_str() {
                    Some(MANIFEST) => file,
                    Some("AGENTS.md") => {
                        file && manifest.as_ref().is_some_and(|manifest| {
                            fs::read_to_string(entry.path())
                                .is_ok_and(|text| text == Self::agents_text(manifest))
                        })
                    }
                    Some(SUBWORKTREES) => dir,
                    _ => dir && ours.contains(&name.as_os_str()),
                };
                !known
            })
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        other.sort();
        other
    }

    /// The repo of this nucleus whose linked worktree a `.git` file names,
    /// read without running git: `gitdir: <bare>/worktrees/<id>`. Nothing
    /// for a folder, a file that says anything else, or another repo.
    fn pointed_at<'a>(
        &self,
        pointer: &Path,
        repos: &'a [(PathBuf, Available)],
    ) -> Option<(&'a PathBuf, &'a Available)> {
        if !fs::symlink_metadata(pointer).ok()?.is_file() {
            return None;
        }
        let text = git::read_small(pointer, 4096).ok()?;
        let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        let target = PathBuf::from(target).canonicalize().ok()?;
        let worktrees = target.parent()?;
        if worktrees.file_name()? != "worktrees" {
            return None;
        }
        let bare = worktrees.parent()?;
        repos
            .iter()
            .find(|(real, _)| real == bare)
            .map(|(real, item)| (real, item))
    }

    /// Remove a whole session, or nothing at all if any worktree in it, its
    /// subagents' included, holds unsaved work.
    pub fn remove_session(&self, session: &Path) -> Result<Value> {
        self.remove_session_with(session, false)
    }

    /// Remove a whole session. With `force`, unsaved work in its worktrees
    /// goes too; without, any worktree that holds some keeps all of it.
    /// Either way, a file at its root that nucleate did not put there, or one
    /// worktree that is not this nucleus's own, keeps the whole session.
    pub fn remove_session_with(&self, session: &Path, force: bool) -> Result<Value> {
        let worktrees = self.session_worktrees(session)?;
        self.remove_these(session, &worktrees, force)
    }

    /// As [`Workspace::remove_session_with`], for a session that must still
    /// hold exactly the worktrees `expected` lists, the ones a caller has
    /// looked at and backed up. If it holds any other, or lacks one, nothing
    /// is removed.
    pub fn remove_session_as_listed(
        &self,
        session: &Path,
        expected: &[(String, RepoInfo)],
        force: bool,
    ) -> Result<Value> {
        let worktrees = self.session_worktrees(session)?;
        let paths = |list: &[(String, RepoInfo)]| -> Vec<PathBuf> {
            list.iter().map(|(_, info)| info.path.clone()).collect()
        };
        if paths(&worktrees) != paths(expected) {
            bail!("refusing to remove session: its worktrees changed");
        }
        self.remove_these(session, &worktrees, force)
    }

    fn remove_these(
        &self,
        session: &Path,
        worktrees: &[(String, RepoInfo)],
        force: bool,
    ) -> Result<Value> {
        let mut problems = Vec::new();
        let other = Self::other_files(session, worktrees);
        if !other.is_empty() {
            problems.push(format!("it holds other files: {}", other.join(", ")));
        }
        for (name, info) in worktrees {
            // All of the session or none of it: one worktree that is not
            // ours to remove keeps the whole session.
            if let Some(why) = self.left_alone_because(&info.path) {
                problems.push(format!("{name}: its worktree is {why}"));
            } else if info.path.exists() && !force {
                let reasons = self.unsafe_reasons(&info.path)?;
                if !reasons.is_empty() {
                    problems.push(format!("{name}: {}", reasons.join("; ")));
                }
            }
        }
        if !problems.is_empty() {
            bail!(
                "refusing to remove session with unsaved work: {}",
                problems.join(" | ")
            );
        }
        // A subagent's worktree first: it may be in a repo the session's own
        // worktree of it is not.
        for (name, info) in worktrees.iter().rev() {
            self.remove_one(name, info, force)?;
        }
        for (_, info) in worktrees {
            if self.holds_only_its_own(&info.bare) {
                let _ = self.git.try_bare(&info.bare, &["worktree", "prune"]);
            }
        }
        fs::remove_dir_all(session)?;
        Ok(json!({ "removed_session": session }))
    }

    /// True when a worktree's `HEAD` is on the remote's default branch: it
    /// holds no commit of its own.
    pub fn on_default_branch(&self, child: &Path) -> Result<bool> {
        let common = self.git.tree(
            child,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let default = format!(
            "refs/remotes/origin/{}",
            self.default_branch(Path::new(&common))
        );
        Ok(self
            .git
            .try_tree(child, &["merge-base", "--is-ancestor", "HEAD", &default])?
            .ok)
    }

    /// True when a worktree's `HEAD` holds commits of its own, and they are
    /// one of a merged pull request's.
    pub fn head_merged(&self, child: &Path) -> Result<bool> {
        if self.on_default_branch(child)? {
            return Ok(false);
        }
        let common = self.git.tree(
            child,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let head = self.git.tree(child, &["rev-parse", "HEAD"])?;
        Ok(self.merged(Path::new(&common), &head))
    }

    pub fn status(&self, session: &Path) -> Result<Value> {
        let manifest = Self::load_manifest(session)?;
        let mut repos = serde_json::Map::new();
        for (name, info) in &manifest.repos {
            let mut entry = serde_json::to_value(info)?;
            if info.path.exists() {
                entry["dirty"] = json!(!self
                    .git
                    .tree(&info.path, &["status", "--porcelain"])?
                    .is_empty());
                entry["head"] = json!(self
                    .git
                    .tree(&info.path, &["rev-parse", "--short", "HEAD"])?);
                entry["unsafe_reasons"] = json!(self.unsafe_reasons(&info.path)?);
            } else {
                entry["missing"] = json!(true);
            }
            repos.insert(name.clone(), entry);
        }
        Ok(json!({
            "session": manifest.name, "path": session, "branch": manifest.branch,
            "available": self.available()?, "repos": repos,
        }))
    }

    // ---- Claude Code worktree hooks -----------------------------------

    fn read_payload() -> Result<Value> {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        serde_json::from_str(&text).context("parsing the hook payload on stdin")
    }

    /// The worktree a folder inside a session is in: the nearest folder, up
    /// to the session's own, with a `.git` of its own. Found here rather than
    /// by asking git, which would be run in a folder no check has looked at.
    fn worktree_top(start: &Path, session: &Path) -> Result<PathBuf> {
        start
            .ancestors()
            .take_while(|dir| *dir != session)
            .find(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("not inside a repo of the session: {}", start.display()))
    }

    /// `WorktreeCreate`: make the folder and return its path (the only thing printed).
    pub fn hook_create(&self, defaults: &[String]) -> Result<PathBuf> {
        let payload = Self::read_payload()?;
        let name = payload["name"]
            .as_str()
            .ok_or_else(|| anyhow!("hook payload has no name"))?;
        let cwd = match payload["cwd"].as_str() {
            Some(cwd) => PathBuf::from(cwd),
            None => std::env::current_dir()?,
        };
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        if self.inside_sessions(&cwd) {
            // A worktree requested from inside a session: a sibling checkout
            // of that repo, at the commit checked out where it was asked for.
            let root = self.find_session_root(&cwd)?;
            let top = Self::worktree_top(&cwd, &root)?;
            let leaf = top
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let common = self.git.tree(
                &top,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?;
            // Not the bare repo's `HEAD`: in a repo `nucleate clone` made it
            // names a branch that does not exist, and in any other a branch
            // the session is not on.
            let start = self.git.tree(&top, &["rev-parse", "HEAD"])?;
            let target = root.join(SUBWORKTREES).join(format!("{name}-{leaf}"));
            fs::create_dir_all(target.parent().unwrap_or(&root))?;
            let branch = format!("{}/{name}", self.config.branch_prefix);
            let target_arg = target.to_string_lossy().to_string();
            self.git.bare(
                Path::new(&common),
                &["worktree", "add", "-b", &branch, &target_arg, &start],
            )?;
            return Ok(target);
        }
        self.create_session(name, defaults)
    }

    /// `WorktreeRemove`: remove the folder, or fail and leave it if it holds unsaved work.
    pub fn hook_remove(&self) -> Result<()> {
        let payload = Self::read_payload()?;
        let path = payload["worktree_path"]
            .as_str()
            .ok_or_else(|| anyhow!("hook payload has no worktree_path"))?;
        let path = Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(path));
        // Every worktree this hook is asked to remove is one `hook-create`
        // made, under `.worktrees/`. Any other is somebody else's.
        if let Some(why) = self.left_alone_because(&path) {
            bail!("refusing to remove {}: {why}", path.display());
        }
        if path.join(MANIFEST).is_file() {
            self.remove_session(&path)?;
            return Ok(());
        }
        let reasons = self.unsafe_reasons(&path)?;
        if !reasons.is_empty() {
            bail!(
                "refusing to remove {}: {}",
                path.display(),
                reasons.join("; ")
            );
        }
        let common = self.git.tree(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let path_arg = path.to_string_lossy().to_string();
        self.git
            .bare(Path::new(&common), &["worktree", "remove", &path_arg])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{read_merges, remember_merge, valid_branch, valid_name, Asked, Unsaved};

    #[test]
    fn answers_remembered_at_once_by_several_threads_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("merged.json");
        let at_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let file = &file;
                scope.spawn(move || {
                    for n in 0..25 {
                        let answer = Asked {
                            merged: n % 2 == 0,
                            at_s,
                            ..Asked::default()
                        };
                        remember_merge(file, format!("octo/web@{thread}-{n}"), answer);
                    }
                });
            }
        });
        let kept = read_merges(&file);
        assert_eq!(kept.len(), 8 * 25);
        assert!(kept["octo/web@3-4"].merged && !kept["octo/web@3-5"].merged);
    }

    #[test]
    fn unsaved_work_is_put_in_the_words_a_refusal_uses() {
        let unsaved = |uncommitted, unpushed, tracked| Unsaved {
            uncommitted,
            unpushed,
            tracked,
        };
        assert!(unsaved(false, 0, true).is_saved());
        assert!(unsaved(false, 0, false).reasons().is_empty());
        assert_eq!(
            unsaved(true, 2, true).reasons(),
            ["uncommitted changes", "2 unpushed commit(s)"]
        );
        assert_eq!(
            unsaved(false, 3, false).reasons(),
            ["HEAD is not on any remote branch or merged pull request"]
        );
        assert!(!unsaved(true, 0, false).is_saved());
    }

    #[test]
    fn names() {
        assert!(valid_name("api-2.0_x"));
        assert!(!valid_name(""));
        assert!(!valid_name("-leading"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("../escape"));
    }

    #[test]
    fn branches() {
        assert!(valid_branch("workspace/fix-1"));
        assert!(!valid_branch("a..b"));
        assert!(!valid_branch("trailing/"));
        assert!(!valid_branch("/leading"));
        assert!(!valid_branch("has space"));
    }
}
