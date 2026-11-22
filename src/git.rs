//! Thin wrappers around the `git` command line, and the guarded way the
//! daemon runs it.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run git and return its result whether or not it succeeded.
pub fn try_git<I, S>(args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new("git")
        .args(args)
        .output()
        .context("running git (is it installed?)")?;
    Ok(Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Run git and fail with its error output if it did not succeed.
pub fn git<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let out = try_git(&args)?;
    if !out.ok {
        let shown: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
        bail!("git {} failed: {}", shown.join(" "), out.stderr);
    }
    Ok(out.stdout)
}

fn with_prefix(flag: &str, dir: &Path, args: &[&str]) -> Vec<std::ffi::OsString> {
    let mut all = vec![flag.into(), dir.as_os_str().to_os_string()];
    all.extend(args.iter().map(|a| a.into()));
    all
}

/// `git --git-dir <bare> ...`
pub fn bare(dir: &Path, args: &[&str]) -> Result<String> {
    git(with_prefix("--git-dir", dir, args))
}

pub fn try_bare(dir: &Path, args: &[&str]) -> Result<Output> {
    try_git(with_prefix("--git-dir", dir, args))
}

/// `git -C <worktree> ...`
pub fn tree(dir: &Path, args: &[&str]) -> Result<String> {
    git(with_prefix("-C", dir, args))
}

pub fn try_tree(dir: &Path, args: &[&str]) -> Result<Output> {
    try_git(with_prefix("-C", dir, args))
}

/// Run git in a worktree with extra environment variables and return its
/// output untrimmed, for output where leading spaces and NULs matter.
pub fn tree_raw(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &OsStr)],
) -> Result<(bool, Vec<u8>, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .context("running git (is it installed?)")?;
    Ok((
        out.status.success(),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

// ---- the daemon's git -----------------------------------------------------
//
// The daemon runs git on the host, as the agent user, in bare repos and
// worktrees that agent sessions can write. A repository's own config can
// name programs for git to run (`core.fsmonitor`, `core.sshCommand`, a
// filter's `clean`, an `include.path` that brings in more), and a worktree's
// `.git` file or a `commondir` can point git at another repository whose
// config does. So before the daemon runs git in a repository it reads that
// repository's config with git's own parser, following no includes, and
// refuses it unless every key is one it knows to name no program. It checks
// that every pointer leads into the nucleus's `.repos`. And it runs git with
// nothing from the system's config or attributes, its own global config, and
// hooks, the file-system monitor and an attributes file switched off.

/// Keys of a repository's config that name no program and change nothing
/// the daemon relies on. Section and key are compared without case, as git
/// compares them; `*` is a subsection, which must be there.
const ALLOWED: [(&str, bool, &str); 12] = [
    ("core", false, "repositoryformatversion"),
    ("core", false, "bare"),
    ("core", false, "filemode"),
    ("core", false, "logallrefupdates"),
    ("core", false, "ignorecase"),
    ("core", false, "precomposeunicode"),
    // Overridden on every call, so it does no harm.
    ("core", false, "hookspath"),
    ("remote", true, "url"),
    ("remote", true, "fetch"),
    ("branch", true, "remote"),
    ("branch", true, "merge"),
    ("branch", true, "vscode-merge-base"),
];

/// Settings given to every call, ahead of anything in a repository.
/// `diff.ignoreSubmodules` keeps `git status` out of a submodule, whose own
/// config is not checked; `git add` goes into one whatever is set, which is
/// why a worktree whose index names one is refused before either runs.
const OVERRIDES: [&str; 8] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.attributesFile=/dev/null",
    "-c",
    "diff.ignoreSubmodules=all",
];

/// The git commands that start git again inside each checked-out
/// submodule, where the submodule's own config would be read.
const ENTER_SUBMODULES: [&str; 2] = ["status", "add"];
/// The most of a worktree's index listing that is read, and how long for.
const MAX_LISTING: u64 = 256 * 1024 * 1024;
const LISTING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Variables of the daemon's own environment that would point git at
/// another repository or bring in config of their own.
const INHERITED: [&str; 11] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_SYSTEM",
];

/// The most of a config file that is read. A real one is a few hundred bytes.
const MAX_CONFIG: u64 = 1024 * 1024;
const CONFIG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// The most of a worktree's `.git` file or a `commondir` that is read.
const MAX_POINTER: u64 = 4096;

/// True when one entry of a repository's config, as `git config --list`
/// prints it, is on the allowlist.
pub fn allowed(key: &str, value: Option<&str>) -> bool {
    let key = key.to_ascii_lowercase();
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let (subsection, name) = match rest.rsplit_once('.') {
        Some((subsection, name)) => (!subsection.is_empty(), name),
        None => (false, rest),
    };
    if section == "extensions" && !subsection && name == "worktreeconfig" {
        return true;
    }
    if section == "gc" && !subsection {
        return value.is_some_and(numeric);
    }
    ALLOWED
        .iter()
        .any(|&(s, sub, n)| s == section && sub == subsection && n == name)
}

/// A whole number as git reads one: an optional sign and a `k`, `m` or `g`.
fn numeric(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let digits = digits
        .strip_suffix(['k', 'K', 'm', 'M', 'g', 'G'])
        .unwrap_or(digits);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Why a repository was refused: a fixed word, and nothing the repository
/// itself says.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// Its config sets a key that is not on the allowlist, or one that is
    /// with a value it does not take.
    Config,
    /// A worktree's `.git`, or a `commondir`, leads outside the nucleus's
    /// repos, or nowhere.
    Pointer,
    /// Its config could not be read.
    Unreadable,
    /// A worktree's index names a submodule. Git starts again inside one
    /// that is checked out, where that repository's own config is read.
    Submodule,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Config => "config",
            Reason::Pointer => "pointer",
            Reason::Unreadable => "unreadable",
            Reason::Submodule => "submodule",
        }
    }

    fn sentence(self) -> &'static str {
        match self {
            Reason::Config => "its git config sets a key nucleated does not allow",
            Reason::Pointer => "a git pointer in it leads outside the nucleus's repos",
            Reason::Unreadable => "its git config could not be read",
            Reason::Submodule => "it holds a submodule, whose own git config is not checked",
        }
    }
}

/// A repository the daemon does not run git in. The error a guarded call
/// fails with; find it with [`refusal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The repository's folder name, without `.git`, or the worktree's.
    pub repo: String,
    pub reason: Reason,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused repo {}: {}", self.repo, self.reason.sentence())
    }
}

impl std::error::Error for Refused {}

/// The refusal an error is, if it is one.
pub fn refusal(error: &anyhow::Error) -> Option<&Refused> {
    error.downcast_ref::<Refused>()
}

/// What happens to a repository that fails the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Vetting {
    /// It is refused: git is not run in it.
    #[default]
    Enforce,
    /// It is logged and shown, and git is run in it anyway, with the same
    /// overrides. For finding out what a box's repos hold before enforcing.
    Report,
}

/// The repository a vetted call runs in, with every pointer resolved.
struct Pinned {
    git_dir: PathBuf,
    common: PathBuf,
}

/// The daemon's way of running git.
#[derive(Debug, Clone)]
pub struct Guarded {
    global: PathBuf,
    /// Where every repository's git data must be: the nucleus's `.repos`.
    /// Nothing for the worktrees of a driver that owns them, which belong to
    /// repositories of the owner's choosing; their config is still vetted.
    repos: Option<PathBuf>,
    vetting: Vetting,
}

/// Refusals already logged, so that each is logged once while it lasts.
static LOGGED: std::sync::Mutex<std::collections::BTreeSet<(PathBuf, Reason)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// The global config of the user the daemon runs as: the one its own
/// environment names, else `~/.gitconfig`, else git's XDG file, else none.
fn own_global_config() -> PathBuf {
    if let Some(named) = std::env::var_os("GIT_CONFIG_GLOBAL").filter(|v| !v.is_empty()) {
        return PathBuf::from(named);
    }
    let home = crate::paths::home();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")))
        .map(|dir| dir.join("git").join("config"));
    home.map(|home| home.join(".gitconfig"))
        .into_iter()
        .chain(xdg)
        .find(|file| file.is_file())
        .unwrap_or_else(|| PathBuf::from("/dev/null"))
}

fn short_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    name.strip_suffix(".git").unwrap_or(&name).to_string()
}

/// A small file's text. Anything but a plain file is an error: opening a
/// pipe a session planted in place of a pointer must not wait for a writer.
fn read_small(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a plain file"));
    }
    let mut text = String::new();
    file.take(max).read_to_string(&mut text)?;
    Ok(text)
}

impl Guarded {
    /// `repos` is the nucleus's `.repos`, or nothing for a driver's worktrees.
    pub fn new(repos: Option<PathBuf>, vetting: Vetting) -> Guarded {
        Guarded {
            global: own_global_config(),
            repos,
            vetting,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        for name in INHERITED {
            command.env_remove(name);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.global);
        command
    }

    fn inside(&self, path: &Path) -> bool {
        let Some(repos) = &self.repos else {
            return true;
        };
        repos
            .canonicalize()
            .is_ok_and(|repos| path != repos && path.starts_with(repos))
    }

    /// Read one config file and hold it to the allowlist. A file that is not
    /// there holds nothing.
    fn vet_file(&self, file: &Path) -> Result<(), Reason> {
        if !file.exists() {
            return Ok(());
        }
        let mut command = self.command();
        command
            .current_dir("/")
            .args(["config", "--no-includes", "--null", "--list", "--file"])
            .arg(file);
        let out = crate::bounded::run(&mut command, CONFIG_TIMEOUT, MAX_CONFIG)
            .map_err(|_| Reason::Unreadable)?;
        if !out.ok {
            return Err(Reason::Unreadable);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for entry in text.split('\0').filter(|entry| !entry.is_empty()) {
            let (key, value) = match entry.split_once('\n') {
                Some((key, value)) => (key, Some(value)),
                None => (entry, None),
            };
            if !allowed(key, value) {
                return Err(Reason::Config);
            }
        }
        Ok(())
    }

    /// Resolve a git folder and the common folder it names, check both are
    /// inside the nucleus's repos, and vet the config of each.
    fn pin_git_dir(&self, git_dir: &Path) -> Result<Pinned, Reason> {
        let git_dir = git_dir.canonicalize().map_err(|_| Reason::Pointer)?;
        if !self.inside(&git_dir) {
            return Err(Reason::Pointer);
        }
        let common = match read_small(&git_dir.join("commondir"), MAX_POINTER) {
            Ok(text) => git_dir
                .join(text.trim_end_matches(['\n', '\r']))
                .canonicalize()
                .map_err(|_| Reason::Pointer)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
            Err(_) => return Err(Reason::Pointer),
        };
        if !self.inside(&common) {
            return Err(Reason::Pointer);
        }
        self.vet_file(&common.join("config"))?;
        self.vet_file(&git_dir.join("config.worktree"))?;
        Ok(Pinned { git_dir, common })
    }

    /// A worktree's git folder, from its `.git` file. A worktree with no
    /// `.git` of its own is refused: git would look for one in the folders
    /// above it.
    fn pin_worktree(&self, dir: &Path) -> Result<Pinned, Reason> {
        let dot = dir.join(".git");
        let meta = std::fs::symlink_metadata(&dot).map_err(|_| Reason::Pointer)?;
        if !meta.is_file() {
            return self.pin_git_dir(&dot);
        }
        let text = read_small(&dot, MAX_POINTER).map_err(|_| Reason::Pointer)?;
        let target = text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("gitdir:"))
            .map(str::trim)
            .filter(|target| !target.is_empty())
            .ok_or(Reason::Pointer)?;
        self.pin_git_dir(&dir.join(target))
    }

    /// True when a worktree's index, or the one a call is given, names a
    /// submodule at all, checked out or not: one that is not checked out now
    /// can be by the time git looks. The listing itself goes into none.
    fn holds_submodule(
        &self,
        dir: &Path,
        pinned: &Pinned,
        index: Option<&OsStr>,
    ) -> Result<bool, Reason> {
        let mut command = self.command();
        command.args(OVERRIDES);
        if pinned.common != pinned.git_dir {
            command.env("GIT_COMMON_DIR", &pinned.common);
        }
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command
            .arg("-C")
            .arg(dir)
            .arg("--git-dir")
            .arg(&pinned.git_dir)
            .arg("--work-tree")
            .arg(dir)
            .args(["ls-files", "--stage", "-z"]);
        let out = crate::bounded::run(&mut command, LISTING_TIMEOUT, MAX_LISTING)
            .map_err(|_| Reason::Unreadable)?;
        if !out.ok {
            return Err(Reason::Unreadable);
        }
        // Each entry starts with its mode; a submodule's is 160000.
        Ok(out
            .stdout
            .split(|byte| *byte == 0)
            .any(|entry| entry.starts_with(b"160000 ")))
    }

    /// Log a refusal the first time it is found, and forget it once the
    /// repository passes a full check, so that a later one is logged again.
    fn note(&self, key: &Path, outcome: &Result<Pinned, Refused>, full: bool) {
        let Ok(mut logged) = LOGGED.lock() else {
            return;
        };
        match outcome {
            Ok(_) if !full => {}
            Ok(_) => logged.retain(|(path, _)| path != key),
            Err(refused) => {
                if logged.insert((key.to_path_buf(), refused.reason)) {
                    match self.vetting {
                        Vetting::Enforce => eprintln!("nucleated: {refused}"),
                        Vetting::Report => eprintln!(
                            "nucleated: would have {refused}; report-only, so git runs in it"
                        ),
                    }
                }
            }
        }
    }

    /// Check a repository. For a worktree, `submodules` also looks for a
    /// submodule in its index, or in `index` when the call is given one,
    /// which only the commands that go into one need.
    fn vet(
        &self,
        dir: &Path,
        bare: bool,
        submodules: bool,
        index: Option<&OsStr>,
    ) -> Result<Pinned, Refused> {
        let pinned = if bare {
            self.pin_git_dir(dir)
        } else {
            self.pin_worktree(dir).and_then(|pinned| {
                match submodules && self.holds_submodule(dir, &pinned, index)? {
                    true => Err(Reason::Submodule),
                    false => Ok(pinned),
                }
            })
        };
        let outcome = pinned.map_err(|reason| Refused {
            repo: short_name(dir),
            reason,
        });
        self.note(dir, &outcome, bare || submodules);
        outcome
    }

    /// The command for one call in `dir`, vetted first. Refused, it is an
    /// error when enforcing, and the plain location when only reporting.
    fn prepare(
        &self,
        dir: &Path,
        bare: bool,
        args: &[&str],
        index: Option<&OsStr>,
    ) -> Result<Command> {
        let mut command = self.command();
        command.args(OVERRIDES);
        let command_name = args.iter().find(|arg| !arg.starts_with('-'));
        let submodules = command_name.is_some_and(|name| ENTER_SUBMODULES.contains(name));
        match self.vet(dir, bare, submodules, index) {
            Ok(pinned) => {
                // Pinned, so that a pointer changed after the check is not
                // followed: git takes both folders from here. A repository
                // that is its own common folder is given none, which git
                // would read as a linked worktree's.
                if pinned.common != pinned.git_dir {
                    command.env("GIT_COMMON_DIR", &pinned.common);
                }
                if !bare {
                    command.arg("-C").arg(dir);
                }
                command.arg("--git-dir").arg(&pinned.git_dir);
                if !bare {
                    command.arg("--work-tree").arg(dir);
                }
            }
            Err(refused) if self.vetting == Vetting::Enforce => return Err(refused.into()),
            Err(_) => {
                command.arg(if bare { "--git-dir" } else { "-C" }).arg(dir);
            }
        }
        command.args(args);
        Ok(command)
    }
}

/// How a caller runs git: as the user would, or guarded, as the daemon does.
#[derive(Debug, Clone, Default)]
pub enum Git {
    #[default]
    Plain,
    Guarded(Guarded),
}

fn collect(command: &mut Command) -> Result<Output> {
    let out = command.output().context("running git (is it installed?)")?;
    Ok(Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

fn checked(out: Output, args: &[&str]) -> Result<String> {
    if !out.ok {
        bail!("git {} failed: {}", args.join(" "), out.stderr);
    }
    Ok(out.stdout)
}

impl Git {
    /// True when a refused repository is not run in.
    pub fn enforcing(&self) -> bool {
        matches!(self, Git::Guarded(guarded) if guarded.vetting == Vetting::Enforce)
    }

    /// Why a bare repo would be refused, if it would, in either mode.
    pub fn vet_bare(&self, dir: &Path) -> Option<Refused> {
        match self {
            Git::Plain => None,
            Git::Guarded(guarded) => guarded.vet(dir, true, true, None).err(),
        }
    }

    /// Why a worktree would be refused, if it would, in either mode.
    pub fn vet_tree(&self, dir: &Path) -> Option<Refused> {
        match self {
            Git::Plain => None,
            Git::Guarded(guarded) => guarded.vet(dir, false, true, None).err(),
        }
    }

    pub fn try_bare(&self, dir: &Path, args: &[&str]) -> Result<Output> {
        match self {
            Git::Plain => try_bare(dir, args),
            Git::Guarded(guarded) => collect(&mut guarded.prepare(dir, true, args, None)?),
        }
    }

    pub fn bare(&self, dir: &Path, args: &[&str]) -> Result<String> {
        match self {
            Git::Plain => bare(dir, args),
            Git::Guarded(_) => checked(self.try_bare(dir, args)?, args),
        }
    }

    pub fn try_tree(&self, dir: &Path, args: &[&str]) -> Result<Output> {
        match self {
            Git::Plain => try_tree(dir, args),
            Git::Guarded(guarded) => collect(&mut guarded.prepare(dir, false, args, None)?),
        }
    }

    pub fn tree(&self, dir: &Path, args: &[&str]) -> Result<String> {
        match self {
            Git::Plain => tree(dir, args),
            Git::Guarded(_) => checked(self.try_tree(dir, args)?, args),
        }
    }

    /// As [`tree_raw`].
    pub fn tree_raw(
        &self,
        dir: &Path,
        args: &[&str],
        envs: &[(&str, &OsStr)],
    ) -> Result<(bool, Vec<u8>, String)> {
        let Git::Guarded(guarded) = self else {
            return tree_raw(dir, args, envs);
        };
        // A call given an index of its own is checked against that index.
        let index = envs
            .iter()
            .find(|(name, _)| *name == "GIT_INDEX_FILE")
            .map(|(_, value)| *value);
        let out = guarded
            .prepare(dir, false, args, index)?
            .envs(envs.iter().copied())
            .output()
            .context("running git (is it installed?)")?;
        Ok((
            out.status.success(),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn the_keys_a_clone_and_its_worktrees_write_are_allowed() {
        for (key, value) in [
            ("core.repositoryformatversion", Some("0")),
            ("core.bare", Some("true")),
            ("core.bare", None),
            ("core.filemode", Some("true")),
            ("core.logallrefupdates", Some("true")),
            ("core.ignorecase", Some("true")),
            ("core.precomposeunicode", Some("true")),
            ("core.hookspath", Some("/anywhere")),
            ("remote.origin.url", Some("https://github.com/octo/web.git")),
            (
                "remote.origin.fetch",
                Some("+refs/heads/*:refs/remotes/origin/*"),
            ),
            ("branch.workspace/fix-1.remote", Some("origin")),
            ("branch.Release.v1.2.merge", Some("refs/heads/Release.v1.2")),
            ("branch.main.vscode-merge-base", Some("origin/main")),
            ("extensions.worktreeconfig", Some("true")),
            ("gc.auto", Some("0")),
            ("gc.autopacklimit", Some("50")),
            ("gc.bigpackthreshold", Some("2g")),
            ("gc.autodetach", Some("-1")),
        ] {
            assert!(allowed(key, value), "{key} = {value:?}");
        }
        // Git prints section and key in lower case; the check does not rely on it.
        assert!(allowed("Core.HooksPath", Some("x")));
    }

    #[test]
    fn a_key_that_can_name_a_program_or_another_file_is_refused() {
        for (key, value) in [
            ("core.fsmonitor", Some("/tmp/x")),
            ("core.sshcommand", Some("/tmp/x")),
            ("core.pager", Some("/tmp/x")),
            ("core.editor", Some("/tmp/x")),
            ("core.worktree", Some("/elsewhere")),
            ("core.attributesfile", Some("/tmp/a")),
            ("filter.lfs.clean", Some("/tmp/x")),
            ("diff.any.textconv", Some("/tmp/x")),
            ("include.path", Some("/tmp/more")),
            ("includeif.gitdir:/.path", Some("/tmp/more")),
            ("remote.origin.uploadpack", Some("/tmp/x")),
            ("remote.origin.pushurl", Some("ssh://elsewhere/x")),
            (
                "url.ssh://elsewhere/.insteadof",
                Some("https://github.com/"),
            ),
            ("credential.helper", Some("!sh")),
            ("alias.st", Some("!sh")),
            ("gpg.program", Some("/tmp/x")),
            ("extensions.objectformat", Some("sha256")),
            // A known key, where it would need a subsection or must have none.
            ("branch.remote", Some("origin")),
            ("branch..remote", Some("origin")),
            ("remote.url", Some("x")),
            ("core.x.bare", Some("true")),
            // `gc.*` takes numbers only.
            ("gc.pruneexpire", Some("2.weeks.ago")),
            ("gc.auto", None),
            ("gc.auto", Some("")),
            ("gc.web.reflogexpire", Some("90")),
            ("nonsection", Some("x")),
        ] {
            assert!(!allowed(key, value), "{key} = {value:?}");
        }
    }
}
