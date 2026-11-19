//! Orca.
//!
//! Everything that knows how Orca is asked, what its answer looks like and
//! where it keeps its worktrees is in this file. Each fact is from Orca's own
//! documentation or source (stablyai/orca, read at version 1.4.214) and names
//! where it was read. `docs/orca.md` is the same account for the owner.
//!
//! One command is run and no other: `orca-ide worktree ps --json`, the
//! documented way for another tool to read a running Orca
//! (docs/site/content/docs/cli/overview.mdx, cli/reference.mdx). It is never
//! given `--agent-wait`, which Orca's source calls too costly for pollers
//! (src/shared/rpc-contract/workspaces/worktree-params.ts).
//!
//! Orca offers no way to ask for state alone. The same answer carries each
//! agent's prompt, title, last reply and tool input, and each worktree's
//! terminal preview, comment and linked pull request
//! (src/shared/worktree/catalog/runtime-worktree-contracts.ts). So the answer
//! is read as it streams into the structs below, which name eleven fields,
//! and every other key is skipped unread. Each is read through
//! [`allowlist::Keyed`], from a JSON object and from nothing else: Orca's
//! answer is never an array, and one that is costs the whole look.
//!
//! Orca owns its worktrees. Housekeeping leaves everything under an `orca`
//! environment's folders alone, and the unsaved work in them is counted from
//! the folders themselves, with Orca stopped or running. Orca can keep a
//! worktree outside those folders, and its answer says where each one is:
//! while it lists one the count would leave out, no count is sent.

use super::{Condition, Driver, HookProcess, Look, Memory, Observed, Phase, LOOK_LIMIT};
use crate::allowlist::{self, flag, number, short_string, Keyed, MAX_PAYLOAD};
use crate::bounded::{self, Failure};
use crate::settings::Environment;
use crate::{agent, hook};
use serde::Deserialize;
use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

pub struct Orca;

/// The one command. Nothing else is ever passed to Orca's program.
const ARGV: [&str; 3] = ["worktree", "ps", "--json"];

/// Names Orca's data folder to its command-line program
/// (src/cli/runtime/profile/launch-user-data-path.ts).
const USER_DATA_VAR: &str = "ORCA_USER_DATA_PATH";

/// Orca's command-line program is `orca-ide` on Linux, where GNOME Orca owns
/// `orca`, and `orca` elsewhere (docs/site/content/docs/remote-servers.mdx).
fn default_cli() -> &'static str {
    if cfg!(target_os = "linux") {
        "orca-ide"
    } else {
        "orca"
    }
}

// ---- the answer, as much of it as is read ----------------------------------

/// What the program prints with `--json`: the runtime's own reply
/// (src/shared/remote-runtime/rpc/runtime-rpc-envelope.ts, printed by
/// src/cli/output/command-result-output.ts).
#[derive(Deserialize)]
struct Answer {
    #[serde(default, deserialize_with = "flag")]
    ok: Option<bool>,
    #[serde(default)]
    result: Option<Keyed<Listing>>,
}

/// `RuntimeWorktreePsResult`.
#[derive(Deserialize)]
struct Listing {
    #[serde(default, deserialize_with = "flag")]
    truncated: Option<bool>,
    #[serde(default)]
    worktrees: Option<Vec<Keyed<Worktree>>>,
}

/// `RuntimeWorktreePsSummary`: where the worktree is, whether it is a git
/// worktree, and its agents. Its id is never read: it holds a path.
#[derive(Deserialize)]
struct Worktree {
    /// Used on the box to find a repo name and a branch. Never stored.
    #[serde(default, deserialize_with = "short_string")]
    path: Option<String>,
    #[serde(default, rename = "workspaceKind", deserialize_with = "short_string")]
    kind: Option<String>,
    #[serde(default)]
    agents: Option<Vec<Keyed<Agent>>>,
}

/// `RuntimeWorktreeAgentRow`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Agent {
    #[serde(default, deserialize_with = "short_string")]
    orca_session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    state: Option<String>,
    /// Compared on the box with the two agents there is a hook adapter for.
    /// Orca lets it be any string, so it is never stored or sent.
    #[serde(default, deserialize_with = "short_string")]
    agent_type: Option<String>,
    #[serde(default, deserialize_with = "number")]
    updated_at: Option<u64>,
    #[serde(default, deserialize_with = "flag")]
    restored_unconfirmed: Option<bool>,
    #[serde(default)]
    main_agent: Option<Keyed<Lead>>,
}

/// `AgentMainAgentStatus`: the main agent's own state, apart from its
/// subagents', and how its last turn ended.
#[derive(Deserialize)]
struct Lead {
    #[serde(default, deserialize_with = "short_string")]
    state: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    outcome: Option<String>,
}

/// An Orca session id is "a plain random UUID"
/// (src/shared/agent-session/identity/orca-session-address.ts). Anything
/// else in its place is not one, and its agent is not reported.
fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// What an agent is doing, and with a failed turn the one word Orca gives
/// for how it ended.
///
/// Orca's states are `working`, `blocked`, `waiting` and `done`
/// (src/shared/agent-status/contracts/agent-status-types.ts). The main
/// agent's own state is used where the row carries one
/// (src/shared/agent-status/lifecycle/main-agent-status.ts). Orca treats
/// `blocked` and `waiting` alike as needing its owner
/// (src/shared/agent-status/lifecycle/agent-transitions.ts) and does not say
/// whether a permission or an answer is wanted, so neither is claimed here.
/// A finished turn's `outcome` is one of six words
/// (src/shared/agent-status/lifecycle/agent-turn-outcome.ts): `failure` is a
/// provider fault and `interruption` a proven unsolicited death, and the
/// rest, or none, are a turn that is simply over.
fn phase(agent: &Agent) -> (Option<Phase>, Option<String>) {
    // Orca itself is not sure of a row it restored and has not confirmed.
    if agent.restored_unconfirmed == Some(true) {
        return (None, None);
    }
    let (state, outcome) = match &agent.main_agent {
        Some(Keyed(lead)) => (lead.state.as_deref(), lead.outcome.as_deref()),
        None => (agent.state.as_deref(), None),
    };
    match (state, outcome) {
        (Some("working"), _) => (Some(Phase::Working), None),
        (Some("blocked" | "waiting"), _) => (Some(Phase::NeedsInput), None),
        (Some("done"), Some(kind @ ("failure" | "interruption"))) => {
            (Some(Phase::Failed), Some(kind.to_string()))
        }
        (Some("done"), _) => (Some(Phase::Idle), None),
        // A word this adapter does not know: no opinion.
        _ => (None, None),
    }
}

/// The agent whose hooks would report an Orca agent of this type, by its
/// config name, when this box has an adapter for it. Orca's words for the two are `claude` and
/// `codex` (src/shared/agent-launch/catalog/tui-agent.ts).
fn hooked(agent_type: Option<&str>) -> Option<&'static str> {
    let name = match agent_type {
        Some("claude") => "claude-code",
        Some("codex") => "codex",
        _ => return None,
    };
    agent::by_name(name).map(|adapter| adapter.name())
}

// ---- Orca's folders --------------------------------------------------------

/// The folders Orca keeps worktrees in: the environment's root, which is
/// Orca's workspace directory, and any `driver_dirs`.
fn bases(environment: &Environment) -> impl Iterator<Item = &Path> {
    let more = environment
        .driver
        .iter()
        .flat_map(|driver| driver.dirs.iter());
    std::iter::once(&environment.root)
        .chain(more)
        .map(PathBuf::as_path)
}

fn has_git(folder: &Path) -> bool {
    folder.join(".git").symlink_metadata().is_ok()
}

/// The top of the Orca worktree that holds `folder`, or nothing when it is
/// in none. Orca makes a worktree at `<workspace directory>/<name>`, or at
/// `<workspace directory>/<repo name>/<name>` with its `nestWorkspaces`
/// setting on (src/main/ipc/worktrees/paths/worktree-logic.ts).
fn worktree_top(environment: &Environment, folder: &Path) -> Option<PathBuf> {
    let folder = folder.canonicalize().ok()?;
    bases(environment).find_map(|base| {
        let base = base.canonicalize().ok()?;
        let mut names = folder
            .strip_prefix(&base)
            .ok()?
            .components()
            .map(|part| match part {
                Component::Normal(name) => Some(name),
                _ => None,
            });
        let first = base.join(names.next()??);
        if has_git(&first) {
            return Some(first);
        }
        let second = first.join(names.next()??);
        has_git(&second).then_some(second)
    })
}

/// The key a hook and a look agree on for one kind of agent in one worktree:
/// the worktree's number and the agent's config name. A Claude Code hook in
/// a worktree then stands in for the Claude Code row Orca lists there, and
/// not for a Codex beside it.
fn claim_key(worktree: &str, agent: &str) -> String {
    format!("{worktree}:{agent}")
}

/// A number made from the path of the Orca worktree that holds `folder`, so
/// the name its owner gave the folder is not written anywhere.
fn worktree_key(environment: &Environment, folder: &Path) -> Option<String> {
    let top = worktree_top(environment, folder)?;
    // FNV-1a, written out so the hook and the daemon always agree.
    let hash = top
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    Some(format!("wt-{hash:016x}"))
}

/// The folder to place an agent's session by, or nothing when the place it
/// would give is not surely a repo's name. A repo on GitHub is named as
/// GitHub names it, and a project's own checkout by its folder. A worktree's
/// own folder is named after the task. For a worktree of a repo hosted
/// elsewhere, the only other name on the box is that of whatever folder the
/// worktree was added from, which Orca's answer does not say is a project of
/// its own. Then nothing is said of where the agent works.
fn placeable(environment: &Environment, folder: &Path) -> Option<PathBuf> {
    let place = hook::place(environment, Some(folder));
    let named = !place.repos.is_empty()
        && (folder.join(".git").is_dir() || place.repos.iter().all(|label| label.contains('/')));
    named.then(|| folder.to_path_buf())
}

/// The directories directly inside one, or nothing when it cannot be read.
fn folders(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        // A link is not followed: what it points at is not Orca's.
        if entry.file_type().ok()?.is_dir() {
            found.push(entry.path());
        }
    }
    Some(found)
}

// ---- asking ----------------------------------------------------------------

/// The one command, for the Orca on this box and no other. Orca's program
/// reads `ORCA_` variables that send it to a paired server on another
/// machine (`ORCA_ENVIRONMENT`, `ORCA_PAIRING_CODE`, `ORCA_REMOTE_PAIRING`;
/// src/cli/index.ts, src/cli/runtime/client.ts) and others that say which of
/// Orca's own agents is calling. None the daemon happens to hold is passed
/// on; the data folder is the one the config names.
fn command(cli: &Path, home: Option<&Path>, held: impl Iterator<Item = OsString>) -> Command {
    let mut command = Command::new(cli);
    command.args(ARGV);
    for name in held {
        if name.as_encoded_bytes().starts_with(b"ORCA_") {
            command.env_remove(name);
        }
    }
    if let Some(home) = home {
        command.env(USER_DATA_VAR, home);
    }
    command
}

/// Counts what is read through it, to tell silence from nonsense.
struct Counting<'a> {
    stream: &'a mut dyn Read,
    bytes: u64,
}

impl Read for Counting<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.stream.read(buffer)?;
        self.bytes += read as u64;
        Ok(read)
    }
}

/// What one answer says of Orca's agents, or nothing when it is not Orca's
/// documented answer.
fn sessions(environment: &Environment, answer: Answer) -> Result<Look, Condition> {
    // An error reply. What it says is not read, so nothing more is known
    // than that Orca's program answered with one.
    if answer.ok != Some(true) {
        return Err(Condition::Error);
    }
    let Keyed(listing) = answer.result.ok_or(Condition::BadReply)?;
    let worktrees = listing.worktrees.ok_or(Condition::BadReply)?;
    let mut sessions = Vec::new();
    let mut listed = Vec::new();
    for Keyed(worktree) in worktrees {
        let folder = worktree
            .path
            .as_deref()
            .map(Path::new)
            .filter(|path| path.is_absolute());
        let has_git = matches!(worktree.kind.as_deref(), None | Some("git"));
        // Orca keeps a worktree wherever it was told to, and says where.
        // The seam checks each against the folders the count is made from.
        listed.extend(folder.filter(|_| has_git).map(Path::to_path_buf));
        let agents = worktree.agents.unwrap_or_default();
        if agents.is_empty() {
            continue;
        }
        // A folder workspace has no git (docs/readme/worktree-storage-maintenance.md),
        // so no repo or branch is looked for above it.
        let cwd = folder
            .filter(|_| has_git)
            .and_then(|folder| placeable(environment, folder));
        let key = folder.and_then(|folder| worktree_key(environment, folder));
        for Keyed(agent) in agents {
            let (phase, error_kind) = phase(&agent);
            // An agent Orca has not named yet is not reported until it is.
            let Some(id) = agent.orca_session_id.filter(|id| is_uuid(id)) else {
                continue;
            };
            let mut observed = Observed::new(id, phase);
            observed.error_kind = error_kind;
            // `updatedAt` moves on with each tool and prompt while the state
            // stays as it is (agent-status-types.ts).
            observed.activity = agent.updated_at;
            observed.cwd = cwd.clone();
            observed.claim_key = hooked(agent.agent_type.as_deref())
                .zip(key.as_deref())
                .map(|(name, worktree)| claim_key(worktree, name));
            sessions.push(observed);
        }
    }
    Ok(Look {
        sessions,
        // Only a list Orca says is whole can show that a session is gone.
        complete: listing.truncated == Some(false),
        link_ref: None,
        worktrees: listed,
        ..Look::default()
    })
}

impl Driver for Orca {
    fn name(&self) -> &'static str {
        "orca"
    }

    fn keys(&self) -> &'static [&'static str] {
        &["driver_cli", "driver_home", "driver_dirs"]
    }

    /// `orca serve service install` writes `orca-serve.service` as a systemd
    /// user unit (src/cli/runtime/serve/supervision/serve-service-unit.ts).
    fn default_unit(&self) -> Option<&'static str> {
        Some("orca-serve.service")
    }

    fn observe(
        &self,
        environment: &Environment,
        _previous: &Memory,
        _now: SystemTime,
    ) -> Result<Look, Condition> {
        let driver = environment.driver.as_ref().ok_or(Condition::Absent)?;
        let cli = driver
            .cli
            .clone()
            .unwrap_or_else(|| PathBuf::from(default_cli()));
        let mut command = command(
            &cli,
            driver.home.as_deref(),
            std::env::vars_os().map(|(name, _)| name),
        );
        // Whether anything was printed, to tell silence from nonsense.
        let heard = Cell::new(false);
        let answer = bounded::try_stream(&mut command, LOOK_LIMIT, MAX_PAYLOAD, |out| {
            let mut counting = Counting {
                stream: out,
                bytes: 0,
            };
            let answer = allowlist::read_object::<Answer>(&mut counting);
            heard.set(counting.bytes > 0);
            answer
        });
        match answer {
            Ok(answer) => sessions(environment, answer),
            Err(Failure::Absent) => Err(Condition::Absent),
            Err(Failure::Late) => Err(Condition::Timeout),
            // Its program ran and said it failed, whatever it printed: what
            // it does with no Orca running.
            Err(Failure::Failed) => Err(Condition::Error),
            Err(Failure::TooMuch) => Err(Condition::BadReply),
            Err(Failure::NoAnswer) if heard.get() => Err(Condition::BadReply),
            // It ran, said it succeeded, and printed nothing.
            Err(Failure::NoAnswer) => Err(Condition::Unreachable),
        }
    }

    /// Every Orca worktree is one Orca made with `git worktree add`
    /// (docs/site/content/docs/model/worktrees.mdx).
    fn owns_worktrees(&self) -> bool {
        true
    }

    /// The worktrees under Orca's folders, found by looking: each folder one
    /// or two levels down that holds a `.git`, which are Orca's two layouts.
    /// A folder with none is a folder workspace and has nothing to count.
    /// Orca is not asked, so this holds while it is stopped.
    fn worktrees(&self, environment: &Environment) -> Option<Vec<PathBuf>> {
        let mut found = Vec::new();
        for base in bases(environment) {
            for first in folders(base)? {
                if has_git(&first) {
                    found.push(first);
                    continue;
                }
                found.extend(
                    folders(&first)?
                        .into_iter()
                        .filter(|second| has_git(second)),
                );
            }
        }
        found.sort();
        found.dedup();
        Some(found)
    }

    /// A hook run from inside one of Orca's worktrees is an agent Orca also
    /// lists there, under that agent's type. Read from the hook's folder and
    /// the agent its command names.
    fn claims(&self, hook: &HookProcess) -> Option<String> {
        let worktree = worktree_key(hook.environment, hook.cwd?)?;
        Some(claim_key(&worktree, hook.agent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn agent(row: Value) -> Agent {
        let Keyed(agent) = allowlist::read(&mut row.to_string().as_bytes()).expect("a row");
        agent
    }

    fn phase_of(row: Value) -> (Option<Phase>, Option<String>) {
        phase(&agent(row))
    }

    #[test]
    fn each_of_orcas_states_is_one_phase_and_any_other_word_is_no_opinion() {
        let plain = |state: &str| phase_of(json!({ "state": state })).0;
        assert_eq!(plain("working"), Some(Phase::Working));
        assert_eq!(plain("blocked"), Some(Phase::NeedsInput));
        assert_eq!(plain("waiting"), Some(Phase::NeedsInput));
        assert_eq!(plain("done"), Some(Phase::Idle));
        assert_eq!(plain("thinking"), None);
        assert_eq!(plain("Working"), None);
        assert_eq!(phase_of(json!({})).0, None);
        assert_eq!(phase_of(json!({ "state": 3 })).0, None);
    }

    #[test]
    fn the_main_agents_own_state_is_used_where_the_row_carries_one() {
        // Subagents keep the row working after the main agent has finished.
        let settled = json!({ "state": "working", "mainAgent": { "state": "done" } });
        assert_eq!(phase_of(settled), (Some(Phase::Idle), None));
        let asking = json!({ "state": "working", "mainAgent": { "state": "waiting" } });
        assert_eq!(phase_of(asking).0, Some(Phase::NeedsInput));
        // A main agent whose state is not a known word is not spoken for
        // from the row's.
        let odd = json!({ "state": "working", "mainAgent": { "state": "napping" } });
        assert_eq!(phase_of(odd).0, None);
        assert_eq!(
            phase_of(json!({ "state": "done", "mainAgent": null })).0,
            Some(Phase::Idle)
        );
    }

    #[test]
    fn a_turn_that_failed_or_died_says_so_in_orcas_word_and_no_other_ending_does() {
        let ended = |outcome: Value| {
            phase_of(
                json!({ "state": "done", "mainAgent": { "state": "done", "outcome": outcome } }),
            )
        };
        assert_eq!(
            ended(json!("failure")),
            (Some(Phase::Failed), Some("failure".into()))
        );
        assert_eq!(
            ended(json!("interruption")),
            (Some(Phase::Failed), Some("interruption".into()))
        );
        for over in ["success", "cancellation", "superseded", "unconfirmed"] {
            assert_eq!(ended(json!(over)), (Some(Phase::Idle), None), "{over}");
        }
        // A sentence where the word belongs is not a kind, and not a failure.
        assert_eq!(
            ended(json!("it failed because the user typed this")),
            (Some(Phase::Idle), None)
        );
        assert_eq!(ended(Value::Null), (Some(Phase::Idle), None));
        // An outcome is a finished turn's. It says nothing of a working one.
        let working = json!({ "state": "working", "mainAgent": { "state": "working", "outcome": "failure" } });
        assert_eq!(phase_of(working), (Some(Phase::Working), None));
    }

    #[test]
    fn a_row_orca_restored_and_has_not_confirmed_is_given_no_opinion() {
        let row = json!({ "state": "working", "restoredUnconfirmed": true });
        assert_eq!(phase_of(row), (None, None));
        let confirmed = json!({ "state": "working", "restoredUnconfirmed": false });
        assert_eq!(phase_of(confirmed).0, Some(Phase::Working));
    }

    #[test]
    fn only_a_uuid_is_an_orca_session_id() {
        assert!(is_uuid("3540ecf3-47c3-4876-b825-81bf5d6b3595"));
        for other in [
            "",
            "fix-the-login-page",
            "3540ecf3-47c3-4876-b825-81bf5d6b359",
            "3540ecf3-47c3-4876-b825-81bf5d6b35955",
            "3540ecf3047c3-4876-b825-81bf5d6b3595",
            "3540ecf3-47c3-4876-b825-81bf5d6b359g",
            "orca_session_id:3540ecf3-47c3-4876-b825-81bf5d6b3595",
        ] {
            assert!(!is_uuid(other), "{other}");
        }
    }

    #[test]
    fn only_an_object_is_read_and_a_row_of_another_shape_is_no_answer() {
        let read = |value: Value| {
            allowlist::read::<Keyed<Answer>>(&mut value.to_string().as_bytes()).map(|read| read.0)
        };
        assert!(read(json!({ "ok": true, "result": { "worktrees": [] } })).is_some());
        // An array would fill a struct's fields in order. It is not read.
        assert!(read(json!([true, { "worktrees": [] }])).is_none());
        assert!(read(json!({ "ok": true, "result": [false, []] })).is_none());
        assert!(
            read(json!({ "ok": true, "result": { "worktrees": [["/x", "git", []]] } })).is_none()
        );
        assert!(read(json!("a sentence")).is_none());
        // An error reply has no result, and is read as one.
        let refused = read(json!({ "ok": false, "error": { "code": "x", "message": "y" } }));
        assert!(refused.is_some_and(|answer| answer.ok == Some(false) && answer.result.is_none()));
    }

    #[test]
    fn the_command_is_the_one_fixed_command_for_this_boxs_orca() {
        let held = [
            "PATH",
            "HOME",
            "ORCA_ENVIRONMENT",
            "ORCA_PAIRING_CODE",
            "ORCA_REMOTE_PAIRING",
            "ORCA_PANE_KEY",
            "ORCA_USER_DATA_PATH",
            "NOT_ORCA_ENVIRONMENT",
        ]
        .map(OsString::from);
        let built = command(
            Path::new("/opt/orca-server/bin/orca-ide"),
            Some(Path::new("/home/agent/.config/orca")),
            held.clone().into_iter(),
        );
        assert_eq!(built.get_program(), "/opt/orca-server/bin/orca-ide");
        assert_eq!(
            built.get_args().collect::<Vec<_>>(),
            ["worktree", "ps", "--json"]
        );
        let changed: Vec<(String, Option<String>)> = built
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().to_string(),
                    value.map(|value| value.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert_eq!(
            changed,
            [
                ("ORCA_ENVIRONMENT".to_string(), None),
                ("ORCA_PAIRING_CODE".to_string(), None),
                ("ORCA_PANE_KEY".to_string(), None),
                ("ORCA_REMOTE_PAIRING".to_string(), None),
                (
                    "ORCA_USER_DATA_PATH".to_string(),
                    Some("/home/agent/.config/orca".to_string())
                ),
            ]
        );
        // With no data folder named, Orca's own default is left to it, and a
        // folder the daemon happens to hold is still not passed on.
        let plain = command(Path::new("orca-ide"), None, held.into_iter());
        assert!(plain
            .get_envs()
            .all(|(name, value)| name.as_encoded_bytes().starts_with(b"ORCA_") && value.is_none()));
        assert_eq!(plain.get_envs().count(), 5);
    }
}
