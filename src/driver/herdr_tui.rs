//! Herdr TUI, the original Herdr: a terminal app with a server of its own.
//!
//! Everything that knows what a Herdr TUI socket's answer looks like is in
//! this file. How a socket is asked, and the one line this program writes to
//! this one, are beside herdr-core's in `herdr_core.rs`.
//!
//! The TUI answers `agent.list` on a Unix socket of the agent user's own,
//! one JSON message per line (Herdr's `socket-api` page and
//! `src/api/schema/agents.rs`, read at 0.9.5). Each agent carries one of
//! five status words. Herdr works most of them out by reading the bottom of
//! the agent's screen, and says of itself that a prompt it does not
//! recognise can read as idle; this adapter passes the word on as it is.
//!
//! As with herdr-core, the socket could do far more than list, and nothing
//! but [`super::herdr_core::AGENT_LIST`] is ever written to it. The answer
//! carries titles, names, labels and tokens beside the state, all skipped
//! unread.

use super::herdr_core::{ask, folder, own_var, same_socket, Call};
use super::{Condition, Driver, HookProcess, Look, Memory, Observed, Phase};
use crate::allowlist::{self, number, session_name, short_string, Nested};
use crate::envelope::valid_session_id;
use crate::paths;
use crate::settings::Environment;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// One line from the socket: `{"id", "result"}` or `{"id", "error"}`.
#[derive(Deserialize)]
struct Reply {
    /// Whether the answer is an error. What the error says is not read.
    #[serde(default, deserialize_with = "allowlist::present")]
    error: bool,
    #[serde(default)]
    result: Nested<Listing>,
}

#[derive(Deserialize)]
struct Listing {
    /// `agent_list` for the answer to `agent.list`.
    #[serde(default, rename = "type", deserialize_with = "short_string")]
    kind: Option<String>,
    #[serde(default)]
    agents: Option<Vec<Nested<Agent>>>,
}

/// The fields of an `AgentInfo` this program takes, and its `title` only
/// when the box sends names. Every other key of the record is skipped
/// unread: name, agent, terminal_title,
/// terminal_title_stripped, display_agent, state_labels, tokens and the
/// rest.
#[derive(Deserialize)]
struct Agent {
    /// Herdr's own opaque id for the terminal, `term_…`.
    #[serde(default, deserialize_with = "short_string")]
    terminal_id: Option<String>,
    /// Where the terminal is shown, such as `w1:p1`.
    #[serde(default, deserialize_with = "short_string")]
    pane_id: Option<String>,
    /// idle, working, blocked, done or unknown.
    #[serde(default, deserialize_with = "short_string")]
    agent_status: Option<String>,
    /// Set while "the current idle transition completed work".
    #[serde(default, deserialize_with = "number")]
    completion_seq: Option<u64>,
    /// The title an integration reported for what the agent is working on,
    /// such as "Refactor auth middleware". Read only when the box sends
    /// names; otherwise skipped unread.
    #[serde(default, deserialize_with = "session_name")]
    title: Option<String>,
    #[serde(default)]
    agent_session: Nested<AgentSession>,
    /// Box only.
    #[serde(default, deserialize_with = "short_string")]
    foreground_cwd: Option<String>,
    /// Box only.
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
}

/// The agent's own session, as its integration told Herdr, less the names
/// of the integration and the agent.
#[derive(Deserialize)]
struct AgentSession {
    /// "id" or "path". A path is never taken.
    #[serde(default, deserialize_with = "short_string")]
    kind: Option<String>,
    /// Compared on the box with the ids hooks have reported, and no more.
    #[serde(default, deserialize_with = "short_string")]
    value: Option<String>,
}

/// One of Herdr's own ids: a terminal's, or a pane's.
fn herdr_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'-'))
}

/// What Herdr's word for an agent says it is doing. Nothing for `unknown`,
/// which is Herdr not being sure, and for a word this file does not know.
fn phase(status: &str) -> Option<Phase> {
    match status {
        // The two differ only in whether a person has looked since.
        "idle" | "done" => Some(Phase::Idle),
        "working" => Some(Phase::Working),
        // Herdr says the agent needs "input, approval, or a decision" and
        // not which of them.
        "blocked" => Some(Phase::NeedsInput),
        _ => None,
    }
}

fn observed(agent: &Agent) -> Option<Observed> {
    let terminal = agent.terminal_id.as_deref().filter(|id| herdr_id(id))?;
    let mut session = Observed::new(
        format!("herdr:{terminal}"),
        agent.agent_status.as_deref().and_then(phase),
    );
    session.turn = agent.completion_seq;
    session.name = agent.title.clone();
    session.cwd = folder(agent.foreground_cwd.as_ref(), agent.cwd.as_ref());
    session.claim_key = agent.pane_id.clone().filter(|id| herdr_id(id));
    session.agent_session_id = agent.agent_session.get().and_then(|theirs| {
        (theirs.kind.as_deref() == Some("id"))
            .then(|| theirs.value.clone())
            .flatten()
            .filter(|id| valid_session_id(id))
    });
    Some(session)
}

/// Where Herdr puts a session's socket (`socket-api`, "Socket paths";
/// `src/session.rs`): `herdr.sock` in its config folder, or in
/// `sessions/<name>` there for a named session.
fn default_socket(config_home: PathBuf, session: Option<&str>) -> PathBuf {
    let herdr = config_home.join("herdr");
    match session {
        Some(session) => herdr.join("sessions").join(session),
        None => herdr,
    }
    .join("herdr.sock")
}

/// The socket an environment's Herdr server is asked on: the config's, else
/// where Herdr itself puts it. Herdr's own `HERDR_SOCKET_PATH` and
/// `HERDR_SESSION` are not read here; an owner who uses either says so in
/// `driver_socket` or `driver_session`.
fn socket(environment: &Environment) -> Option<PathBuf> {
    let driver = environment.driver.as_ref()?;
    if let Some(socket) = &driver.socket {
        return Some(socket.clone());
    }
    let config_home = own_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(paths::home()?.join(".config")))?;
    Some(default_socket(config_home, driver.session.as_deref()))
}

pub struct HerdrTui;

impl Driver for HerdrTui {
    fn name(&self) -> &'static str {
        "herdr-tui"
    }

    fn keys(&self) -> &'static [&'static str] {
        &["driver_socket", "driver_session"]
    }

    fn default_poll(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn min_poll(&self) -> Duration {
        Duration::from_secs(2)
    }

    fn observe(
        &self,
        environment: &Environment,
        _previous: &Memory,
        _now: SystemTime,
    ) -> Result<Look, Condition> {
        let socket = socket(environment).ok_or(Condition::Absent)?;
        let reply: Reply = ask(&socket, Call::AgentList)?;
        // Herdr answered the call with an error. What it says is not read,
        // so nothing more is known than that.
        if reply.error {
            return Err(Condition::Error);
        }
        let Nested::Is(Listing {
            kind: Some(kind),
            agents: Some(agents),
        }) = reply.result
        else {
            return Err(Condition::BadReply);
        };
        if kind != "agent_list" {
            return Err(Condition::BadReply);
        }
        Ok(Look {
            sessions: agents
                .iter()
                .filter_map(|agent| observed(agent.get()?))
                .collect(),
            // The list is every pane that hosts an agent: one it does not
            // hold has gone back to its shell or closed.
            complete: true,
            // Herdr documents no address that opens one agent.
            link_ref: None,
            ..Look::default()
        })
    }

    /// Herdr gives every pane's processes `HERDR_ENV=1`, `HERDR_PANE_ID` and
    /// `HERDR_SOCKET_PATH` (`socket-api`). Pane ids are "scoped to one
    /// server", so a pane on another server's socket is another pane.
    fn claims(&self, hook: &HookProcess) -> Option<String> {
        if (hook.var)("HERDR_ENV").as_deref() != Some("1") {
            return None;
        }
        let theirs = PathBuf::from((hook.var)("HERDR_SOCKET_PATH").filter(|s| !s.is_empty())?);
        let pane = (hook.var)("HERDR_PANE_ID").filter(|id| herdr_id(id))?;
        same_socket(&theirs, &socket(hook.environment)?).then_some(pane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(value: serde_json::Value) -> Option<Observed> {
        let agent: Nested<Agent> = serde_json::from_value(value).unwrap();
        observed(agent.get()?)
    }

    #[test]
    fn each_documented_status_word_is_one_phase_and_unknown_is_no_opinion() {
        assert_eq!(phase("idle"), Some(Phase::Idle));
        assert_eq!(phase("done"), Some(Phase::Idle));
        assert_eq!(phase("working"), Some(Phase::Working));
        assert_eq!(phase("blocked"), Some(Phase::NeedsInput));
        assert_eq!(phase("unknown"), None);
        assert_eq!(phase("error"), None);
        assert_eq!(phase("waiting for the user to answer"), None);
    }

    #[test]
    fn the_socket_is_where_herdr_puts_it() {
        assert_eq!(
            default_socket("/home/agent/.config".into(), None),
            PathBuf::from("/home/agent/.config/herdr/herdr.sock")
        );
        assert_eq!(
            default_socket("/home/agent/.config".into(), Some("review")),
            PathBuf::from("/home/agent/.config/herdr/sessions/review/herdr.sock")
        );
    }

    #[test]
    fn a_session_is_named_by_herdrs_own_id_for_its_terminal() {
        let seen = agent(json!({
            "terminal_id": "term_65f1a2b3c4d5e1", "pane_id": "w1:p2", "agent_status": "idle",
            "completion_seq": 12, "state_change_seq": 40,
            "agent_session": { "source": "herdr:claude", "agent": "claude", "kind": "id",
                "value": "0b8e6a7c-1f7b-4a53-9a44-6a1f3f1f8f10" },
            "cwd": "/home/agent/started-here", "foreground_cwd": "/home/agent/now-here",
        }))
        .unwrap();
        assert_eq!(seen.id, "herdr:term_65f1a2b3c4d5e1");
        assert_eq!(seen.phase, Some(Phase::Idle));
        assert_eq!(seen.turn, Some(12));
        assert_eq!(seen.claim_key.as_deref(), Some("w1:p2"));
        assert_eq!(
            seen.agent_session_id.as_deref(),
            Some("0b8e6a7c-1f7b-4a53-9a44-6a1f3f1f8f10")
        );
        assert_eq!(seen.cwd, Some(PathBuf::from("/home/agent/now-here")));
        assert_eq!(seen.activity, None);
    }

    #[test]
    fn an_id_that_is_not_one_of_herdrs_is_not_used() {
        assert_eq!(
            agent(json!({ "terminal_id": "what the agent was asked", "agent_status": "idle" })),
            None
        );
        assert_eq!(agent(json!({ "agent_status": "idle" })), None);
        assert_eq!(
            agent(json!(["term_1", "w1:p1", "idle", 3, null, "/a", "/b"])),
            None
        );
        let odd = agent(json!({
            "terminal_id": "term_1", "pane_id": "the pane with the tests in it",
            "agent_status": "idle", "agent_session": "a sentence",
        }))
        .unwrap();
        assert_eq!(odd.claim_key, None);
        assert_eq!(odd.agent_session_id, None);
    }

    #[test]
    fn an_agents_own_session_is_taken_only_as_an_id_and_never_as_a_path() {
        let by_path = agent(json!({
            "terminal_id": "term_1", "agent_status": "working",
            "agent_session": { "kind": "path", "value": "/home/agent/.codex/sessions/one.jsonl" },
        }))
        .unwrap();
        assert_eq!(by_path.agent_session_id, None);
        let unkinded = agent(json!({
            "terminal_id": "term_1", "agent_status": "working",
            "agent_session": { "value": "abc" },
        }))
        .unwrap();
        assert_eq!(unkinded.agent_session_id, None);
        let unfit = agent(json!({
            "terminal_id": "term_1", "agent_status": "working",
            "agent_session": { "kind": "id", "value": "an id with spaces in it" },
        }))
        .unwrap();
        assert_eq!(unfit.agent_session_id, None);
    }
}
