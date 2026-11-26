//! Thin wrappers around the `git` command line, and the guarded way
//! `nucleate` and the daemon run it.

use crate::paths;
use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Git may never ask for anything: nothing answers it. A fetch that needs
/// credentials git cannot find fails at once, rather than waiting at a
/// prompt on whatever terminal the session that ran it holds.
const NO_PROMPT: (&str, &str) = ("GIT_TERMINAL_PROMPT", "0");

/// Run git and return its result whether or not it succeeded.
pub fn try_git<I, S>(args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new("git")
        .env(NO_PROMPT.0, NO_PROMPT.1)
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
        .env(NO_PROMPT.0, NO_PROMPT.1)
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

// ---- the guarded git -------------------------------------------------------
//
// `nucleate` and the daemon run git on the host, as the agent user, in bare
// repos and worktrees that agent sessions can write: `nucleate` from Claude
// Code's worktree hooks and from the commands a person runs, the daemon on
// its own schedule. Both run it the same way. A repository's own config can
// name programs for git to run (`core.fsmonitor`, `core.sshCommand`, a
// filter's `clean`, an `include.path` that brings in more), and a worktree's
// `.git` file or a `commondir` can point git at another repository whose
// config does. So before either runs git in a repository it reads that
// repository's config with git's own parser, following no includes, and
// refuses it unless every key is one it knows to name no program. It checks
// that every pointer leads into the nucleus's `.repos`. And it runs git with
// nothing from the system's config or attributes, its own global config, and
// hooks, the file-system monitor and an attributes file switched off. Where
// the nucleus's config names a GitHub token file, git signs in to GitHub with
// it, in place of whatever helper the global config names for GitHub.

/// Keys of a repository's config that name no program and change nothing
/// either relies on. Section and key are compared without case, as git
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

/// Variables of the caller's own environment that would point git at
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
/// The most a call given a time limit may print.
const MAX_TIMED_OUTPUT: u64 = 1024 * 1024;
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
            Reason::Config => "its git config sets a key Nucarium does not allow",
            Reason::Pointer => "a git pointer in it leads outside the nucleus's repos",
            Reason::Unreadable => "its git config could not be read",
            Reason::Submodule => "it holds a submodule, whose own git config is not checked",
        }
    }
}

/// A repository git is not run in. The error a guarded call
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

/// GitHub's git host, where its repositories are fetched from and pushed to.
pub const GITHUB_ORIGIN: &str = "https://github.com";

/// The token file git signs in to GitHub with.
#[derive(Debug, Clone)]
struct SignIn {
    /// [`GITHUB_ORIGIN`], or a stand-in on this machine, for tests.
    origin: String,
    /// An absolute path: git runs the helper from the folder it is run in.
    token_file: PathBuf,
}

/// A path quoted for `sh`, byte for byte.
fn sh_quoted(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut quoted = vec![b'\''];
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'\'' => quoted.extend_from_slice(b"'\\''"),
            byte => quoted.push(byte),
        }
    }
    quoted.push(b'\'');
    quoted
}

impl SignIn {
    /// The settings that put this helper in place of every credential helper
    /// the user's config names for the origin: an empty one first, which
    /// empties git's list, then this one. Git runs it with `get` when a
    /// server asks who it is; it reads the file then, and hands the token to
    /// git on its output, so the token is in no command line and no
    /// environment. A file that is not there gives git nothing, and the call
    /// fails as one with no credentials does.
    fn args(&self) -> [OsString; 4] {
        use std::os::unix::ffi::OsStringExt;
        let key = format!("credential.{}.helper=", self.origin);
        let file = sh_quoted(&self.token_file);
        let mut helper = key.clone().into_bytes();
        helper.extend_from_slice(b"!f() { test \"$1\" = get || return 0; test -s ");
        helper.extend_from_slice(&file);
        helper.extend_from_slice(
            b" || return 0; printf 'username=x-access-token\\npassword='; tr -d '[:space:]' < ",
        );
        helper.extend_from_slice(&file);
        helper.extend_from_slice(b"; printf '\\n'; }; f");
        [
            "-c".into(),
            key.into(),
            "-c".into(),
            OsString::from_vec(helper),
        ]
    }
}

/// The guarded way of running git.
#[derive(Debug, Clone)]
pub struct Guarded {
    global: PathBuf,
    /// How git signs in to GitHub, when a token file is named. Without one,
    /// the user's own config says.
    github: Option<SignIn>,
    /// Whether a refusal is logged. The daemon logs it; `nucleate` fails
    /// with it instead, and the error says the same.
    logs: bool,
    /// Where every repository's git data must be: the nucleus's `.repos`.
    /// Nothing for the worktrees of a driver that owns them, which belong to
    /// repositories of the owner's choosing; their config is still vetted.
    repos: Option<PathBuf>,
    /// Without `repos`, the folders every repository's git data must still
    /// be in, when they are named: those of a driver in a container, which
    /// is all of the box such a driver's worktrees can be of.
    within: Option<Vec<PathBuf>>,
    vetting: Vetting,
}

/// Refusals already logged, so that each is logged once while it lasts.
static LOGGED: std::sync::Mutex<std::collections::BTreeSet<(PathBuf, Reason)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// The global config of the user git is run as: the one its own
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

/// A small file's text. Anything but a plain file is an error, and is not
/// opened: a pipe a session planted in place of a pointer is never waited
/// on, and a device never opened. A link is followed, to a plain file only.
pub(crate) fn read_small(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::Read;
    let looked = paths::look_regular(path, true)?;
    let file = paths::open_looked(path, true, &looked)?;
    let mut text = String::new();
    file.take(max).read_to_string(&mut text)?;
    Ok(text)
}

impl Guarded {
    /// `repos` is the nucleus's `.repos`, or nothing for a driver's worktrees.
    pub fn new(repos: Option<PathBuf>, vetting: Vetting) -> Guarded {
        Guarded {
            global: own_global_config(),
            github: None,
            logs: true,
            repos,
            within: None,
            vetting,
        }
    }

    /// Hold the git data of repositories with no `.repos` to these folders.
    /// A folder that is not there holds nothing.
    pub fn held_to(mut self, folders: &[PathBuf]) -> Guarded {
        self.within = Some(
            folders
                .iter()
                .filter_map(|folder| folder.canonicalize().ok())
                .collect(),
        );
        self
    }

    /// Log no refusal: the caller fails with it instead.
    pub fn quiet(mut self) -> Guarded {
        self.logs = false;
        self
    }

    /// Sign in to GitHub at `origin` as `x-access-token`, with the token in
    /// `token_file`, read again each time git asks. Nothing changes without
    /// a file.
    pub fn signing_in_to(mut self, origin: &str, token_file: Option<&Path>) -> Guarded {
        self.github = token_file.map(|file| SignIn {
            origin: origin.to_string(),
            token_file: std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf()),
        });
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        for name in INHERITED {
            command.env_remove(name);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.global)
            .env(NO_PROMPT.0, NO_PROMPT.1);
        command
    }

    fn inside(&self, path: &Path) -> bool {
        let Some(repos) = &self.repos else {
            return self
                .within
                .as_ref()
                .is_none_or(|within| within.iter().any(|folder| path.starts_with(folder)));
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
            Err(_) if !self.logs => {}
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
        if let Some(github) = &self.github {
            command.args(github.args());
        }
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

/// How a caller runs git: as the user would, or guarded, as `nucleate` and
/// the daemon do.
#[derive(Debug, Clone)]
pub enum Git {
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

/// The output of a call that must succeed, or an error that names the
/// repository it was run in, as [`git`] does.
fn checked(out: Output, place: Option<(&str, &Path)>, args: &[&str]) -> Result<String> {
    if !out.ok {
        let place = place
            .map(|(flag, dir)| format!("{flag} {} ", dir.display()))
            .unwrap_or_default();
        bail!("git {place}{} failed: {}", args.join(" "), out.stderr);
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

    /// `git init --bare` a new repository at `dir`. Guarded, git is run from
    /// `/`, so that no repository around the caller's folder is read, with
    /// the caller's own global config and nothing else.
    pub fn init_bare(&self, dir: &Path) -> Result<()> {
        let shown = dir.to_string_lossy();
        let args = ["init", "-q", "--bare", &shown];
        match self {
            Git::Plain => git(args).map(drop),
            Git::Guarded(guarded) => {
                let mut command = guarded.command();
                command
                    .current_dir("/")
                    .args(OVERRIDES)
                    .args(&args[..3])
                    .arg(dir);
                checked(collect(&mut command)?, None, &args).map(drop)
            }
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
            Git::Guarded(_) => checked(self.try_bare(dir, args)?, Some(("--git-dir", dir)), args),
        }
    }

    /// `git worktree repair` in a bare repo, so that the worktrees it has
    /// registered point at it again after it moved. Run from inside the
    /// bare repo itself: repair also repairs the folder it is run from, and
    /// run from another repository's worktree it would point that one here.
    pub fn repair_worktrees(&self, bare: &Path) -> Result<()> {
        let args = ["worktree", "repair"];
        let out = match self {
            Git::Plain => {
                let mut command = Command::new("git");
                command.arg("--git-dir").arg(bare).args(args);
                collect(command.current_dir(bare))?
            }
            Git::Guarded(guarded) => {
                collect(guarded.prepare(bare, true, &args, None)?.current_dir(bare))?
            }
        };
        checked(out, Some(("--git-dir", bare)), &args).map(drop)
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
            Git::Guarded(_) => checked(self.try_tree(dir, args)?, Some(("-C", dir)), args),
        }
    }

    /// As [`Git::try_tree`], stopped once it has run for `timeout`: a call
    /// that runs over comes back failed, with `timed out` for its error
    /// output. For a call that waits on a network, such as a push.
    pub fn try_tree_within(
        &self,
        dir: &Path,
        args: &[&str],
        timeout: std::time::Duration,
    ) -> Result<Output> {
        let mut command = match self {
            Git::Plain => {
                let mut command = Command::new("git");
                command
                    .env(NO_PROMPT.0, NO_PROMPT.1)
                    .args(with_prefix("-C", dir, args));
                command
            }
            Git::Guarded(guarded) => guarded.prepare(dir, false, args, None)?,
        };
        match crate::bounded::run(&mut command, timeout, MAX_TIMED_OUTPUT) {
            Ok(out) => Ok(Output {
                ok: out.ok,
                stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
                stderr: out.stderr,
            }),
            Err(crate::bounded::Failure::Late) => Ok(Output {
                ok: false,
                stdout: String::new(),
                stderr: "timed out".to_string(),
            }),
            Err(failure) => bail!("running git: {failure:?}"),
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
    use super::{allowed, Git, Guarded, Vetting};
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// The tokens a test signs in with, and the Basic credentials each makes
    /// as `x-access-token`.
    const TOKEN: &str = "ghs_test1token2value3";
    const TOKEN_BASIC: &str = "Basic eC1hY2Nlc3MtdG9rZW46Z2hzX3Rlc3QxdG9rZW4ydmFsdWUz";
    const NEW_TOKEN: &str = "ghs_new4token5value6";
    const NEW_TOKEN_BASIC: &str = "Basic eC1hY2Nlc3MtdG9rZW46Z2hzX25ldzR0b2tlbjV2YWx1ZTY=";

    /// Run git as a person would, with nothing from their own config.
    fn plain_git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with one commit, ready to be served over git's dumb HTTP.
    fn upstream(dir: &Path) -> PathBuf {
        let repo = dir.join("upstream");
        std::fs::create_dir_all(&repo).unwrap();
        plain_git(&repo, &["init", "-q", "-b", "main"]);
        plain_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        plain_git(&repo, &["update-server-info"]);
        repo
    }

    /// A server that serves `repo` as `/octo/web.git` to a client whose
    /// `Authorization` is the one `accepted` holds at the time, and asks
    /// every other who it is. Returns the server's origin.
    fn serves_with_auth(repo: &Path, accepted: Arc<Mutex<String>>) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", server.server_addr());
        let git_dir = repo.join(".git");
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let signed_in = request.headers().iter().any(|header| {
                    header.field.equiv("Authorization")
                        && header.value.as_str() == accepted.lock().unwrap().as_str()
                });
                if !signed_in {
                    let header =
                        tiny_http::Header::from_bytes("WWW-Authenticate", "Basic realm=\"web\"")
                            .unwrap();
                    let _ = request.respond(tiny_http::Response::empty(401).with_header(header));
                    continue;
                }
                let path = request.url().split('?').next().unwrap_or_default();
                let file = path
                    .strip_prefix("/octo/web.git/")
                    .filter(|rest| !rest.split('/').any(|part| part == ".."))
                    .and_then(|rest| std::fs::read(git_dir.join(rest)).ok());
                let _ = match file {
                    Some(bytes) => request.respond(tiny_http::Response::from_data(bytes)),
                    None => request.respond(tiny_http::Response::empty(404)),
                };
            }
        });
        origin
    }

    /// A global config whose credential helpers, the one for every host and
    /// the one for `origin`, answer with `password`, as the box-wide helpers
    /// of another account would.
    fn global_helper(dir: &Path, origin: &str, password: &str) -> PathBuf {
        let file = dir.join("gitconfig");
        let helper = format!(
            "\thelper = \"!f() {{ echo username=x-access-token; echo password={password}; }}; f\"\n"
        );
        std::fs::write(
            &file,
            format!("[credential]\n{helper}[credential \"{origin}\"]\n{helper}"),
        )
        .unwrap();
        file
    }

    #[test]
    fn a_fetch_signs_in_with_the_token_file_in_place_of_the_users_own_helper() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = upstream(dir.path());
        let accepted = Arc::new(Mutex::new(TOKEN_BASIC.to_string()));
        let origin = serves_with_auth(&upstream, accepted.clone());
        let url = format!("{origin}/octo/web.git");
        // A path the helper must quote.
        let token_file = dir.path().join("it's a token");
        std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();

        let mut guarded = Guarded::new(None, Vetting::Enforce)
            .quiet()
            .signing_in_to(&origin, Some(&token_file));
        // The box-wide helper signs in as another account, which this server refuses.
        guarded.global = global_helper(dir.path(), &origin, "ghs_another_accounts_token");
        let git = Git::Guarded(guarded.clone());
        let repo = dir.path().join("web.git");
        git.init_bare(&repo).unwrap();

        let fetched = git
            .try_bare(
                &repo,
                &["fetch", "-q", &url, "+refs/heads/*:refs/remotes/origin/*"],
            )
            .unwrap();
        assert!(fetched.ok, "{}", fetched.stderr);
        assert_eq!(
            git.bare(&repo, &["rev-parse", "refs/remotes/origin/main"])
                .unwrap(),
            plain_git(&upstream, &["rev-parse", "HEAD"])
        );

        // The token is on no command line and in no environment: not git's,
        // and not those of the programs git runs, whose command lines the
        // trace records.
        let trace = dir.path().join("trace");
        let mut command = guarded
            .prepare(&repo, true, &["ls-remote", &url], None)
            .unwrap();
        command.env("GIT_TRACE", &trace);
        let shown = |text: &OsStr| text.to_string_lossy().contains(TOKEN);
        assert!(!command.get_args().any(shown));
        assert!(!command
            .get_envs()
            .any(|(name, value)| shown(name) || value.is_some_and(shown)));
        assert!(!std::env::vars_os().any(|(name, value)| shown(&name) || shown(&value)));
        let listed = command.output().unwrap();
        assert!(listed.status.success());
        let trace = std::fs::read_to_string(&trace).unwrap();
        assert!(trace.contains("run_command"), "{trace}");
        assert!(!trace.contains(TOKEN));

        // A replaced file is read on the next call.
        *accepted.lock().unwrap() = NEW_TOKEN_BASIC.to_string();
        std::fs::write(&token_file, NEW_TOKEN).unwrap();
        let listed = git.try_bare(&repo, &["ls-remote", &url]).unwrap();
        assert!(listed.ok, "{}", listed.stderr);

        // A file that is gone gives git nothing to sign in with.
        std::fs::remove_file(&token_file).unwrap();
        let listed = git.try_bare(&repo, &["ls-remote", &url]).unwrap();
        assert!(!listed.ok);
        assert!(
            listed.stderr.contains("terminal prompts disabled"),
            "{}",
            listed.stderr
        );
    }

    #[test]
    fn without_a_token_file_git_signs_in_with_the_users_own_helper() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = upstream(dir.path());
        let origin = serves_with_auth(&upstream, Arc::new(Mutex::new(TOKEN_BASIC.to_string())));
        let url = format!("{origin}/octo/web.git");
        let mut guarded = Guarded::new(None, Vetting::Enforce)
            .quiet()
            .signing_in_to(&origin, None);
        guarded.global = global_helper(dir.path(), &origin, TOKEN);
        let git = Git::Guarded(guarded);
        let repo = dir.path().join("web.git");
        git.init_bare(&repo).unwrap();
        let listed = git.try_bare(&repo, &["ls-remote", &url]).unwrap();
        assert!(listed.ok, "{}", listed.stderr);
    }

    #[test]
    fn a_token_file_signs_in_to_github_and_nowhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = upstream(dir.path());
        let origin = serves_with_auth(&upstream, Arc::new(Mutex::new(TOKEN_BASIC.to_string())));
        let url = format!("{origin}/octo/web.git");
        let token_file = dir.path().join("github-token");
        std::fs::write(&token_file, TOKEN).unwrap();
        let mut guarded = Guarded::new(None, Vetting::Enforce)
            .quiet()
            .signing_in_to(super::GITHUB_ORIGIN, Some(&token_file));
        guarded.global = PathBuf::from("/dev/null");
        let git = Git::Guarded(guarded);
        let repo = dir.path().join("web.git");
        git.init_bare(&repo).unwrap();
        let listed = git.try_bare(&repo, &["ls-remote", &url]).unwrap();
        assert!(!listed.ok);
        assert!(
            listed.stderr.contains("terminal prompts disabled"),
            "{}",
            listed.stderr
        );
    }

    /// A server that asks for credentials on every request.
    fn asks_for_credentials() -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/octo/web.git", server.server_addr());
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let header =
                    tiny_http::Header::from_bytes("WWW-Authenticate", "Basic realm=\"web\"")
                        .unwrap();
                let _ = request.respond(tiny_http::Response::empty(401).with_header(header));
            }
        });
        url
    }

    #[test]
    fn a_fetch_that_needs_credentials_fails_at_once_without_prompting() {
        let url = asks_for_credentials();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("web.git");
        for git in [
            Git::Plain,
            Git::Guarded(Guarded::new(None, Vetting::Enforce).quiet()),
        ] {
            git.init_bare(&repo).unwrap();
            let out = git.try_bare(&repo, &["ls-remote", &url]).unwrap();
            assert!(!out.ok, "{git:?}");
            assert!(
                out.stderr.contains("terminal prompts disabled"),
                "{git:?}: {}",
                out.stderr
            );
        }
    }

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
