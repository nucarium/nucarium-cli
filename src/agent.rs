//! The seam between an agent's hooks and the event envelope.
//!
//! An agent is a coding agent that calls `nucleated hook` from its own
//! lifecycle hooks. Each agent has one adapter, in a file of its own under
//! `src/agent/`, and that file holds everything that knows what the agent's
//! hook payload looks like. An adapter is given the hook's event name and its
//! standard input, and answers with at most one lifecycle event built from a
//! short list of named fields. The rest of the payload — prompts, messages,
//! titles, tool input and output, transcript paths — is skipped by the parser
//! and never held: every adapter reads through [`crate::allowlist`]. The one
//! exception is the session's own name, read only on a box whose owner turned
//! `session_names` on.
//!
//! A tool the daemon has to ask, because it calls no hook, is a driver and
//! lives behind [`crate::driver`].

use crate::envelope::EventType;
use crate::settings::Environment;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub mod claude_code;
pub mod codex;

/// What a hook invocation amounts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    pub event: EventType,
    pub session_id: String,
    /// The folder the session is working in. Used on the box to find the
    /// session's repos and branch, and never sent.
    pub cwd: Option<PathBuf>,
    /// The session's name, as the agent has it. Only ever read inside
    /// [`crate::allowlist::with_names`] with names on.
    pub session_name: Option<String>,
    /// For a failure, the agent's short machine-readable kind.
    pub error_kind: Option<String>,
    /// For a failure on a usage limit, when the agent says the limit resets.
    /// A time the adapter parsed, never the agent's words for one.
    pub limit_resets_at: Option<SystemTime>,
}

pub trait Agent: Sync {
    /// The name used for this agent in the config file.
    fn name(&self) -> &'static str;

    /// Turn one hook invocation into at most one lifecycle event.
    fn lifecycle(&self, event: &str, payload: &mut dyn Read) -> Option<Lifecycle>;

    /// The environment variable naming this agent's config folder, which tells
    /// one login's hooks from another's.
    fn config_dir_var(&self) -> &'static str;

    /// The agent vendor's own id for the session, when the agent provides one.
    fn bridge_session_id(&self) -> Option<String>;

    /// The block that registers the hook entry point in the agent's settings.
    /// An agent other than the default one writes `--agent <its name>` into
    /// each command, so the entry point never has to guess whose payload it
    /// was handed.
    fn hooks_block(&self, env: Option<&str>) -> Value;

    /// When this agent's login expires, read from its config folder, or
    /// nothing when the adapter cannot read one. An adapter answers only from
    /// a format the agent documents: a guessed time sends a false warning.
    ///
    /// Asked at every heartbeat, so it must be cheap: read a small file, start
    /// nothing, and never touch the network.
    fn login_expires_at(&self, config_dir: Option<&Path>) -> Option<SystemTime>;

    /// The version of the agent an environment runs, as the agent reports it,
    /// or nothing when the adapter cannot ask. Asked at every heartbeat, under
    /// the same rule.
    fn version(&self, environment: &Environment) -> Option<String>;
}

/// The agent an environment runs when its config names none, and the agent a
/// hook belongs to when its command line names none.
pub const DEFAULT: &str = "claude-code";

/// Every agent there is an adapter for, by its config name.
pub const NAMES: [&str; 2] = ["claude-code", "codex"];

/// The one registry of agent adapters.
pub fn by_name(name: &str) -> Option<&'static dyn Agent> {
    match name {
        "claude-code" => Some(&claude_code::ClaudeCode),
        "codex" => Some(&codex::Codex),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_in_the_registry_finds_the_adapter_that_answers_to_it() {
        for name in NAMES {
            assert_eq!(by_name(name).map(|agent| agent.name()), Some(name));
        }
        assert!(NAMES.contains(&DEFAULT));
        assert!(by_name("another").is_none());
        assert!(by_name("").is_none());
    }
}
