//! Claude Code. Hook reference: <https://code.claude.com/docs/en/hooks>.
//!
//! Everything that knows what a Claude Code hook payload looks like is in
//! this file.

use super::{Agent, Lifecycle};
use crate::allowlist::{name_source, read_object, session_name, short_string};
use crate::envelope::EventType;
use crate::settings::Environment;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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
    /// "The session's custom title, present when one is set". Read only when
    /// the box sends names; otherwise skipped unread, as every other key is.
    #[serde(default, deserialize_with = "session_name")]
    session_title: Option<String>,
}

/// `UserPromptSubmit`: the common fields and, as on `SessionStart`, the
/// session's custom title. Its `prompt` is never read.
#[derive(Deserialize)]
struct UserPrompt {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "session_name")]
    session_title: Option<String>,
}

/// `Stop`: the common fields and, on a box that sends names, where the
/// session's transcript is. A title the app running the session gives it
/// during its first turn comes after that turn's `UserPromptSubmit`, and no
/// hook carries it until the next prompt, so at the end of a turn it is looked
/// up in the transcript. Off, the path is skipped unread.
#[derive(Deserialize)]
struct Stop {
    #[serde(default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "name_source")]
    transcript_path: Option<String>,
}

/// A transcript entry, as far as finding the session's title goes. Every
/// other key of the entry is skipped unread.
#[derive(Deserialize)]
struct TitleEntry {
    #[serde(rename = "type", default, deserialize_with = "short_string")]
    kind: Option<String>,
    #[serde(rename = "sessionId", default, deserialize_with = "short_string")]
    session_id: Option<String>,
    #[serde(rename = "customTitle", default, deserialize_with = "session_name")]
    custom_title: Option<String>,
}

/// How much of the end of a transcript is searched for the session's title.
/// Claude Code writes the title again as the session goes on, so the latest
/// is near the end: in 400 real transcripts it was never 3 MB from it.
const TITLE_SEARCH: u64 = 4 * 1024 * 1024;

/// The title the transcript at `path` last gives the session `session_id`:
/// the latest `custom-title` entry written for that session, and not one a
/// forked or resumed session copied from the session before it. Each line is
/// in memory only while it is checked for the entry's type; one that does not
/// name it is dropped undecoded, and one that does is parsed for three keys.
/// Nothing but the title is kept.
fn transcript_title(path: &Path, session_id: &str) -> Option<String> {
    const NEEDLE: &[u8] = b"\"custom-title\"";
    // Only a file: opening a pipe or a device could wait for ever.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(TITLE_SEARCH);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut title = None;
    for line in BufReader::new(file.take(TITLE_SEARCH)).split(b'\n') {
        let Ok(line) = line else { break };
        if !line.windows(NEEDLE.len()).any(|bytes| bytes == NEEDLE) {
            continue;
        }
        let Some(entry) = read_object::<TitleEntry>(&mut line.as_slice()) else {
            continue;
        };
        if entry.kind.as_deref() == Some("custom-title")
            && entry.session_id.as_deref() == Some(session_id)
        {
            if let Some(name) = entry.custom_title {
                title = Some(name);
            }
        }
    }
    title
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
        let mut name = None;
        let (kind, (session_id, cwd)) = match event.as_str() {
            "sessionstart" => {
                let hook: SessionStart = read_object(payload)?;
                // A compaction restarts the session's context, not the
                // session. Whatever the session was doing, it still is, so
                // there is nothing to report.
                if hook.source.as_deref() == Some("compact") {
                    return None;
                }
                name = hook.session_title;
                (EventType::SessionStart, (hook.session_id, hook.cwd))
            }
            "userpromptsubmit" => {
                let hook: UserPrompt = read_object(payload)?;
                name = hook.session_title;
                (EventType::UserPromptSubmit, (hook.session_id, hook.cwd))
            }
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
            "stop" => {
                let hook: Stop = read_object(payload)?;
                name = hook
                    .transcript_path
                    .as_deref()
                    .zip(hook.session_id.as_deref())
                    .and_then(|(path, id)| transcript_title(Path::new(path), id));
                (EventType::Stop, (hook.session_id, hook.cwd))
            }
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
            session_name: name,
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

    #[test]
    fn the_session_title_is_read_only_when_the_box_sends_names() {
        use crate::allowlist::with_names;
        let titled = |source: Option<&str>| {
            let mut payload = json!({
                "session_id": "s1",
                "session_title": "Speed up the\nlogin page",
                "prompt": "rewrite the billing module",
            });
            if let Some(source) = source {
                payload["source"] = json!(source);
            }
            payload
        };
        let name = |on: bool, event: &str, payload: Value| {
            with_names(on, || lifecycle(event, payload)).and_then(|l| l.session_name)
        };
        // Off, which is the default, no title is read on any event.
        for event in ["SessionStart", "UserPromptSubmit", "Stop"] {
            assert_eq!(name(false, event, titled(Some("startup"))), None, "{event}");
            assert_eq!(
                lifecycle(event, titled(Some("startup")))
                    .unwrap()
                    .session_name,
                None
            );
        }
        // On, the two events that carry it give it, made to fit.
        for (event, source) in [("SessionStart", Some("resume")), ("UserPromptSubmit", None)] {
            assert_eq!(
                name(true, event, titled(source)).as_deref(),
                Some("Speed up the login page"),
                "{event}"
            );
        }
        // Every other event reads no title, even on.
        for event in ["Stop", "PostToolUse", "SessionEnd", "StopFailure"] {
            assert_eq!(name(true, event, titled(None)), None, "{event}");
        }
        // A title that is not a string, or is nothing once made to fit, is no
        // name and costs the event nothing.
        for odd in [
            json!({ "text": "a title" }),
            json!(["a title"]),
            json!(7),
            json!(" \n "),
        ] {
            let payload = json!({ "session_id": "s1", "session_title": odd });
            let read = with_names(true, || lifecycle("UserPromptSubmit", payload)).unwrap();
            assert_eq!(read.event, EventType::UserPromptSubmit);
            assert_eq!(read.session_name, None, "{odd}");
        }
        // A session with no title has none.
        assert_eq!(
            name(true, "SessionStart", json!({ "session_id": "s1" })),
            None
        );
    }

    /// A transcript as Claude Code writes one, with the session's title given
    /// twice, among lines that look like a title entry and are not one.
    fn transcript(dir: &Path, lines: &[Value]) -> PathBuf {
        let path = dir.join("s1.jsonl");
        let mut text = String::new();
        for line in lines {
            text.push_str(&line.to_string());
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    fn stop(on: bool, path: &Path) -> Option<String> {
        let payload = json!({ "session_id": "s1", "transcript_path": path });
        crate::allowlist::with_names(on, || lifecycle("Stop", payload))
            .unwrap()
            .session_name
    }

    #[test]
    fn the_end_of_a_turn_reads_the_title_the_app_gave_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(
            dir.path(),
            &[
                // A forked session's transcript starts with its parent's lines.
                json!({ "type": "custom-title", "customTitle": "The parent", "sessionId": "s0" }),
                json!({ "type": "user", "sessionId": "s1",
                        "message": { "content": "call it \"custom-title\" please" } }),
                json!({ "type": "custom-title", "customTitle": "A first try", "sessionId": "s1" }),
                // A tool's input shaped like a title entry is inside a message,
                // not an entry of its own.
                json!({ "type": "assistant", "sessionId": "s1", "message": { "content": [
                    { "type": "tool_use", "input": { "type": "custom-title", "customTitle": "Not this", "sessionId": "s1" } }
                ] } }),
                json!({ "type": "custom-title", "customTitle": "Speed up the\nlogin  page", "sessionId": "s1" }),
                json!({ "type": "custom-title", "customTitle": { "text": "odd" }, "sessionId": "s1" }),
                json!({ "type": "last-prompt", "sessionId": "s1", "lastPrompt": "custom-title" }),
            ],
        );
        // The latest title written for this session, made to fit.
        assert_eq!(
            stop(true, &path).as_deref(),
            Some("Speed up the login page")
        );
        // Off, the path is not read, so no title is.
        assert_eq!(stop(false, &path), None);
        assert_eq!(
            lifecycle(
                "Stop",
                json!({ "session_id": "s1", "transcript_path": path })
            )
            .unwrap()
            .session_name,
            None
        );
    }

    #[test]
    fn a_transcript_with_no_title_of_its_own_gives_none_and_costs_the_stop_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let untitled = transcript(
            dir.path(),
            &[
                json!({ "type": "user", "sessionId": "s1", "message": { "content": "hello" } }),
                json!({ "type": "custom-title", "customTitle": "The parent", "sessionId": "s0" }),
                json!({ "type": "custom-title", "customTitle": "No session" }),
            ],
        );
        for path in [
            untitled,
            dir.path().join("not there.jsonl"),
            // A folder, not a transcript.
            dir.path().to_path_buf(),
        ] {
            let payload = json!({ "session_id": "s1", "transcript_path": path });
            let read = crate::allowlist::with_names(true, || lifecycle("Stop", payload)).unwrap();
            assert_eq!(read.event, EventType::Stop, "{}", path.display());
            assert_eq!(read.session_name, None, "{}", path.display());
        }
        // A path that is not a string is no path.
        let payload = json!({ "session_id": "s1", "transcript_path": ["s1.jsonl"] });
        let read = crate::allowlist::with_names(true, || lifecycle("Stop", payload)).unwrap();
        assert_eq!(read.session_name, None);
    }

    #[test]
    fn only_the_end_of_a_long_transcript_is_searched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let mut text =
            json!({ "type": "custom-title", "customTitle": "Long ago", "sessionId": "s1" })
                .to_string();
        text.push('\n');
        let filler = json!({ "type": "user", "sessionId": "s1", "message": { "content": "x".repeat(1000) } });
        while (text.len() as u64) <= TITLE_SEARCH {
            text.push_str(&filler.to_string());
            text.push('\n');
        }
        std::fs::write(&path, &text).unwrap();
        assert_eq!(stop(true, &path), None);
        let recent = json!({ "type": "custom-title", "customTitle": "Lately", "sessionId": "s1" });
        text.push_str(&recent.to_string());
        text.push('\n');
        std::fs::write(&path, &text).unwrap();
        assert_eq!(stop(true, &path).as_deref(), Some("Lately"));
    }
}
