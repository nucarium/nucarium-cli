//! The seam between an agent's hooks and the event envelope.
//!
//! Everything that knows what a particular agent's hook payload looks like
//! lives in this file. An adapter is given the hook's event name and its
//! standard input, and answers with at most one lifecycle event built from a
//! short list of named fields. The rest of the payload — prompts, messages,
//! titles, tool input and output, transcript paths — is skipped by the parser
//! and never held.

use crate::envelope::EventType;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;
use std::io::Read;
use std::path::PathBuf;

/// What a hook invocation amounts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    pub event: EventType,
    pub session_id: String,
    /// The folder the session is working in. Used on the box to find the
    /// session's repos and branch, and never sent.
    pub cwd: Option<PathBuf>,
    /// For a failure, the agent's short machine-readable kind.
    pub error_kind: Option<String>,
}

pub trait Agent {
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
    fn hooks_block(&self, env: Option<&str>) -> Value;
}

pub fn by_name(name: &str) -> Option<&'static dyn Agent> {
    match name {
        "claude-code" => Some(&ClaudeCode),
        _ => None,
    }
}

/// The payload is cut off here. A hook payload this large is a tool result,
/// and nothing this program reads is in it.
const MAX_PAYLOAD: u64 = 32 * 1024 * 1024;
/// No id or kind is longer than this; a longer string is not one of them.
const MAX_FIELD: usize = 4096;

/// Accept a short string and quietly turn anything else into nothing, so an
/// unexpected type in one field never costs the event, and content that turns
/// up where a short word was expected is never kept.
fn short_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    struct Short;

    impl<'de> Visitor<'de> for Short {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("anything")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok((value.len() <= MAX_FIELD).then(|| value.to_string()))
        }

        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(Short)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(Short)
}

/// Claude Code. Hook reference: <https://code.claude.com/docs/en/hooks>.
pub struct ClaudeCode;

/// The fields read from every Claude Code hook. Each event reads at most one
/// more, named in its own struct below. Unknown fields are skipped unread.
#[derive(Deserialize)]
struct Common {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct SessionStart {
    #[serde(flatten)]
    common: Common,
    /// `startup`, `resume`, `clear`, `compact` or `fork`.
    #[serde(default, deserialize_with = "short_string")]
    source: Option<String>,
}

#[derive(Deserialize)]
struct Notification {
    #[serde(flatten)]
    common: Common,
    #[serde(default, deserialize_with = "short_string")]
    notification_type: Option<String>,
}

#[derive(Deserialize)]
struct StopFailure {
    #[serde(flatten)]
    common: Common,
    /// A kind such as `rate_limit`. The envelope drops it unless it is one.
    #[serde(default, deserialize_with = "short_string")]
    error: Option<String>,
}

fn read<T: de::DeserializeOwned>(payload: &mut dyn Read) -> Option<T> {
    serde_json::from_reader(payload.take(MAX_PAYLOAD)).ok()
}

impl ClaudeCode {
    const COMMAND: &'static str = "nucleated hook";

    /// The events to register, each with its `Notification` matcher if any and
    /// whether it runs in the background. `SessionEnd` runs in the foreground:
    /// a background hook may not outlive the session that is ending.
    const REGISTERED: &'static [(&'static str, Option<&'static str>, bool)] = &[
        ("SessionStart", None, true),
        ("UserPromptSubmit", None, true),
        (
            "Notification",
            Some(
                "permission_prompt|idle_prompt|agent_needs_input|elicitation_dialog|\
                 elicitation_url_dialog|elicitation_complete|elicitation_response",
            ),
            true,
        ),
        ("PostToolUse", None, true),
        ("Stop", None, true),
        ("StopFailure", None, true),
        ("SessionEnd", None, false),
    ];
}

impl Agent for ClaudeCode {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn lifecycle(&self, event: &str, payload: &mut dyn Read) -> Option<Lifecycle> {
        // `SessionStart`, `session_start` and `session-start` are one event.
        let event: String = event
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let mut error_kind = None;
        let (kind, common) = match event.as_str() {
            "sessionstart" => {
                let hook: SessionStart = read(payload)?;
                // A compaction restarts the session's context, not the
                // session. Whatever the session was doing, it still is, so
                // there is nothing to report.
                if hook.source.as_deref() == Some("compact") {
                    return None;
                }
                (EventType::SessionStart, hook.common)
            }
            "userpromptsubmit" => (EventType::UserPromptSubmit, read(payload)?),
            "notification" => {
                let hook: Notification = read(payload)?;
                let kind = match hook.notification_type.as_deref()? {
                    "permission_prompt" => EventType::PermissionPrompt,
                    "idle_prompt" | "agent_needs_input" => EventType::InputPrompt,
                    "elicitation_dialog" | "elicitation_url_dialog" => EventType::ElicitationPrompt,
                    "elicitation_complete" | "elicitation_response" => EventType::Activity,
                    _ => return None,
                };
                (kind, hook.common)
            }
            // A tool ran, so the agent is working: this is how a session
            // leaves "needs input" after a permission is granted.
            "posttooluse" | "posttoolusefailure" => (EventType::Activity, read(payload)?),
            "stop" => (EventType::Stop, read(payload)?),
            "stopfailure" => {
                let hook: StopFailure = read(payload)?;
                error_kind = hook.error;
                (EventType::StopFailure, hook.common)
            }
            "sessionend" => (EventType::SessionEnd, read(payload)?),
            _ => return None,
        };
        Some(Lifecycle {
            event: kind,
            session_id: common.session_id?,
            cwd: common.cwd.map(PathBuf::from),
            error_kind,
        })
    }

    fn config_dir_var(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn bridge_session_id(&self) -> Option<String> {
        std::env::var("CLAUDE_CODE_BRIDGE_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty())
    }

    fn hooks_block(&self, env: Option<&str>) -> Value {
        let mut hooks = serde_json::Map::new();
        for (event, matcher, background) in Self::REGISTERED {
            let mut command = format!("{} {event}", Self::COMMAND);
            if let Some(env) = env {
                command.push_str(&format!(" --env {env}"));
            }
            let mut handler = json!({ "type": "command", "command": command });
            if *background {
                handler["async"] = json!(true);
            }
            handler["timeout"] = json!(5);
            let mut entry = serde_json::Map::new();
            if let Some(matcher) = matcher {
                entry.insert("matcher".to_string(), json!(matcher));
            }
            entry.insert("hooks".to_string(), json!([handler]));
            hooks.insert(event.to_string(), json!([entry]));
        }
        json!({ "hooks": hooks })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle(event: &str, payload: Value) -> Option<Lifecycle> {
        ClaudeCode.lifecycle(event, &mut payload.to_string().as_bytes())
    }

    fn kind(event: &str, payload: Value) -> Option<EventType> {
        lifecycle(event, payload).map(|l| l.event)
    }

    #[test]
    fn each_hook_event_becomes_its_lifecycle_event() {
        let base = || json!({ "session_id": "s1", "cwd": "/home/agent/work" });
        let with = |key: &str, value: &str| {
            let mut payload = base();
            payload[key] = json!(value);
            payload
        };
        assert_eq!(
            kind("SessionStart", with("source", "startup")),
            Some(EventType::SessionStart)
        );
        assert_eq!(
            kind("SessionStart", with("source", "resume")),
            Some(EventType::SessionStart)
        );
        assert_eq!(
            kind("SessionStart", with("source", "clear")),
            Some(EventType::SessionStart)
        );
        assert_eq!(kind("SessionStart", with("source", "compact")), None);
        assert_eq!(
            kind("UserPromptSubmit", base()),
            Some(EventType::UserPromptSubmit)
        );
        assert_eq!(kind("PostToolUse", base()), Some(EventType::Activity));
        assert_eq!(kind("Stop", base()), Some(EventType::Stop));
        assert_eq!(kind("SessionEnd", base()), Some(EventType::SessionEnd));
        let notification = |kind_: &str| kind("Notification", with("notification_type", kind_));
        assert_eq!(
            notification("permission_prompt"),
            Some(EventType::PermissionPrompt)
        );
        assert_eq!(notification("idle_prompt"), Some(EventType::InputPrompt));
        assert_eq!(
            notification("agent_needs_input"),
            Some(EventType::InputPrompt)
        );
        assert_eq!(
            notification("elicitation_dialog"),
            Some(EventType::ElicitationPrompt)
        );
        assert_eq!(
            notification("elicitation_complete"),
            Some(EventType::Activity)
        );
        assert_eq!(notification("auth_success"), None);
        assert_eq!(kind("Notification", base()), None);
        let failed = lifecycle("StopFailure", with("error", "rate_limit")).unwrap();
        assert_eq!(failed.event, EventType::StopFailure);
        assert_eq!(failed.error_kind.as_deref(), Some("rate_limit"));
    }

    #[test]
    fn event_names_are_matched_loosely_and_unknown_ones_do_nothing() {
        let payload = || json!({ "session_id": "s1" });
        assert_eq!(
            kind("session_start", payload()),
            Some(EventType::SessionStart)
        );
        assert_eq!(kind("session-end", payload()), Some(EventType::SessionEnd));
        assert_eq!(kind("PreToolUse", payload()), None);
        assert_eq!(kind("SubagentStop", payload()), None);
        assert_eq!(kind("", payload()), None);
    }

    #[test]
    fn a_payload_that_is_not_a_hook_does_nothing() {
        assert_eq!(kind("Stop", json!({ "cwd": "/x" })), None);
        assert_eq!(kind("Stop", json!([1, 2, 3])), None);
        assert!(ClaudeCode
            .lifecycle("Stop", &mut "not json".as_bytes())
            .is_none());
        assert!(ClaudeCode.lifecycle("Stop", &mut "".as_bytes()).is_none());
    }

    #[test]
    fn a_field_of_the_wrong_type_does_not_cost_the_event() {
        let odd = json!({
            "session_id": "s1",
            "cwd": { "path": "/x" },
            "error": { "message": "a long explanation" },
            "notification_type": ["permission_prompt"],
        });
        let failed = lifecycle("StopFailure", odd.clone()).unwrap();
        assert_eq!(failed.cwd, None);
        assert_eq!(failed.error_kind, None);
        assert_eq!(kind("Notification", odd), None);
    }

    #[test]
    fn only_a_failure_reads_the_error_field() {
        let payload = json!({ "session_id": "s1", "error": "the tool printed this" });
        assert_eq!(
            lifecycle("PostToolUseFailure", payload).unwrap().error_kind,
            None
        );
    }

    #[test]
    fn the_hooks_block_registers_the_entry_point_for_every_event() {
        let block = ClaudeCode.hooks_block(None);
        let hooks = block["hooks"].as_object().unwrap();
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "Notification",
            "PostToolUse",
            "Stop",
            "StopFailure",
            "SessionEnd",
        ] {
            let handler = &hooks[event][0]["hooks"][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["command"], format!("nucleated hook {event}"));
            assert_eq!(handler["timeout"], 5);
        }
        assert_eq!(hooks["Stop"][0]["hooks"][0]["async"], true);
        assert!(hooks["SessionEnd"][0]["hooks"][0].get("async").is_none());
        assert!(hooks["Notification"][0]["matcher"]
            .as_str()
            .unwrap()
            .contains("permission_prompt"));
        let named = ClaudeCode.hooks_block(Some("client-a"));
        assert_eq!(
            named["hooks"]["Stop"][0]["hooks"][0]["command"],
            "nucleated hook Stop --env client-a"
        );
    }
}
