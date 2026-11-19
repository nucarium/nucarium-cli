//! `nucleated hook <event>`: the entry point an agent's hooks call.
//!
//! It reads the hook's JSON on standard input, keeps the handful of named
//! fields the adapter allows, and writes one small file to the spool. It
//! prints nothing, needs neither the daemon nor the network, and always exits
//! 0 within two seconds: nothing here may ever hold up a session.

use crate::agent::{self, Agent, Lifecycle};
use crate::envelope::{unix_ms, Draft, Envelope};
use crate::github;
use crate::settings::{self, Environment, Settings};
use crate::spool::{Record, Spool};
use crate::workspace::Manifest;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long the entry point may take before it gives up.
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(2);

/// Run the hook entry point. Never fails, never prints, never takes long.
pub fn run(event: &str, env: Option<&str>) {
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
            let _ = record(
                &settings,
                event,
                env,
                &mut std::io::stdin().lock(),
                SystemTime::now(),
            );
        }
    });
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolve(a) == resolve(b)
}

/// The environment a hook names outright: `--env`, else the one whose
/// `config_dir` is the agent's config folder for this process.
fn named<'a>(settings: &'a Settings, flag: Option<&str>) -> Option<&'a Environment> {
    if let Some(name) = flag {
        return settings.environment(name);
    }
    settings.environments.iter().find(|env| {
        let Some(config_dir) = &env.config_dir else {
            return false;
        };
        agent::by_name(&env.agent)
            .and_then(|agent| std::env::var_os(agent.config_dir_var()))
            .is_some_and(|actual| same_dir(Path::new(&actual), config_dir))
    })
}

/// The environment a hook belongs to by where it is running: the one whose
/// nucleus holds the session's folder, else the only one there is.
fn located<'a>(settings: &'a Settings, cwd: Option<&Path>) -> Option<&'a Environment> {
    let inside = cwd.and_then(|cwd| {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut holding: Vec<(usize, &Environment)> = settings
            .environments
            .iter()
            .filter_map(|env| {
                let root = env.root.canonicalize().ok()?;
                cwd.starts_with(&root)
                    .then_some((root.components().count(), env))
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
    inside.or(match settings.environments.as_slice() {
        [only] => Some(only),
        _ => None,
    })
}

/// Where a session is working: its repos, its branch, and its nucleus session
/// folder if it has one.
#[derive(Debug, Default, PartialEq, Eq)]
struct Place {
    repos: Vec<String>,
    branch: Option<String>,
    workspace: Option<String>,
}

/// A nucleus session: the manifest names its repos and its branch.
fn nucleus_place(root: &Path, cwd: &Path) -> Option<Place> {
    let sessions = root.canonicalize().ok()?.join(".worktrees");
    let session = cwd
        .ancestors()
        .find(|dir| dir.parent() == Some(sessions.as_path()))?;
    let manifest: Manifest =
        serde_json::from_str(&fs::read_to_string(session.join(".workspace.json")).ok()?).ok()?;
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

/// Any other checkout: read the branch and the remote from git's own files,
/// without running git, so this cannot hang.
fn checkout_place(cwd: &Path) -> Option<Place> {
    let top = cwd.ancestors().find(|dir| dir.join(".git").exists())?;
    let dot_git = top.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree: `.git` is a file pointing at its git folder.
        let pointer = fs::read_to_string(&dot_git).ok()?;
        top.join(pointer.trim().strip_prefix("gitdir:")?.trim())
    };
    let branch = fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
        });
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(relative) => git_dir.join(relative.trim()),
        Err(_) => git_dir,
    };
    let label = fs::read_to_string(common.join("config"))
        .ok()
        .and_then(|config| origin_url(&config))
        .and_then(|url| github::slug(&url))
        .map(|(owner, repo)| format!("{owner}/{repo}"))
        .or_else(|| top.file_name().map(|n| n.to_string_lossy().to_string()));
    Some(Place {
        repos: label.into_iter().collect(),
        branch,
        workspace: None,
    })
}

fn place(environment: &Environment, cwd: Option<&Path>) -> Place {
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
    let named = named(settings, env);
    if env.is_some() && named.is_none() {
        return None;
    }
    let adapter: &dyn Agent = agent::by_name(
        named
            .or(settings.environments.first())
            .map(|env| env.agent.as_str())?,
    )?;
    let Lifecycle {
        event: kind,
        session_id,
        cwd,
        error_kind,
    } = adapter.lifecycle(event, payload)?;
    let environment = named.or_else(|| located(settings, cwd.as_deref()))?;
    let place = place(environment, cwd.as_deref());
    let draft = Draft {
        env_id: environment.name.clone(),
        session_id,
        bridge_session_id: adapter.bridge_session_id(),
        repos: place.repos,
        branch: place.branch,
        error_kind,
    };
    let record = Record {
        event: Envelope::new(kind, draft, now)?,
        at_ms: unix_ms(now),
        workspace: place.workspace,
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
