//! Sessions, the repositories inside them, and the rules for removing either.
//!
//! Layout under the workspace root:
//!
//! ```text
//! .repos/<repo>.git          bare clones; a repo is available when its clone exists
//! .worktrees/<session>/      one folder per session
//!     .workspace.json        the session's manifest
//!     <repo>/                a linked git worktree of .repos/<repo>.git
//! .seeds/<repo>/             untracked files copied into new worktrees
//! ```

use crate::config::Config;
use crate::{git, github, paths};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST: &str = ".workspace.json";

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

pub struct Workspace {
    config: Config,
    repos_dir: PathBuf,
    sessions_dir: PathBuf,
    seeds_dir: PathBuf,
    /// Where GitHub's answers to "was this commit merged?" are remembered,
    /// for a caller that asks on a schedule.
    merges: Option<PathBuf>,
}

/// What a worktree holds that exists nowhere else. This is the one rule:
/// `nucleate` refuses to delete on it, and the daemon counts by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Unsaved {
    /// Changes that are not committed, untracked files included.
    pub uncommitted: bool,
    /// Commits on no remote: those ahead of the branch's upstream or, for a
    /// branch with none, those no remote branch holds, unless a merged pull
    /// request's head is this very commit.
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

/// One answer from GitHub about one commit, and when it was given.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Asked {
    merged: bool,
    at_s: u64,
}

/// "Not merged" is asked again after this long. "Merged" never changes.
const ASK_AGAIN_AFTER_S: u64 = 3_600;
/// An answer nobody has needed for this long is forgotten.
const FORGET_AFTER_S: u64 = 30 * 86_400;

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
    pub fn new(config: Config) -> Self {
        let root = config.root.clone();
        Workspace {
            config,
            repos_dir: root.join(".repos"),
            sessions_dir: root.join(".worktrees"),
            seeds_dir: root.join(".seeds"),
            merges: None,
        }
    }

    /// Remember GitHub's answers in `file`, so that counting unsaved work on
    /// a schedule asks about each commit once an hour at most, and never
    /// again once it was merged.
    pub fn remembering_merges(mut self, file: PathBuf) -> Self {
        self.merges = Some(file);
        self
    }

    /// Ask a stand-in on this machine in place of GitHub. Any other address
    /// is ignored.
    pub fn asking_github_at(mut self, url: &str) -> Self {
        if let Some(url) = github::stand_in(url) {
            self.config.github_api = url;
        }
        self
    }

    pub fn github_api(&self) -> &str {
        &self.config.github_api
    }

    pub fn github_token_file(&self) -> Option<&Path> {
        self.config.github_token_file.as_deref()
    }

    // ---- repositories -------------------------------------------------

    pub fn remote_url(bare: &Path) -> String {
        git::try_bare(bare, &["config", "--get", "remote.origin.url"])
            .map(|o| o.stdout)
            .unwrap_or_default()
    }

    pub fn available(&self) -> Result<Vec<Available>> {
        let mut found = Vec::new();
        let Ok(entries) = fs::read_dir(&self.repos_dir) else {
            return Ok(found);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file = entry.file_name().to_string_lossy().to_string();
            let Some(name) = file.strip_suffix(".git") else {
                continue;
            };
            let is_bare = git::try_bare(&path, &["rev-parse", "--is-bare-repository"])?;
            if !is_bare.ok || is_bare.stdout != "true" {
                continue;
            }
            let label = github::slug(&Self::remote_url(&path))
                .map(|(owner, repo)| format!("{owner}/{repo}"))
                .unwrap_or_else(|| name.to_string());
            found.push(Available {
                repo: name.to_string(),
                label,
                bare: path,
            });
        }
        found.sort_by(|a, b| a.repo.cmp(&b.repo));
        Ok(found)
    }

    /// Find a repo by its short name (`demo`) or its label (`octo/demo`).
    fn lookup(&self, value: &str) -> Result<Available> {
        let available = self.available()?;
        if let Some(hit) = available
            .iter()
            .find(|a| a.label == value || a.repo == value)
        {
            return Ok(hit.clone());
        }
        let short = value.rsplit('/').next().unwrap_or(value);
        if !valid_name(short) {
            bail!("invalid repo name: {value:?}");
        }
        available
            .into_iter()
            .find(|a| a.repo == short)
            .ok_or_else(|| {
                anyhow!(
                    "repo not available in {}: {value}",
                    self.repos_dir.display()
                )
            })
    }

    /// `nucleate clone`: make a repo available. The bare repo fetches into
    /// remote-tracking refs, so a fetch never rewrites a branch a session has
    /// checked out, and records the remote's default branch.
    pub fn clone(&self, url: &str, name: Option<&str>) -> Result<Available> {
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
        let bare = self.repos_dir.join(format!("{name}.git"));
        if bare.exists() {
            bail!("repo already available: {}", bare.display());
        }
        // A path on this machine is fetched by its full name, so the clone
        // does not depend on the folder the command was run from.
        let local = Path::new(url);
        let source = match local.canonicalize() {
            Ok(path) if !url.contains("://") => path.to_string_lossy().to_string(),
            _ => url.to_string(),
        };
        fs::create_dir_all(&self.repos_dir)?;
        let bare_arg = bare.to_string_lossy().to_string();
        git::git(["init", "-q", "--bare", &bare_arg])?;
        let fetched = git::bare(&bare, &["remote", "add", "origin", &source])
            .and_then(|_| {
                git::bare(
                    &bare,
                    &[
                        "config",
                        "remote.origin.fetch",
                        "+refs/heads/*:refs/remotes/origin/*",
                    ],
                )
            })
            .and_then(|_| git::bare(&bare, &["fetch", "-q", "origin"]));
        if let Err(error) = fetched {
            let _ = fs::remove_dir_all(&bare);
            return Err(error);
        }
        // An empty remote has no head to record; sessions then start from "main".
        let _ = git::try_bare(&bare, &["remote", "set-head", "origin", "--auto"]);
        let head = format!("refs/heads/{}", Self::default_branch(&bare));
        let _ = git::try_bare(&bare, &["symbolic-ref", "HEAD", &head]);
        let label = github::slug(&Self::remote_url(&bare))
            .map(|(owner, repo)| format!("{owner}/{repo}"))
            .unwrap_or_else(|| name.clone());
        Ok(Available {
            repo: name,
            label,
            bare,
        })
    }

    fn default_branch(bare: &Path) -> String {
        let head = git::try_bare(
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
            let fetched = git::try_bare(&item.bare, &["fetch", "-q", "--prune", "origin"])?;
            let _ = git::try_bare(&item.bare, &["remote", "set-head", "origin", "--auto"]);
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

    /// Drop git's records of worktrees whose folders are gone.
    pub fn prune_worktrees(&self) -> Result<()> {
        for item in self.available()? {
            let _ = git::try_bare(&item.bare, &["worktree", "prune"]);
        }
        Ok(())
    }

    /// The checked-out worktrees of one bare repo.
    pub fn worktrees(bare: &Path) -> Result<Vec<PathBuf>> {
        let listing = git::bare(bare, &["worktree", "list", "--porcelain"])?;
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
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn save_manifest(session: &Path, manifest: &Manifest) -> Result<()> {
        let tmp = session.join(format!("{MANIFEST}.tmp"));
        fs::write(&tmp, serde_json::to_string_pretty(manifest)? + "\n")?;
        fs::rename(&tmp, session.join(MANIFEST))?;
        Ok(())
    }

    fn write_agents(session: &Path, manifest: &Manifest) -> Result<()> {
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
        fs::write(session.join("AGENTS.md"), text)?;
        Ok(())
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
        let item = self.lookup(repo)?;
        if let Some(existing) = manifest.repos.get(&item.repo) {
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
        let default = Self::default_branch(&item.bare);
        let refspec = |b: &str| format!("+refs/heads/{b}:refs/remotes/origin/{b}");
        git::bare(&item.bare, &["fetch", "-q", "origin", &refspec(&default)])?;
        // Also fetch the requested branch, so one pushed from elsewhere is
        // resumed rather than silently recreated from the default branch.
        let _ = git::try_bare(&item.bare, &["fetch", "-q", "origin", &refspec(&branch)]);

        let has = |reference: String| -> Result<bool> {
            Ok(git::try_bare(&item.bare, &["show-ref", "--verify", "-q", &reference])?.ok)
        };
        let child_arg = child.to_string_lossy().to_string();
        if has(format!("refs/heads/{branch}"))? {
            git::bare(&item.bare, &["worktree", "add", &child_arg, &branch])?;
        } else if has(format!("refs/remotes/origin/{branch}"))? {
            let upstream = format!("origin/{branch}");
            git::bare(
                &item.bare,
                &[
                    "worktree", "add", "-b", &branch, "--track", &child_arg, &upstream,
                ],
            )?;
        } else {
            let start = format!("origin/{default}");
            git::bare(
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
            base: git::tree(&child, &["rev-parse", "HEAD"])?,
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
                        let _ =
                            git::try_bare(&info.bare, &["worktree", "remove", "--force", &path]);
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
        let status = git::tree(child, &["--no-optional-locks", "status", "--porcelain"])?;
        let uncommitted = !status.is_empty();
        let upstream = git::try_tree(
            child,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
        )?;
        if upstream.ok && !upstream.stdout.is_empty() {
            let ahead = git::tree(child, &["rev-list", "--count", "@{upstream}..HEAD"])?;
            return Ok(Unsaved {
                uncommitted,
                unpushed: count(&ahead)?,
                tracked: true,
            });
        }
        let mut unpushed = 0;
        if git::tree(child, &["branch", "-r", "--contains", "HEAD"])?.is_empty() {
            let common = git::tree(
                child,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?;
            let head = git::tree(child, &["rev-parse", "HEAD"])?;
            if !self.merged(Path::new(&common), &head) {
                let alone = git::tree(
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

    /// Reasons a worktree must not be deleted: anything that exists nowhere else.
    pub fn unsafe_reasons(&self, child: &Path) -> Result<Vec<String>> {
        Ok(self.unsaved(child)?.reasons())
    }

    fn merged(&self, bare: &Path, sha: &str) -> bool {
        let remote = Self::remote_url(bare);
        let ask = || {
            github::merged(
                &self.config.github_api,
                &remote,
                sha,
                self.config.github_token_file.as_deref(),
            )
        };
        let (Some(file), Some((owner, repo))) = (&self.merges, github::slug(&remote)) else {
            return ask();
        };
        let key = format!("{owner}/{repo}@{sha}");
        let now_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut asked: BTreeMap<String, Asked> = fs::read_to_string(file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        match asked.get(&key) {
            Some(answer) if answer.merged => return true,
            Some(answer) if now_s.saturating_sub(answer.at_s) < ASK_AGAIN_AFTER_S => return false,
            _ => {}
        }
        let merged = ask();
        asked.retain(|_, answer| now_s.saturating_sub(answer.at_s) < FORGET_AFTER_S);
        asked.insert(
            key,
            Asked {
                merged,
                at_s: now_s,
            },
        );
        if let Ok(text) = serde_json::to_string(&asked) {
            let _ = paths::write_private(file, text.as_bytes());
        }
        merged
    }

    fn remove_one(&self, name: &str, info: &RepoInfo, force: bool) -> Result<()> {
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
            git::bare(&info.bare, &args)?;
        }
        // Delete the local branch only when its tip is on a remote branch or was merged.
        let reference = format!("refs/heads/{}", info.branch);
        let tip = git::try_bare(&info.bare, &["rev-parse", "-q", "--verify", &reference])?;
        if tip.ok {
            let contains = git::try_bare(&info.bare, &["branch", "-r", "--contains", &tip.stdout])?;
            if !contains.stdout.is_empty() || self.merged(&info.bare, &tip.stdout) {
                let _ = git::try_bare(&info.bare, &["branch", "-D", &info.branch]);
            }
        }
        Ok(())
    }

    pub fn remove_repo(&self, session: &Path, repo: &str, force: bool) -> Result<Value> {
        let mut manifest = Self::load_manifest(session)?;
        let name = self.lookup(repo)?.repo;
        let info = manifest
            .repos
            .get(&name)
            .cloned()
            .ok_or_else(|| anyhow!("repo not in session: {name}"))?;
        self.remove_one(&name, &info, force)?;
        manifest.repos.remove(&name);
        Self::save_manifest(session, &manifest)?;
        Self::write_agents(session, &manifest)?;
        Ok(json!({ "removed": name, "session": session }))
    }

    /// Remove a whole session, or nothing at all if any repo in it holds unsaved work.
    pub fn remove_session(&self, session: &Path) -> Result<Value> {
        let manifest = Self::load_manifest(session)?;
        let mut problems = Vec::new();
        for (name, info) in &manifest.repos {
            if info.path.exists() {
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
        for (name, info) in &manifest.repos {
            self.remove_one(name, info, false)?;
        }
        for info in manifest.repos.values() {
            let _ = git::try_bare(&info.bare, &["worktree", "prune"]);
        }
        fs::remove_dir_all(session)?;
        Ok(json!({ "removed_session": session }))
    }

    pub fn status(&self, session: &Path) -> Result<Value> {
        let manifest = Self::load_manifest(session)?;
        let mut repos = serde_json::Map::new();
        for (name, info) in &manifest.repos {
            let mut entry = serde_json::to_value(info)?;
            if info.path.exists() {
                entry["dirty"] =
                    json!(!git::tree(&info.path, &["status", "--porcelain"])?.is_empty());
                entry["head"] = json!(git::tree(&info.path, &["rev-parse", "--short", "HEAD"])?);
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
            // A worktree requested from inside a session: a sibling checkout of that repo.
            let top = PathBuf::from(git::tree(&cwd, &["rev-parse", "--show-toplevel"])?);
            let root = self.find_session_root(&cwd)?;
            let leaf = top
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let target = root.join(".subworktrees").join(format!("{name}-{leaf}"));
            fs::create_dir_all(target.parent().unwrap_or(&root))?;
            let common = git::tree(
                &cwd,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?;
            let branch = format!("{}/{name}", self.config.branch_prefix);
            let target_arg = target.to_string_lossy().to_string();
            git::bare(
                Path::new(&common),
                &["worktree", "add", "-b", &branch, &target_arg, "HEAD"],
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
        let common = git::tree(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let path_arg = path.to_string_lossy().to_string();
        git::bare(Path::new(&common), &["worktree", "remove", &path_arg])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{valid_branch, valid_name, Unsaved};

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
