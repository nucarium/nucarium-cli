//! `nucleated hook <event>`: the entry point an agent's hooks call.
//!
//! It reads the hook's JSON on standard input, keeps the handful of named
//! fields the adapter allows, and writes one small file to the spool. It
//! prints nothing, needs neither the daemon nor the network, and always exits
//! 0 within two seconds: nothing here may ever hold up a session.

use crate::agent::{self, Agent, Lifecycle};
use crate::driver;
use crate::envelope::{unix_ms, Draft, Envelope};
use crate::git::read_small;
use crate::github;
use crate::process;
use crate::settings::{self, Environment, Settings};
use crate::spool::{Record, Spool};
use crate::workspace::Manifest;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long the entry point may take before it gives up.
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(2);

/// Run the hook entry point. Never fails, never prints, never takes long.
/// `agent` is the `--agent` a hook's command names; with none, the hook is
/// the default agent's.
pub fn run(event: &str, env: Option<&str>, agent: Option<&str>) {
    // Whatever happens below — a pipe that never closes, a slow disk — the
    // process is gone within the limit, and it leaves with success.
    std::thread::spawn(|| {
        std::thread::sleep(GIVE_UP_AFTER);
        std::process::exit(0);
    });
    // A panic must not reach the agent's terminal either.
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| {
        if let Ok(settings) = settings::load() {
            let _ = record_as(
                &settings,
                event,
                env,
                agent,
                &mut std::io::stdin().lock(),
                SystemTime::now(),
            );
        }
        // Read whatever is left of the payload before leaving. An event this
        // program has no use for is never read above, and an agent still
        // writing to a hook that has gone is told its pipe broke. Nothing
        // read here is kept, and the limit above ends a pipe that never
        // closes.
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    });
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolve(a) == resolve(b)
}

/// True when a hook of `agent` may belong to this environment: the agent is
/// the one the environment runs, or the environment is watched through a
/// driver, which runs agents of every kind.
fn takes(environment: &Environment, agent: &dyn Agent) -> bool {
    environment.agent == agent.name() || environment.driver.is_some()
}

/// The environment a hook names outright: `--env`, else the one whose
/// `config_dir` is the agent's config folder for this process.
fn named<'a>(
    settings: &'a Settings,
    flag: Option<&str>,
    agent: &dyn Agent,
) -> Option<&'a Environment> {
    if let Some(name) = flag {
        return settings.environment(name);
    }
    let actual = std::env::var_os(agent.config_dir_var())?;
    settings.environments.iter().find(|env| {
        env.agent == agent.name()
            && env
                .config_dir
                .as_ref()
                .is_some_and(|config_dir| same_dir(Path::new(&actual), config_dir))
    })
}

/// The environment a hook belongs to by where it is running: among those
/// that take this agent's hooks, the one whose nucleus holds the session's
/// folder, else the only one there is.
fn located<'a>(
    settings: &'a Settings,
    agent: &dyn Agent,
    cwd: Option<&Path>,
) -> Option<&'a Environment> {
    let candidates: Vec<&Environment> = settings
        .environments
        .iter()
        .filter(|env| takes(env, agent))
        .collect();
    let inside = cwd.and_then(|cwd| {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut holding: Vec<(usize, &Environment)> = candidates
            .iter()
            .filter_map(|env| {
                let root = env.root.canonicalize().ok()?;
                cwd.starts_with(&root)
                    .then_some((root.components().count(), *env))
            })
            .collect();
        // The deepest nucleus wins. Two logins sharing that nucleus cannot be
        // told apart by location, so then location says nothing.
        let deepest = holding.iter().map(|(depth, _)| *depth).max()?;
        holding.retain(|(depth, _)| *depth == deepest);
        match holding.as_slice() {
            [(_, only)] => Some(*only),
            _ => None,
        }
    });
    inside.or(match candidates.as_slice() {
        [only] => Some(*only),
        _ => None,
    })
}

/// The most of a session folder's manifest that is read.
const MAX_MANIFEST: u64 = 64 * 1024;
/// The most of a pointer git keeps in a file of its own (`.git`, `HEAD`,
/// `commondir`) that is read.
const MAX_POINTER: u64 = 4096;
/// The most of a repository's git config that is read for its remote.
const MAX_GIT_CONFIG: u64 = 256 * 1024;

/// Where a session is working: its repos, its branch, and its nucleus session
/// folder if it has one.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) repos: Vec<String>,
    pub(crate) branch: Option<String>,
    pub(crate) workspace: Option<String>,
}

/// A nucleus session: the manifest names its repos and its branch.
fn nucleus_place(root: &Path, cwd: &Path) -> Option<Place> {
    let sessions = root.canonicalize().ok()?.join(".worktrees");
    let session = cwd
        .ancestors()
        .find(|dir| dir.parent() == Some(sessions.as_path()))?;
    let manifest: Manifest =
        serde_json::from_str(&read_small(&session.join(".workspace.json"), MAX_MANIFEST).ok()?)
            .ok()?;
    Some(Place {
        repos: manifest
            .repos
            .values()
            .map(|repo| repo.label.clone())
            .collect(),
        branch: Some(manifest.branch),
        workspace: session.file_name().map(|n| n.to_string_lossy().to_string()),
    })
}

/// The `url` of `[remote "origin"]` in a git config file.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
        } else if in_origin {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "url" {
                    return Some(value.trim().to_string());
                }
            }
        }
    }
    None
}

/// The name of the folder a repository lives in, from its git folder: for
/// `<name>/.git`, `<name>`, and for a bare `<name>.git`, `<name>`. Nothing
/// for a git folder that is neither.
fn repository_folder(common: &Path) -> Option<String> {
    let common = common.canonicalize().ok()?;
    let name = common.file_name()?.to_string_lossy().to_string();
    if name == ".git" {
        return Some(common.parent()?.file_name()?.to_string_lossy().to_string());
    }
    name.strip_suffix(".git")
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Any other checkout: read the branch and the remote from git's own files,
/// without running git, so this cannot hang. Each is read only if it is a
/// plain file, and only so much of it: a pipe planted in place of one is not
/// waited on, and a link to a device that never ends is not read.
///
/// A repository on GitHub is named as GitHub names it. Any other is named by
/// the folder it lives in. For a linked worktree that is the folder of the
/// repository it was added from, never the worktree's own: a tool that makes
/// a worktree per task names the folder after the task, and a task's name is
/// not a repo's.
fn checkout_place(cwd: &Path) -> Option<Place> {
    let top = cwd.ancestors().find(|dir| dir.join(".git").exists())?;
    let dot_git = top.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree: `.git` is a file pointing at its git folder.
        let pointer = read_small(&dot_git, MAX_POINTER).ok()?;
        top.join(pointer.trim().strip_prefix("gitdir:")?.trim())
    };
    let branch = read_small(&git_dir.join("HEAD"), MAX_POINTER)
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
        });
    // A linked worktree's git folder names the repository's own.
    let (common, linked) = match read_small(&git_dir.join("commondir"), MAX_POINTER) {
        Ok(relative) => (git_dir.join(relative.trim()), true),
        Err(_) => (git_dir, false),
    };
    let label = read_small(&common.join("config"), MAX_GIT_CONFIG)
        .ok()
        .and_then(|config| origin_url(&config))
        .and_then(|url| github::slug(&url))
        .map(|(owner, repo)| format!("{owner}/{repo}"))
        .or_else(|| {
            if linked {
                repository_folder(&common)
            } else {
                top.file_name().map(|n| n.to_string_lossy().to_string())
            }
        });
    Some(Place {
        repos: label.into_iter().collect(),
        branch,
        workspace: None,
    })
}

/// Where a session working in `cwd` is: a hook's session, or one a driver
/// listed. The folder is read on the box and goes no further than this.
pub(crate) fn place(environment: &Environment, cwd: Option<&Path>) -> Place {
    let Some(cwd) = cwd else {
        return Place::default();
    };
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    nucleus_place(&environment.root, &cwd)
        .or_else(|| checkout_place(&cwd))
        .unwrap_or_default()
}

/// Turn one hook invocation into one spool file. Returns the file written, or
/// nothing when the hook is not one this box records.
pub fn record(
    settings: &Settings,
    event: &str,
    env: Option<&str>,
    payload: &mut dyn Read,
    now: SystemTime,
) -> Option<PathBuf> {
    record_as(settings, event, env, None, payload, now)
}

/// As [`record`], for a hook whose command names its agent with `--agent`.
///
/// Whose payload this is comes first, and nothing is guessed about it: the
/// agent the command names, else the agent of the environment it names, else
/// the default agent. Only then is the environment found, among those that
/// take that agent's hooks.
pub fn record_as(
    settings: &Settings,
    event: &str,
    env: Option<&str>,
    agent: Option<&str>,
    payload: &mut dyn Read,
    now: SystemTime,
) -> Option<PathBuf> {
    let by_flag = env.and_then(|name| settings.environment(name));
    if env.is_some() && by_flag.is_none() {
        return None;
    }
    let adapter: &dyn Agent = agent::by_name(
        agent
            .or(by_flag.map(|env| env.agent.as_str()))
            .unwrap_or(agent::DEFAULT),
    )?;
    let named = named(settings, env, adapter);
    let Lifecycle {
        event: kind,
        session_id,
        cwd,
        error_kind,
        limit_resets_at,
    } = adapter.lifecycle(event, payload)?;
    let environment = named.or_else(|| located(settings, adapter, cwd.as_deref()))?;
    let place = place(environment, cwd.as_deref());
    let draft = Draft {
        env_id: environment.name.clone(),
        session_id,
        bridge_session_id: adapter.bridge_session_id(),
        repos: place.repos,
        branch: place.branch,
        error_kind,
        limit_resets_at,
        // If this agent runs inside a driver's session, the driver lists it
        // too. The record says so, and the daemon has the driver's adapter
        // stand aside: the hook wins.
        claim: driver::hook_claim(settings, adapter.name(), cwd.as_deref(), &|name| {
            std::env::var(name).ok()
        }),
        // What a session left behind is the reporter's to say, not a hook's.
        ..Draft::default()
    };
    let record = Record {
        event: Envelope::new(kind, draft, now)?,
        at_ms: unix_ms(now),
        workspace: place.workspace,
        process: process::of_this_hook(),
    };
    let spool = Spool::new(&settings.spool_dir);
    // A spool nobody is emptying is not allowed to fill the disk.
    if spool.holds_at_least(settings.spool_max_events.saturating_mul(2)) {
        return None;
    }
    spool.write(&record).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A repository with one commit and no GitHub remote, and a linked
    /// worktree of it in a folder named for a task.
    fn linked(dir: &Path, repository: &str, bare: bool) -> PathBuf {
        let git = |at: &Path, args: &[&str]| {
            let done = std::process::Command::new("git")
                .args(args)
                .current_dir(at)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(done.status.success(), "git {args:?}");
        };
        let source = dir.join("source");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let repository = dir.join(repository);
        let from = source.to_str().unwrap();
        let to = repository.to_str().unwrap();
        if bare {
            git(dir, &["clone", "-q", "--bare", from, to]);
        } else {
            git(dir, &["clone", "-q", from, to]);
        }
        git(
            &repository,
            &[
                "remote",
                "set-url",
                "origin",
                "https://git.example.test/team/web.git",
            ],
        );
        let worktree = dir.join("worktrees").join("fix the login page");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/login",
                worktree.to_str().unwrap(),
            ],
        );
        worktree
    }

    #[test]
    fn a_linked_worktree_is_named_for_its_repository_and_never_for_its_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        // Hosted somewhere other than GitHub, so the name is a folder's.
        let worktree = linked(dir.path(), "web", false);
        let inside = worktree.join("src");
        fs::create_dir_all(&inside).unwrap();
        let place = checkout_place(&inside).unwrap();
        assert_eq!(place.repos, ["web"]);
        assert_eq!(place.branch.as_deref(), Some("task/login"));
        // The repository's own checkout is named by its folder, as before.
        let own = checkout_place(&dir.path().join("web")).unwrap();
        assert_eq!(own.repos, ["web"]);
        assert_eq!(own.branch.as_deref(), Some("main"));
    }

    #[test]
    fn a_pipe_or_a_device_planted_in_a_checkout_is_never_waited_on_or_read() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = linked(dir.path(), "web", false);
        let started = std::time::Instant::now();
        // `.git` as a pipe nobody writes to.
        let piped = dir.path().join("piped");
        fs::create_dir_all(&piped).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(piped.join(".git"))
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(checkout_place(&piped), None);
        // A worktree whose HEAD is a link to a device that never ends.
        let pointer = read_small(&worktree.join(".git"), MAX_POINTER).unwrap();
        let git_dir = PathBuf::from(pointer.trim().strip_prefix("gitdir:").unwrap().trim());
        fs::remove_file(git_dir.join("HEAD")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", git_dir.join("HEAD")).unwrap();
        let place = checkout_place(&worktree).unwrap();
        assert_eq!(place.branch, None);
        assert_eq!(place.repos, ["web"]);
        // Its repository's config as a pipe too.
        let config = dir.path().join("web").join(".git").join("config");
        fs::remove_file(&config).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&config)
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(checkout_place(&worktree).unwrap().branch, None);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_worktree_of_a_bare_repository_is_named_for_it_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = linked(dir.path(), "api.git", true);
        assert_eq!(checkout_place(&worktree).unwrap().repos, ["api"]);
        // A bare repository in a folder with no such name has no name to
        // give, and the task's is not given in its place.
        let other = tempfile::tempdir().unwrap();
        let worktree = linked(other.path(), "storage", true);
        let place = checkout_place(&worktree).unwrap();
        assert_eq!(place.repos, Vec::<String>::new());
        assert_eq!(place.branch.as_deref(), Some("task/login"));
    }

    #[test]
    fn the_folder_a_repository_lives_in_is_read_from_its_git_folder() {
        let dir = tempfile::tempdir().unwrap();
        for (git_dir, name) in [
            ("web/.git", Some("web")),
            ("api.git", Some("api")),
            ("storage", None),
        ] {
            let path = dir.path().join(git_dir);
            fs::create_dir_all(&path).unwrap();
            assert_eq!(repository_folder(&path).as_deref(), name, "{git_dir}");
        }
        assert_eq!(repository_folder(&dir.path().join("gone/.git")), None);
    }

    #[test]
    fn the_origin_url_is_read_from_a_git_config() {
        let config = "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://example.com/x.git\n\
                      [remote \"origin\"]\n\turl = git@github.com:your-org/web.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n";
        assert_eq!(
            origin_url(config).as_deref(),
            Some("git@github.com:your-org/web.git")
        );
        assert_eq!(origin_url("[core]\n\tbare = true\n"), None);
    }
}
