//! Claude Code. Hook reference: <https://code.claude.com/docs/en/hooks>.
//!
//! Everything that knows what a Claude Code hook payload looks like is in
//! this file.

use super::{Agent, Lifecycle};
use crate::allowlist::{read_object, short_string};
use crate::envelope::EventType;
use crate::settings::Environment;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct ClaudeCode;

/// The fields read from every Claude Code hook. Each event reads at most one
/// more, named in a struct of its own below. Unknown fields are skipped
/// unread.
///
/// The two common fields are written out in each struct, not shared through
/// `#[serde(flatten)]`: that would hold every key of the payload it does not
/// know, and the value under it, until the struct was built. And each is read
/// with [`read_object`], because a hook payload is always an object.
#[derive(Deserialize)]
struct Common {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct SessionStart {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
    /// `startup`, `resume`, `clear`, `compact` or `fork`.
    #[serde(default, deserialize_with = "short_string")]
    source: Option<String>,
}

#[derive(Deserialize)]
struct Notification {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    notification_type: Option<String>,
}

#[derive(Deserialize)]
struct StopFailure {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
    /// A kind such as `rate_limit`. The envelope drops it unless it is one.
    #[serde(default, deserialize_with = "short_string")]
    error: Option<String>,
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
        let common = |payload: &mut dyn Read| {
            read_object::<Common>(payload).map(|hook| (hook.session_id, hook.cwd))
        };
        let mut error_kind = None;
        let (kind, (session_id, cwd)) = match event.as_str() {
            "sessionstart" => {
                let hook: SessionStart = read_object(payload)?;
                // A compaction restarts the session's context, not the
                // session. Whatever the session was doing, it still is, so
                // there is nothing to report.
                if hook.source.as_deref() == Some("compact") {
                    return None;
                }
                (EventType::SessionStart, (hook.session_id, hook.cwd))
            }
            "userpromptsubmit" => (EventType::UserPromptSubmit, common(payload)?),
            "notification" => {
                let hook: Notification = read_object(payload)?;
                let kind = match hook.notification_type.as_deref()? {
                    "permission_prompt" => EventType::PermissionPrompt,
                    "idle_prompt" | "agent_needs_input" => EventType::InputPrompt,
                    "elicitation_dialog" | "elicitation_url_dialog" => EventType::ElicitationPrompt,
                    "elicitation_complete" | "elicitation_response" => EventType::Activity,
                    _ => return None,
                };
                (kind, (hook.session_id, hook.cwd))
            }
            // A tool ran, so the agent is working: this is how a session
            // leaves "needs input" after a permission is granted.
            "posttooluse" | "posttoolusefailure" => (EventType::Activity, common(payload)?),
            "stop" => (EventType::Stop, common(payload)?),
            "stopfailure" => {
                let hook: StopFailure = read_object(payload)?;
                error_kind = hook.error;
                (EventType::StopFailure, (hook.session_id, hook.cwd))
            }
            "sessionend" => (EventType::SessionEnd, common(payload)?),
            _ => return None,
        };
        Some(Lifecycle {
            event: kind,
            session_id: session_id?,
            cwd: cwd.map(PathBuf::from),
            error_kind,
            // The hook reference names no field for when a limit resets, so
            // none is read.
            limit_resets_at: None,
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

    // Claude Code documents neither a credentials file to read an expiry from
    // nor a way to ask a running agent server its version. Until it does,
    // the box says nothing and Watch shows nothing.
    fn login_expires_at(&self, _config_dir: Option<&Path>) -> Option<SystemTime> {
        None
    }

    fn version(&self, _environment: &Environment) -> Option<String> {
        None
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
        // The right values in the order of the fields are still not a hook:
        // a payload is an object, and an array is never lined up with one.
        assert_eq!(kind("Stop", json!(["s1", "/home/agent/work"])), None);
        assert_eq!(
            kind("SessionStart", json!(["s1", "/home/agent/work", "startup"])),
            None
        );
        assert_eq!(
            kind("Notification", json!(["s1", "/x", "permission_prompt"])),
            None
        );
        assert_eq!(kind("StopFailure", json!(["s1", "/x", "rate_limit"])), None);
        assert_eq!(kind("Stop", json!("s1")), None);
        assert_eq!(kind("Stop", json!(null)), None);
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
    fn nothing_is_made_up_where_the_agent_gives_no_field() {
        // Fields named as a reset time might be are not ones the hook
        // reference documents, so they are not read.
        let payload = json!({
            "session_id": "s1",
            "error": "rate_limit",
            "limit_resets_at": "2026-11-20T03:10:00.000Z",
            "resets_at": 1_795_144_200,
            "retry_after": "in four hours",
        });
        let failed = lifecycle("StopFailure", payload).unwrap();
        assert_eq!(failed.error_kind.as_deref(), Some("rate_limit"));
        assert_eq!(failed.limit_resets_at, None);

        let config_dir = PathBuf::from("/home/agent/.claude-client-a");
        assert_eq!(ClaudeCode.login_expires_at(Some(&config_dir)), None);
        assert_eq!(ClaudeCode.login_expires_at(None), None);
        let environment = Environment {
            name: "client-a".into(),
            root: PathBuf::from("/home/agent/client-a"),
            unit: "agent-server@client-a.service".into(),
            check: None,
            agent: "claude-code".into(),
            driver: None,
            config_dir: Some(config_dir),
            github_token_file: None,
            branch_prefix: None,
        };
        assert_eq!(ClaudeCode.version(&environment), None);
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
