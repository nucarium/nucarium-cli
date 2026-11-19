//! Codex. Hook reference: <https://learn.chatgpt.com/docs/hooks>; the payload
//! of each hook is `codex-rs/hooks/src/schema.rs` in openai/codex.
//!
//! Everything that knows what a Codex hook payload looks like is in this
//! file. Codex's hooks carry a session id and a working folder beside the
//! prompt, the last message and the tool input and output. Only the first two
//! are read, and on `SessionStart` the short word that says why it started.
//!
//! What Codex gives no hook for is not reported: a turn that fails, a usage
//! limit and when it resets, waiting on anything but an approval, when a
//! login expires, the installed version. `docs/codex.md` says where each of
//! those was looked for.

use super::{Agent, Lifecycle};
use crate::allowlist::{read_object, short_string};
use crate::envelope::EventType;
use crate::settings::Environment;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Codex;

/// The fields read from every Codex hook. Every other key is skipped unread.
///
/// A Codex hook payload is always an object, so each struct is read with
/// [`read_object`]: a derived struct alone would also take an array, field by
/// field. And the two common fields are written out in both structs, not
/// shared through `#[serde(flatten)]`, which holds the keys it does not know
/// in memory until the struct is built; nothing skipped may be held.
#[derive(Deserialize)]
struct Common {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
}

/// `SessionStart`, the one hook a third field is read from.
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

impl Codex {
    const COMMAND: &'static str = "nucleated hook";

    /// The events to register, each with its time limit in seconds and
    /// whether it runs in the background.
    ///
    /// Codex caps `Interrupt` and `SessionEnd` at three seconds and always
    /// runs `SessionEnd` in the foreground
    /// (codex-rs/hooks/src/engine/discovery.rs); the entry point gives up
    /// after two. None is given a matcher, so each runs for every tool and
    /// every source.
    ///
    /// `SessionStart` runs in the foreground. Codex runs it when a session's
    /// first turn begins, just ahead of that turn's `UserPromptSubmit`, and
    /// a session's events are put in order by the time each hook process
    /// took. Two background processes started together can take those times
    /// either way round, and the start would then land on top of the prompt
    /// and show a working session as idle. Codex waits for a hook that is
    /// not `async` before it continues what triggered it
    /// (<https://learn.chatgpt.com/docs/hooks>), so the start is recorded
    /// before the prompt's hook is started at all. The wait is the time the
    /// entry point takes to write one small file, and at most its two
    /// seconds.
    const REGISTERED: &'static [(&'static str, u64, bool)] = &[
        ("SessionStart", 5, false),
        ("UserPromptSubmit", 5, true),
        ("PermissionRequest", 5, true),
        ("PostToolUse", 5, true),
        ("Stop", 5, true),
        ("Interrupt", 3, true),
        ("SessionEnd", 3, false),
    ];
}

impl Agent for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn lifecycle(&self, event: &str, payload: &mut dyn Read) -> Option<Lifecycle> {
        // `SessionStart`, `session_start` and `session-start` are one event.
        let event: String = event
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let common = |payload: &mut dyn Read| read_object::<Common>(payload);
        let (kind, session_id, cwd) = match event.as_str() {
            "sessionstart" => {
                let hook: SessionStart = read_object(payload)?;
                // A compaction restarts the session's context, not the
                // session. Whatever the session was doing, it still is.
                if hook.source.as_deref() == Some("compact") {
                    return None;
                }
                (EventType::SessionStart, hook.session_id, hook.cwd)
            }
            "userpromptsubmit" => {
                let hook = common(payload)?;
                (EventType::UserPromptSubmit, hook.session_id, hook.cwd)
            }
            // A tool ran, so the agent is working: this is how a session
            // leaves "needs input" after an approval is given.
            "posttooluse" => {
                let hook = common(payload)?;
                (EventType::Activity, hook.session_id, hook.cwd)
            }
            // Codex is about to ask for an approval.
            "permissionrequest" => {
                let hook = common(payload)?;
                (EventType::PermissionPrompt, hook.session_id, hook.cwd)
            }
            // A turn that finished, or one the owner cut short. Either way
            // the agent is idle. Codex runs no `Stop` for an interrupted
            // turn, so without the second the session would look as if it
            // were still working.
            "stop" | "interrupt" => {
                let hook = common(payload)?;
                (EventType::Stop, hook.session_id, hook.cwd)
            }
            "sessionend" => {
                let hook = common(payload)?;
                (EventType::SessionEnd, hook.session_id, hook.cwd)
            }
            // `PreToolUse`, the compaction, subagent and model-request
            // events, and any name Codex adds later. `PostModelRequest` can
            // say an attempt failed, but an attempt is not a turn and its
            // `error` is a sentence, so it is not a failed stop.
            _ => return None,
        };
        Some(Lifecycle {
            event: kind,
            session_id: session_id?,
            cwd: cwd.map(PathBuf::from),
            // No Codex hook carries a machine-readable error kind or a time
            // a limit resets, so neither is read.
            error_kind: None,
            limit_resets_at: None,
        })
    }

    /// Codex names its config folder with `CODEX_HOME`
    /// (codex-rs/utils/home-dir), and runs a hook with the session's own
    /// environment.
    fn config_dir_var(&self) -> &'static str {
        "CODEX_HOME"
    }

    /// The hook's own `session_id` is Codex's id for the session. Codex
    /// documents no second id that its phone app could be linked by.
    fn bridge_session_id(&self) -> Option<String> {
        None
    }

    /// The whole of a Codex `hooks.json`. Codex refuses a top-level key it
    /// does not know (codex-rs/config/src/hook_config.rs), so there is only
    /// `hooks`.
    fn hooks_block(&self, env: Option<&str>) -> Value {
        let mut hooks = serde_json::Map::new();
        for (event, timeout, background) in Self::REGISTERED {
            let mut command = format!("{} {event} --agent {}", Self::COMMAND, self.name());
            if let Some(env) = env {
                command.push_str(&format!(" --env {env}"));
            }
            let mut handler = json!({ "type": "command", "command": command });
            handler["timeout"] = json!(timeout);
            if *background {
                handler["async"] = json!(true);
            }
            hooks.insert(event.to_string(), json!([{ "hooks": [handler] }]));
        }
        json!({ "hooks": hooks })
    }

    // Codex documents no expiry for a login. The one file that might hold
    // one is the credentials file itself, which this program does not read,
    // and the credentials may be in the system's keyring instead.
    fn login_expires_at(&self, _config_dir: Option<&Path>) -> Option<SystemTime> {
        None
    }

    // No file Codex documents holds the installed version: `version.json` in
    // its config folder is the newest release it has seen offered. Asking
    // would mean starting `codex`, which a heartbeat does not do.
    fn version(&self, _environment: &Environment) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle(event: &str, payload: Value) -> Option<Lifecycle> {
        Codex.lifecycle(event, &mut payload.to_string().as_bytes())
    }

    fn kind(event: &str, payload: Value) -> Option<EventType> {
        lifecycle(event, payload).map(|l| l.event)
    }

    fn base() -> Value {
        json!({ "session_id": "s1", "cwd": "/home/agent/work" })
    }

    #[test]
    fn each_hook_event_becomes_its_lifecycle_event() {
        let started = |source: &str| {
            let mut payload = base();
            payload["source"] = json!(source);
            kind("SessionStart", payload)
        };
        for source in ["startup", "resume", "clear", "fork"] {
            assert_eq!(started(source), Some(EventType::SessionStart), "{source}");
        }
        assert_eq!(started("compact"), None);
        assert_eq!(kind("SessionStart", base()), Some(EventType::SessionStart));
        assert_eq!(
            kind("UserPromptSubmit", base()),
            Some(EventType::UserPromptSubmit)
        );
        assert_eq!(kind("PostToolUse", base()), Some(EventType::Activity));
        assert_eq!(
            kind("PermissionRequest", base()),
            Some(EventType::PermissionPrompt)
        );
        assert_eq!(kind("Stop", base()), Some(EventType::Stop));
        assert_eq!(kind("Interrupt", base()), Some(EventType::Stop));
        assert_eq!(kind("SessionEnd", base()), Some(EventType::SessionEnd));
        let stopped = lifecycle("Stop", base()).unwrap();
        assert_eq!(stopped.session_id, "s1");
        assert_eq!(stopped.cwd, Some(PathBuf::from("/home/agent/work")));
    }

    #[test]
    fn the_events_that_say_nothing_about_the_session_do_nothing() {
        for event in [
            "PreToolUse",
            "PreCompact",
            "PostCompact",
            "SubagentStart",
            "SubagentStop",
            "PreModelRequest",
            "PostModelRequest",
            // Claude Code's names, which Codex does not have.
            "Notification",
            "StopFailure",
            "PostToolUseFailure",
            "",
        ] {
            assert_eq!(kind(event, base()), None, "{event}");
        }
    }

    #[test]
    fn event_names_are_matched_loosely() {
        assert_eq!(kind("session_start", base()), Some(EventType::SessionStart));
        assert_eq!(kind("session-end", base()), Some(EventType::SessionEnd));
        assert_eq!(
            kind("permission_request", base()),
            Some(EventType::PermissionPrompt)
        );
    }

    #[test]
    fn a_payload_that_is_not_a_hook_does_nothing() {
        assert_eq!(kind("Stop", json!({ "cwd": "/x" })), None);
        // A hook payload is an object: the same values in a list are not one.
        assert_eq!(kind("Stop", json!(["s1", "/home/agent/work"])), None);
        assert_eq!(
            kind("SessionStart", json!(["s1", "/home/agent/work", "startup"])),
            None
        );
        assert_eq!(kind("Stop", json!("s1")), None);
        assert!(Codex
            .lifecycle("Stop", &mut "not json".as_bytes())
            .is_none());
        assert!(Codex.lifecycle("Stop", &mut "".as_bytes()).is_none());
    }

    #[test]
    fn a_field_of_the_wrong_type_does_not_cost_the_event() {
        let odd = json!({
            "session_id": "s1",
            "cwd": { "path": "/x" },
            "source": ["compact"],
        });
        let started = lifecycle("SessionStart", odd).unwrap();
        assert_eq!(started.event, EventType::SessionStart);
        assert_eq!(started.cwd, None);
        assert_eq!(kind("Stop", json!({ "session_id": { "id": "s1" } })), None);
    }

    #[test]
    fn only_a_session_start_reads_its_source() {
        // On any other event a `source` is not a field Codex documents, so
        // it changes nothing.
        let mut payload = base();
        payload["source"] = json!("compact");
        assert_eq!(kind("Stop", payload.clone()), Some(EventType::Stop));
        assert_eq!(kind("SessionEnd", payload), Some(EventType::SessionEnd));
    }

    #[test]
    fn nothing_is_made_up_where_codex_gives_no_field() {
        // What a failed model request carries, and names a reset time or an
        // error kind might one day have: none is read.
        let payload = json!({
            "session_id": "s1",
            "status": "failed",
            "error": "You've hit your usage limit. Try again in four hours.",
            "error_kind": "usage_limit_exceeded",
            "codexErrorInfo": "usageLimitExceeded",
            "limit_resets_at": "2026-11-20T03:10:00.000Z",
            "resets_at": 1_795_144_200,
        });
        assert_eq!(kind("PostModelRequest", payload.clone()), None);
        for event in ["Stop", "Interrupt", "SessionEnd", "PostToolUse"] {
            let read = lifecycle(event, payload.clone()).unwrap();
            assert_eq!(read.error_kind, None);
            assert_eq!(read.limit_resets_at, None);
        }

        assert_eq!(Codex.bridge_session_id(), None);
        let config_dir = PathBuf::from("/home/agent/.codex-client-a");
        assert_eq!(Codex.login_expires_at(Some(&config_dir)), None);
        assert_eq!(Codex.login_expires_at(None), None);
        let environment = Environment {
            name: "client-a".into(),
            root: PathBuf::from("/home/agent/client-a"),
            unit: "codex-server@client-a.service".into(),
            check: None,
            agent: "codex".into(),
            driver: None,
            config_dir: Some(config_dir),
            github_token_file: None,
            branch_prefix: None,
        };
        assert_eq!(Codex.version(&environment), None);
    }

    #[test]
    fn a_session_start_is_waited_for_so_it_is_never_recorded_after_its_first_prompt() {
        // Codex starts the two hooks one after the other when a session's
        // first turn begins. Were both in the background, either could take
        // its time first, and the start would be applied over the prompt.
        let block = Codex.hooks_block(Some("client-a"));
        let start = &block["hooks"]["SessionStart"][0]["hooks"][0];
        assert_ne!(start["async"], true);
        // It still has to be quick: Codex is waiting.
        assert!(start["timeout"]
            .as_u64()
            .is_some_and(|seconds| seconds <= 5));
    }

    #[test]
    fn the_hooks_block_is_a_whole_hooks_file_that_names_the_agent_in_every_command() {
        let block = Codex.hooks_block(None);
        // Codex refuses a hooks file with a top-level key it does not know.
        assert_eq!(block.as_object().unwrap().len(), 1);
        let hooks = block["hooks"].as_object().unwrap();
        let registered = [
            "SessionStart",
            "UserPromptSubmit",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
            "Interrupt",
            "SessionEnd",
        ];
        assert_eq!(hooks.len(), registered.len());
        for event in registered {
            let groups = hooks[event].as_array().unwrap();
            assert_eq!(groups.len(), 1);
            // No matcher: every tool, every source.
            assert_eq!(groups[0].as_object().unwrap().len(), 1);
            let handler = &groups[0]["hooks"][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(
                handler["command"],
                format!("nucleated hook {event} --agent codex")
            );
            // Every event registered is one the adapter turns into something.
            assert!(kind(event, base()).is_some(), "{event}");
        }
        assert_eq!(hooks["Stop"][0]["hooks"][0]["async"], true);
        assert_eq!(hooks["Interrupt"][0]["hooks"][0]["async"], true);
        assert_eq!(hooks["UserPromptSubmit"][0]["hooks"][0]["async"], true);
        // Codex runs SessionEnd in the foreground whatever it is told, and
        // caps it and Interrupt at three seconds.
        assert!(hooks["SessionEnd"][0]["hooks"][0].get("async").is_none());
        for event in ["Interrupt", "SessionEnd"] {
            assert!(hooks[event][0]["hooks"][0]["timeout"].as_u64().unwrap() <= 3);
        }
        let named = Codex.hooks_block(Some("client-a"));
        assert_eq!(
            named["hooks"]["Stop"][0]["hooks"][0]["command"],
            "nucleated hook Stop --agent codex --env client-a"
        );
    }
}
