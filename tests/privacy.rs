//! The release gate for the privacy promise.
//!
//! Every hook event is fed a payload with a sentinel string in every
//! content-bearing field — prompt, message, title, tool input and output,
//! transcript path, file content and the rest — and in the working
//! directory's path and the process environment. The sentinel must then be
//! found nowhere: not in the spool, not in the local state, not in anything
//! the hook prints, and not in any request the daemon makes.
//!
//! The same is done for what the box finds out by itself. A session's
//! repository is given a commit subject, file names and file contents full
//! of the sentinel, and a stand-in for GitHub describes its pull request with
//! the sentinel in the title, the body, the branch and the author. What then
//! leaves is counts, times and one address, and what the box posts to its
//! owner's own address while Watch is unreachable is names and times.
//!
//! If this test fails, content can leave the box. Do not release.

mod common;

use common::gate::{self, leaks_on_disk, only_listed_fields, tick, watch, MARK};
use common::{contract, files_under, run, Mock, Reply, Request, Rig};
use nucleate::daemon::{Daemon, Timings};
use nucleate::envelope::rfc3339;
use nucleate::reporter::Reporter;
use nucleate::watch::{EnvironmentReport, Heartbeat, MachineHealth, UnsavedReport};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

// The sentinel, the lists of every field an event and a heartbeat may carry,
// the scanner and the stand-in for Watch are in `common::gate`, where each
// adapter's own gate test finds them too.

const SESSION: &str = "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a";

/// Every hook event Claude Code documents, with the values of the one short
/// field the adapter may read for it. Events the adapter ignores are here
/// too: ignoring them must not leak either.
fn events() -> Vec<(&'static str, Value)> {
    let mut events = Vec::new();
    for source in ["startup", "resume", "clear", "compact", "fork"] {
        events.push(("SessionStart", json!({ "source": source })));
    }
    for kind in [
        "permission_prompt",
        "idle_prompt",
        "agent_needs_input",
        "elicitation_dialog",
        "elicitation_url_dialog",
        "elicitation_complete",
        "elicitation_response",
        "auth_success",
        "agent_completed",
        "plugin_notification",
    ] {
        events.push(("Notification", json!({ "notification_type": kind })));
    }
    for error in [
        "rate_limit",
        "authentication_failed",
        "billing_error",
        "server_error",
        "unknown",
    ] {
        events.push(("StopFailure", json!({ "error": error })));
    }
    for reason in ["clear", "logout", "prompt_input_exit", "other"] {
        events.push(("SessionEnd", json!({ "reason": reason })));
    }
    for event in [
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PermissionDenied",
        "PostToolUse",
        "PostToolUseFailure",
        "PostToolBatch",
        "Stop",
        "SubagentStart",
        "SubagentStop",
        "Elicitation",
        "ElicitationResult",
        "PreCompact",
        "PostCompact",
        "InstructionsLoaded",
        "ConfigChange",
        "CwdChanged",
        "FileChanged",
        "TaskCreated",
        "TaskCompleted",
        "SomethingNotInventedYet",
    ] {
        events.push((event, json!({})));
    }
    events
}

/// A hook payload with the sentinel in every field that carries content, a
/// path, or anything else the box does not send.
fn payload(event: &str, cwd: &Path, allowed: &Value) -> Value {
    let mark = |field: &str| format!("{MARK} {field} the user typed this");
    let mut payload = json!({
        "session_id": SESSION,
        "prompt_id": mark("prompt_id"),
        "transcript_path": format!("/home/agent/.claude/projects/{MARK}_transcript_path/{SESSION}.jsonl"),
        "cwd": cwd,
        "scratchpad_dir": format!("/tmp/{MARK}_scratchpad_dir"),
        "permission_mode": "default",
        "hook_event_name": event,
        "agent_id": mark("agent_id"),
        "agent_type": mark("agent_type"),
        "model": mark("model"),
        "session_title": mark("session_title"),
        "prompt": mark("prompt"),
        "message": mark("message"),
        "title": mark("title"),
        "tool_name": mark("tool_name"),
        "tool_use_id": mark("tool_use_id"),
        "tool_input": {
            "command": mark("tool_input.command"),
            "file_path": format!("/home/agent/work/{MARK}_tool_input_file_path.rs"),
            "content": mark("tool_input.content: the file's content"),
            "old_string": mark("tool_input.old_string"),
            "new_string": mark("tool_input.new_string"),
            "url": format!("https://example.test/{MARK}_tool_input_url"),
        },
        "tool_response": {
            "stdout": mark("tool_response.stdout"),
            "stderr": mark("tool_response.stderr"),
            "file": { "filePath": format!("/x/{MARK}_tool_response_path"), "content": mark("tool_response.file.content") },
        },
        "permission_suggestions": [{ "type": "addRules", "rules": [{ "ruleContent": mark("permission_suggestions") }] }],
        "mcp_server": mark("mcp_server"),
        "reason": mark("reason"),
        "error": mark("error: a long explanation of what went wrong"),
        "error_details": mark("error_details"),
        "is_interrupt": false,
        "last_assistant_message": mark("last_assistant_message"),
        "stop_hook_active": false,
        "custom_instructions": mark("custom_instructions"),
        "compact_summary": mark("compact_summary"),
        "trigger": mark("trigger"),
        "file_path": format!("/home/agent/{MARK}_file_path"),
        "elicitation": { "message": mark("elicitation.message"), "requested_schema": { "title": mark("elicitation.schema") } },
        "response": { "action": "accept", "content": { "answer": mark("elicitation response") } },
        "source": mark("source"),
        "notification_type": mark("notification_type"),
        "something_added_next_year": { "deep": [mark("a field that does not exist yet")] },
    });
    // Where an agent might one day say when a limit resets, when its login
    // runs out, or which version it is. None is a field the box reads.
    for field in [
        "limit_resets_at",
        "resets_at",
        "retry_after",
        "login_expires_at",
        "expires_at",
        "agent_version",
        "version",
    ] {
        payload[field] = json!(mark(field));
    }
    // The one short field the adapter may read for this event, set to a real
    // value. Everything else keeps its sentinel.
    for (key, value) in allowed.as_object().cloned().unwrap_or_default() {
        payload[key] = value;
    }
    payload
}

/// Call the hook entry point with the sentinel in its environment as well.
fn hook(rig: &Rig, event: &str, payload: &Value) {
    let out = run(
        rig.nucleated()
            .args(["hook", event])
            .env(
                "ANTHROPIC_API_KEY",
                format!("{MARK}_api_key_in_environment"),
            )
            .env("GH_TOKEN", format!("{MARK}_github_token_in_environment"))
            .env("USER", format!("{MARK}_user"))
            .env("SSH_CONNECTION", format!("{MARK}_ip_address 22"))
            .env("CLAUDE_CODE_BRIDGE_SESSION_ID", "session_01BRIDGEid"),
        &payload.to_string(),
    );
    assert_eq!(out.status.code(), Some(0), "the hook failed for {event}");
    assert!(
        out.stdout.is_empty(),
        "the hook printed something for {event}"
    );
    assert!(out.stderr.is_empty(), "the hook complained for {event}");
}

/// A box whose nucleus sits under a folder named with the sentinel, with a
/// session working in a subfolder named with it too.
fn rig() -> (Rig, std::path::PathBuf) {
    let rig = Rig::under(&format!("{MARK}_parent_folder"));
    let worktree = rig.session("fix-login");
    let cwd = worktree.join("src").join(format!("{MARK}_subfolder"));
    std::fs::create_dir_all(&cwd).unwrap();
    (rig, cwd)
}

#[test]
fn the_sentinel_fixture_would_catch_a_leak() {
    // The gate is only worth something if its net has no holes: the payload
    // really is full of the sentinel, and the scanner really finds it.
    let fixture = payload("PostToolUse", Path::new("/x"), &json!({})).to_string();
    assert!(
        fixture.matches(MARK).count() >= 42,
        "the fixture lost its sentinels"
    );
    for field in [
        "prompt",
        "message",
        "title",
        "session_title",
        "tool_input",
        "tool_response",
        "transcript_path",
        "content",
        "last_assistant_message",
        "error_details",
        "compact_summary",
        "custom_instructions",
        "file_path",
        "limit_resets_at",
        "login_expires_at",
        "agent_version",
    ] {
        assert!(
            fixture.contains(&format!("\"{field}\"")),
            "{field} is not in the fixture"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("spool")).unwrap();
    std::fs::write(
        dir.path().join("spool/leak.json"),
        format!("{{\"x\":\"{MARK}\"}}"),
    )
    .unwrap();
    assert_eq!(leaks_on_disk(dir.path()).len(), 1);
}

#[test]
fn no_content_reaches_the_spool_the_local_state_or_the_wire() {
    let (rig, cwd) = rig();
    let mock = watch();
    rig.sign_in(&mock.url);

    for (event, allowed) in events() {
        hook(&rig, event, &payload(event, &cwd, &allowed));
    }

    // The spool, straight after the hooks ran.
    let spooled = rig.spooled();
    assert!(
        spooled.len() >= 20,
        "the hooks recorded too little to prove anything: {}",
        spooled.len()
    );
    assert_eq!(
        leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the spool"
    );

    // What was recorded is the metadata, and it is right.
    let first = &spooled[0];
    assert_eq!(first["event"]["session_id"], SESSION);
    assert_eq!(first["event"]["env_id"], "client-a");
    assert_eq!(first["event"]["repos"], json!(["web"]));
    assert_eq!(first["event"]["branch"], "workspace/fix-login");
    assert_eq!(first["event"]["bridge_session_id"], "session_01BRIDGEid");
    assert_eq!(first["workspace"], "fix-login");

    // The wire: every request the daemon makes.
    tick(&rig);
    let sent = mock.everything();
    assert!(
        mock.to("/v1/heartbeats:ingest").len() == 1 && !mock.to("/v1/events:ingest").is_empty()
    );
    assert!(!sent.contains(MARK), "content on the wire:\n{sent}");
    // Nothing sent was something Watch would refuse: no unknown field, and
    // every value inside its pattern or range.
    assert_eq!(contract::violations(&mock), Vec::<String>::new());

    // Every field sent is one the envelope and the heartbeat name.
    only_listed_fields(&mock);
    let mut types = std::collections::BTreeSet::new();
    for request in mock.to("/v1/events:ingest") {
        for event in request.json()["events"].as_array().unwrap() {
            // Claude Code's hook names no reset time, so none is sent,
            // whatever the payload held under a name like one.
            assert!(event.get("limit_resets_at").is_none());
            types.insert(event["type"].as_str().unwrap().to_string());
        }
    }
    // The whole lifecycle went through, so the proof covers every kind of event.
    for kind in [
        "session_start",
        "user_prompt_submit",
        "activity",
        "permission_prompt",
        "input_prompt",
        "elicitation_prompt",
        "stop",
        "stop_failure",
        "session_end",
    ] {
        assert!(types.contains(kind), "no {kind} event was sent");
    }
    let heartbeat = mock.to("/v1/heartbeats:ingest")[0].json();
    // A number of seconds, and nothing else.
    assert_eq!(heartbeat["activity_beat_s"], 300);
    for environment in heartbeat["environments"].as_array().unwrap() {
        // Claude Code gives the box neither to read, so neither is sent.
        assert!(environment.get("agent_version").is_none());
        assert!(environment.get("login_expires_at").is_none());
    }

    // The local state the daemon keeps: sessions, its own state, anything set aside.
    assert!(files_under(&rig.home.state_dir().join("sessions")).len() == 1);
    assert_eq!(
        leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the local state"
    );
}

/// A session's name as the owner gave it, marked so the gate can find it.
/// It is not the sentinel: on a box that sends names it is meant to leave.
const NAMED: &str = "ZZNAMEZZ Speed up the login page";

/// Every hook event, with the sentinel everywhere as above and the session's
/// custom title set to [`NAMED`] split over two lines.
fn titled_hooks(rig: &Rig, cwd: &Path) {
    for (event, allowed) in events() {
        let mut payload = payload(event, cwd, &allowed);
        payload["session_title"] = json!("ZZNAMEZZ Speed up\n the  login page ");
        hook(rig, event, &payload);
    }
}

/// Every place on the box that holds `text`.
fn held_on_disk(rig: &Rig, text: &str) -> Vec<String> {
    files_under(&rig.home.state_dir())
        .into_iter()
        .filter(|(_, bytes)| String::from_utf8_lossy(bytes).contains(text))
        .map(|(path, _)| path.display().to_string())
        .collect()
}

/// The `session_name` of every event sent, by event type.
fn names_sent(mock: &common::Mock) -> Vec<(String, Option<String>)> {
    mock.to("/v1/events:ingest")
        .iter()
        .flat_map(|request| request.json()["events"].as_array().unwrap().clone())
        .map(|event| {
            (
                event["type"].as_str().unwrap().to_string(),
                event["session_name"].as_str().map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn a_box_that_does_not_send_names_reads_none_even_for_a_watch_that_takes_them() {
    let (rig, cwd) = rig();
    let mock = gate::watch_taking(&["session_name"]);
    rig.sign_in(&mock.url);
    titled_hooks(&rig, &cwd);
    assert_eq!(held_on_disk(&rig, "ZZNAMEZZ"), Vec::<String>::new());
    tick(&rig);
    let sent = mock.everything();
    assert!(!mock.to("/v1/events:ingest").is_empty());
    assert!(!sent.contains("ZZNAMEZZ"), "a name on the wire:\n{sent}");
    assert!(!sent.contains(MARK), "content on the wire:\n{sent}");
    assert!(names_sent(&mock).iter().all(|(_, name)| name.is_none()));
    assert_eq!(held_on_disk(&rig, "ZZNAMEZZ"), Vec::<String>::new());
}

#[test]
fn a_box_that_sends_names_sends_the_session_title_and_nothing_else() {
    let (rig, cwd) = rig();
    rig.configure("session_names = true");
    let mock = gate::watch_taking(&["session_name"]);
    rig.sign_in(&mock.url);
    titled_hooks(&rig, &cwd);
    // The sentinel is still nowhere; the name waits in the spool to be sent.
    assert_eq!(
        leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the spool"
    );
    tick(&rig);
    let sent = mock.everything();
    assert!(!sent.contains(MARK), "content on the wire:\n{sent}");
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    only_listed_fields(&mock);
    // The two events whose hooks carry the title send it, made to fit, and
    // no other event carries a name.
    let names = names_sent(&mock);
    for kind in ["session_start", "user_prompt_submit"] {
        assert!(
            names.iter().any(|(sent, _)| sent == kind),
            "no {kind} event was sent"
        );
    }
    for (kind, name) in &names {
        match kind.as_str() {
            "session_start" | "user_prompt_submit" => {
                assert_eq!(name.as_deref(), Some(NAMED), "{kind}")
            }
            _ => assert_eq!(name.as_deref(), None, "{kind}"),
        }
    }
    // Once sent, the name is held nowhere on the box: not in the spool, not in
    // the session's state.
    assert_eq!(held_on_disk(&rig, "ZZNAMEZZ"), Vec::<String>::new());
}

/// A session's transcript as Claude Code writes it, in a folder named with
/// the sentinel, with the sentinel in every line but one: the entry that gives
/// the session its title, as the desktop app does after the first prompt.
fn transcript_full_of_content(dir: &Path) -> std::path::PathBuf {
    let folder = dir.join(format!("{MARK}_transcripts"));
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join(format!("{SESSION}.jsonl"));
    let mark = |what: &str| format!("{MARK} {what}");
    let lines = [
        // A forked session starts with its parent's lines, title and all.
        json!({ "type": "custom-title", "customTitle": mark("the parent's title"), "sessionId": "the-parent" }),
        json!({ "type": "user", "sessionId": SESSION, "cwd": mark("cwd"),
                "message": { "role": "user", "content": mark("the prompt \"custom-title\"") } }),
        json!({ "type": "custom-title", "customTitle": "ZZNAMEZZ Speed up\n the  login page ", "sessionId": SESSION }),
        json!({ "type": "assistant", "sessionId": SESSION, "message": { "content": [
            { "type": "text", "text": mark("the reply") },
            { "type": "tool_use", "name": mark("tool"), "input": {
                "type": "custom-title", "customTitle": mark("a tool's input"), "sessionId": SESSION } },
        ] } }),
        json!({ "type": "attachment", "sessionId": SESSION, "attachment": { "content": mark("a file") } }),
        json!({ "type": "last-prompt", "sessionId": SESSION, "lastPrompt": mark("custom-title") }),
        json!({ "type": "summary", "summary": mark("the summary"), "leafUuid": mark("uuid") }),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path
}

/// Every hook event of a first turn, before any hook carries the title, each
/// pointing at `transcript`.
fn first_turn_hooks(rig: &Rig, cwd: &Path, transcript: &Path) {
    for (event, allowed) in events() {
        let mut payload = payload(event, cwd, &allowed);
        payload.as_object_mut().unwrap().remove("session_title");
        payload["transcript_path"] = json!(transcript);
        hook(rig, event, &payload);
    }
}

#[test]
fn a_box_that_sends_names_takes_a_title_from_the_transcript_and_nothing_else() {
    let (rig, cwd) = rig();
    rig.configure("session_names = true");
    let mock = gate::watch_taking(&["session_name"]);
    rig.sign_in(&mock.url);
    let dir = tempfile::tempdir().unwrap();
    let transcript = transcript_full_of_content(dir.path());
    first_turn_hooks(&rig, &cwd, &transcript);
    assert_eq!(
        leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the spool"
    );
    tick(&rig);
    let sent = mock.everything();
    assert!(!sent.contains(MARK), "content on the wire:\n{sent}");
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    only_listed_fields(&mock);
    // The end of the turn sends the session's own title, made to fit, and no
    // event sends anything else the transcript holds.
    let names = names_sent(&mock);
    assert!(
        names.iter().any(|(sent, _)| sent == "stop"),
        "no stop event was sent"
    );
    for (kind, name) in &names {
        match kind.as_str() {
            "stop" => assert_eq!(name.as_deref(), Some(NAMED), "{kind}"),
            _ => assert_eq!(name.as_deref(), None, "{kind}"),
        }
    }
    assert_eq!(held_on_disk(&rig, "ZZNAMEZZ"), Vec::<String>::new());
}

#[test]
fn a_box_that_does_not_send_names_takes_no_title_from_the_transcript() {
    let (rig, cwd) = rig();
    let mock = gate::watch_taking(&["session_name"]);
    rig.sign_in(&mock.url);
    let dir = tempfile::tempdir().unwrap();
    let transcript = transcript_full_of_content(dir.path());
    first_turn_hooks(&rig, &cwd, &transcript);
    assert_eq!(held_on_disk(&rig, "ZZNAMEZZ"), Vec::<String>::new());
    tick(&rig);
    let sent = mock.everything();
    assert!(!mock.to("/v1/events:ingest").is_empty());
    assert!(!sent.contains("ZZNAMEZZ"), "a name on the wire:\n{sent}");
    assert!(!sent.contains(MARK), "content on the wire:\n{sent}");
}

#[test]
fn a_name_goes_only_to_a_watch_that_takes_it_and_not_after_names_are_turned_off() {
    // A Watch that does not take the field hears no name, and refuses nothing.
    let (rig, cwd) = rig();
    rig.configure("session_names = true");
    let mock = watch();
    rig.sign_in(&mock.url);
    titled_hooks(&rig, &cwd);
    tick(&rig);
    assert!(!mock.to("/v1/events:ingest").is_empty());
    assert!(!mock.everything().contains("ZZNAMEZZ"));
    assert_eq!(contract::violations(&mock), Vec::<String>::new());

    // Names spooled while the box sent them go without them once it stops.
    let (later, cwd) = self::rig();
    later.configure("session_names = true");
    let mock = gate::watch_taking(&["session_name"]);
    later.sign_in(&mock.url);
    titled_hooks(&later, &cwd);
    assert!(!held_on_disk(&later, "ZZNAMEZZ").is_empty());
    later.configure("");
    tick(&later);
    assert!(!mock.to("/v1/events:ingest").is_empty());
    assert!(!mock.everything().contains("ZZNAMEZZ"));
    assert!(names_sent(&mock).iter().all(|(_, name)| name.is_none()));
}

#[test]
fn no_content_is_kept_by_a_box_with_no_target() {
    let (rig, cwd) = rig();
    for (event, allowed) in events() {
        hook(&rig, event, &payload(event, &cwd, &allowed));
    }
    tick(&rig);
    assert!(rig.spooled().is_empty());
    assert!(!files_under(&rig.home.state_dir()).is_empty());
    assert_eq!(leaks_on_disk(&rig.home.state_dir()), Vec::<String>::new());
    // What `nucleated status` prints is built from those same files.
    let status = run(rig.nucleated().args(["status", "--json"]), "");
    assert!(status.status.success());
    let view: Value = serde_json::from_slice(&status.stdout).unwrap();
    let sessions = nucleate::sessions::Store::new(&rig.home.state_dir()).list("client-a");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, SESSION);
    let mut shown = view.clone();
    // The nucleus path is the owner's own config, shown back to them; the
    // sessions are what must be clean.
    shown["environments"] = json!([]);
    assert!(!shown.to_string().contains(MARK), "{shown}");
}

#[test]
fn content_where_a_short_word_belongs_is_dropped_not_sent() {
    let (rig, cwd) = rig();
    let mock = watch();
    rig.sign_in(&mock.url);
    let sentence = format!("{MARK} this is a sentence, not a kind");
    let base = |event: &str| payload(event, &cwd, &json!({}));

    // A session id that is not an id: no event at all.
    let mut odd = base("Stop");
    odd["session_id"] = json!(sentence);
    hook(&rig, "Stop", &odd);
    // A notification type that is not one the box knows: no event.
    hook(&rig, "Notification", &base("Notification"));
    // A session source that is not a source: still a start, and nothing kept.
    hook(&rig, "SessionStart", &base("SessionStart"));
    // An error that is a message, not a kind: the failure is recorded without it.
    hook(&rig, "StopFailure", &base("StopFailure"));
    // Fields of the wrong type altogether.
    let mut wrong = base("StopFailure");
    wrong["error"] = json!({ "message": sentence, "kind": [sentence] });
    wrong["cwd"] = json!([sentence]);
    hook(&rig, "StopFailure", &wrong);
    // A bridge session id that is not an id.
    let out = run(
        rig.nucleated()
            .args(["hook", "Stop"])
            .env("CLAUDE_CODE_BRIDGE_SESSION_ID", &sentence),
        &payload("Stop", &cwd, &json!({})).to_string(),
    );
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());
    // A working directory that does not exist, named with the sentinel.
    let mut nowhere = base("Stop");
    nowhere["cwd"] = json!(format!("/nonexistent/{MARK}_folder/project"));
    hook(&rig, "Stop", &nowhere);

    let kinds: Vec<String> = rig
        .spooled()
        .iter()
        .map(|record| record["event"]["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        [
            "session_start",
            "stop_failure",
            "stop_failure",
            "stop",
            "stop"
        ]
    );
    for record in rig.spooled() {
        assert!(record["event"].get("error_kind").is_none());
    }
    tick(&rig);
    assert!(!mock.everything().contains(MARK));
    assert_eq!(leaks_on_disk(&rig.home.state_dir()), Vec::<String>::new());
}

#[test]
fn the_new_fields_hold_a_time_a_number_or_a_short_word_and_never_a_sentence() {
    let sentence = format!("{MARK} this is a sentence, not a value");
    let now = SystemTime::now();

    // `agent_version` and `login_expires_at`: whatever an agent's adapter
    // hands over, a heartbeat takes a short version string and a time near
    // now, and leaves anything else out.
    let report = |version: &str, expires: SystemTime| {
        EnvironmentReport::new(
            "client-a",
            true,
            Some(version.to_string()),
            Some(expires),
            now,
        )
    };
    let wordy = report(&sentence, now + Duration::from_secs(86_400 * 3_650));
    assert_eq!(wordy.agent_version, None);
    assert_eq!(wordy.login_expires_at, None);
    let known = report("2.1.0", now + Duration::from_secs(86_400));
    let machine = MachineHealth {
        disk_used_pct: 41.5,
        load_1m: 0.62,
        uptime_s: 864_000,
        ..MachineHealth::default()
    };
    let body = serde_json::to_value(Heartbeat::new(machine, vec![wordy, known], now)).unwrap();
    assert!(!body.to_string().contains(MARK));
    assert_eq!(
        body["environments"],
        json!([
            { "env_id": "client-a", "server_alive": true },
            {
                "env_id": "client-a", "server_alive": true, "agent_version": "2.1.0",
                "login_expires_at": rfc3339(now + Duration::from_secs(86_400)),
            },
        ])
    );
    // `activity_beat_s`: a number the daemon is built with.
    assert_eq!(body["activity_beat_s"], 300);

    // `limit_resets_at`: an event read back from the spool with words where
    // the time belongs is set aside, not sent. A spool file is the only way
    // to put words there: the envelope is built from a time, not a string.
    let rig = Rig::new();
    let mock = watch();
    rig.sign_in(&mock.url);
    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            jitter: false,
            ..Timings::default()
        },
    );
    daemon.tick(Instant::now());
    let id = "0193f2a0-0000-7000-8000-00000000beef";
    let spool = rig.spool_dir();
    std::fs::create_dir_all(&spool).unwrap();
    std::fs::write(
        spool.join(format!("{id}.json")),
        json!({
            "event": {
                "event_id": id, "env_id": "client-a", "session_id": SESSION,
                "type": "stop_failure", "error_kind": "rate_limit",
                "limit_resets_at": sentence, "client_ts": rfc3339(now),
            },
            "at_ms": 1,
        })
        .to_string(),
    )
    .unwrap();
    daemon.tick(Instant::now() + Duration::from_secs(1));
    daemon.tick(Instant::now() + Duration::from_secs(30));
    assert!(mock.to("/v1/events:ingest").is_empty());
    assert!(!mock.everything().contains(MARK));
    assert!(rig.spooled().is_empty());
    assert_eq!(files_under(&rig.set_aside_dir()).len(), 1);
    assert!(files_under(&rig.home.state_dir().join("sessions")).is_empty());
}

/// A stand-in for GitHub whose every answer is full of the sentinel, around
/// one address that is an address.
fn github_full_of_content() -> Mock {
    common::github(|slug, sha| {
        let mark = |field: &str| format!("{MARK} {field} written by a person");
        let pull = |html_url: String| {
            json!({
                "html_url": html_url,
                "url": format!("https://api.github.com/repos/{slug}/pulls/1?{MARK}_api_url"),
                "state": "open",
                "merged_at": null,
                "title": mark("pull request title"),
                "body": mark("pull request body"),
                "user": { "login": mark("pull request author") },
                "labels": [{ "name": mark("label"), "description": mark("label description") }],
                "head": {
                    "sha": sha,
                    "ref": format!("{MARK}/pull-request-branch"),
                    "label": mark("head label"),
                    "repo": { "description": mark("repository description") },
                },
                "base": { "ref": format!("{MARK}/base-branch") },
            })
        };
        Reply::json(
            200,
            json!([
                pull(format!("https://github.com/{slug}/pull/412")),
                // An address with anything more in it is not an address.
                pull(format!("https://github.com/{slug}/pull/413?title={MARK}")),
                pull(format!("https://github.com/{slug}/pull/414#{MARK}")),
                pull(format!("https://github.com/{slug}/pull/{MARK}")),
                pull(format!("https://github.com/{slug}/pull/415/{MARK}")),
                pull(format!("https://github.com/{MARK}/web/pull/416")),
                pull(mark("pull request address")),
            ]),
        )
    })
}

#[test]
fn what_a_session_left_behind_leaves_the_box_as_counts_times_and_one_address() {
    let (rig, cwd) = {
        let rig = Rig::under(&format!("{MARK}_parent_folder"));
        rig.as_github("your-org/web");
        let worktree = rig.session("fix-login");
        let cwd = worktree.join("src").join(format!("{MARK}_subfolder"));
        std::fs::create_dir_all(&cwd).unwrap();
        (rig, cwd)
    };
    let worktree = rig.root.join(".worktrees/fix-login/web");
    let content = |what: &str| format!("{MARK} {what} written by the agent\n");

    // A commit whose subject, body, file name and file contents all carry
    // the sentinel, pushed, so that there is a pull request to ask about.
    std::fs::write(
        worktree.join(format!("{MARK}_committed_file.rs")),
        content("committed file content"),
    )
    .unwrap();
    common::git(&worktree, &["add", "."]);
    let message = format!("{MARK} commit subject\n\n{MARK} commit body");
    common::git(&worktree, &["commit", "-q", "-m", &message]);
    rig.push(&worktree);
    // Work that is not committed: an untracked file and a changed one.
    std::fs::write(
        worktree.join(format!("{MARK}_untracked_file.txt")),
        content("untracked file content"),
    )
    .unwrap();
    std::fs::write(worktree.join("README.md"), content("changed file content")).unwrap();
    // The net has no holes: git really would hand all of it over.
    assert!(
        common::git(&worktree, &["log", "-1", "--format=%s%n%b"])
            .matches(MARK)
            .count()
            == 2
    );
    assert!(common::git(&worktree, &["status", "--porcelain"]).contains(MARK));
    assert!(common::git(&worktree, &["diff"]).contains(MARK));

    // A Watch that takes the session folders, so they are sent too.
    let mock = common::gate::watch_taking(&["worktrees"]);
    rig.sign_in(&mock.url);
    let stub = github_full_of_content();
    for (event, allowed) in events() {
        hook(&rig, event, &payload(event, &cwd, &allowed));
    }
    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            jitter: false,
            ..Timings::default()
        },
    );
    let start = Instant::now();
    daemon.tick(start);
    // The reporter counts the nucleus's unsaved work and looks at what the
    // session left, asking the stand-in about its pull requests.
    Reporter::default()
        .asking_github_at(&stub.url)
        .tick(&rig.settings(), SystemTime::now() + Duration::from_secs(11));
    assert!(
        !stub.requests().is_empty(),
        "GitHub was not asked, so nothing was proved"
    );
    daemon.tick(start + Duration::from_secs(30));
    daemon.tick(start + Duration::from_secs(61));
    daemon.write_state(true);

    // What left: the counts, the times and the one address.
    let outcomes: Vec<Value> = mock
        .to(contract::EVENTS)
        .iter()
        .flat_map(|request| request.json()["events"].as_array().cloned().unwrap())
        .filter(|event| event["type"] == "outcome")
        .collect();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["commits"], 1);
    assert_eq!(outcomes[0]["uncommitted_repos"], 1);
    assert_eq!(outcomes[0]["unpushed_commits"], 0);
    assert_eq!(
        outcomes[0]["pull_requests"],
        json!(["https://github.com/your-org/web/pull/412"])
    );
    assert_eq!(outcomes[0]["repos"], json!(["your-org/web"]));
    let beats = mock.to(contract::HEARTBEATS);
    assert_eq!(beats.len(), 2);
    let counted = &beats[1].json()["environments"][0]["unsaved"];
    assert_eq!(counted["uncommitted_repos"], 1);
    assert_eq!(counted["unpushed_commits"], 0);
    assert_eq!(beats[1].json()["machine"]["uncommitted_repos"], 1);
    // The session folder: its name, its repo, its branch and the one address.
    let folders = &beats[1].json()["environments"][0]["worktrees"];
    assert_eq!(folders[0]["folder"], "fix-login");
    assert_eq!(folders[0]["repos"][0]["repo"], "your-org/web");
    assert_eq!(folders[0]["repos"][0]["branch"], "workspace/fix-login");
    assert_eq!(
        folders[0]["repos"][0]["open_pull_request"],
        "https://github.com/your-org/web/pull/412"
    );

    let sent = mock.everything();
    assert!(!sent.contains(MARK), "content on the wire:\n{sent}");
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    only_listed_fields(&mock);
    // What the box keeps: the count, the marker, the session and its outcome.
    assert_eq!(
        leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the local state"
    );
    let status = run(rig.nucleated().args(["status", "--json"]), "");
    let mut shown: Value = serde_json::from_slice(&status.stdout).unwrap();
    // The nucleus path is the owner's own config, shown back to them.
    shown["environments"] = json!([]);
    assert!(!shown.to_string().contains(MARK), "{shown}");
}

#[test]
fn what_the_box_posts_for_itself_is_names_and_times() {
    let (rig, cwd) = rig();
    // Watch does not answer, and the owner's own address does.
    let mock = Mock::start(|_: &Request| Reply::problem(503, "internal_error"));
    let ntfy = Mock::start(|_: &Request| Reply::json(200, json!({})));
    rig.sign_in(&mock.url);
    rig.configure(&format!(
        "[notify]\nurl = \"{}/your-private-topic\"\nafter = \"2m\"\n",
        ntfy.url
    ));
    for (event, allowed) in events() {
        hook(&rig, event, &payload(event, &cwd, &allowed));
    }
    // One more session, left waiting on its owner with a prompt full of content.
    let mut waiting = payload(
        "Notification",
        &cwd,
        &json!({ "notification_type": "permission_prompt" }),
    );
    waiting["session_id"] = json!("0c9d8e7a-58b4-4d0c-9a6e-7f3c2a1e2b1f");
    hook(&rig, "Notification", &waiting);

    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            jitter: false,
            http_timeout: Duration::from_secs(2),
            ..Timings::default()
        },
    );
    let start = Instant::now();
    for later in [0, 120, 121, 122, 300] {
        daemon.tick(start + Duration::from_secs(later));
    }
    daemon.write_state(true);

    let posts = ntfy.requests();
    let kinds: Vec<&str> = posts
        .iter()
        .map(|post| post.header("x-nucarium-alert-kind").unwrap())
        .collect();
    assert_eq!(kinds, ["box_watch_unreachable", "box_session_waiting"]);
    let posted = ntfy.everything();
    assert!(!posted.contains(MARK), "content in a post:\n{posted}");
    for post in &posts {
        // The machine's name, the environment's name and a time.
        assert!(post.body.contains(" UTC"), "{}", post.body);
        for (name, _) in &post.headers {
            let name = name.to_ascii_lowercase();
            assert!(
                [
                    "host",
                    "user-agent",
                    "accept",
                    "accept-encoding",
                    "connection",
                    "content-length",
                    "content-type",
                    "title",
                    "priority",
                    "tags",
                    "x-nucarium-alert-kind",
                ]
                .contains(&name.as_str()),
                "a post carried the header {name}"
            );
        }
    }
    assert_eq!(
        posts[1].header("title"),
        Some("An agent on client-a is waiting for you")
    );
    // Nothing went to Watch but the two requests it has always been sent,
    // and nothing the box wrote down about the outage holds content.
    assert!(!mock.everything().contains(MARK));
    assert!(mock
        .requests()
        .iter()
        .all(|request| [contract::HEARTBEATS, contract::EVENTS].contains(&request.path.as_str())));
    assert_eq!(leaks_on_disk(&rig.home.state_dir()), Vec::<String>::new());
}

#[test]
fn a_count_a_time_or_an_address_with_words_in_it_is_never_sent() {
    let sentence = format!("{MARK} this is a sentence, not a value");
    let now = SystemTime::now();

    // The heartbeat's new fields are numbers, yes-or-noes and times by
    // their types: there is no string among them to put a sentence in.
    let report = EnvironmentReport::new("client-a", true, None, None, now)
        .with_restarts(Some(4))
        .with_unsaved(UnsavedReport::new(2, 5, now, now))
        .with_snapshot(Some(now), now);
    let machine = MachineHealth {
        on_battery: Some(true),
        reboot_required: Some(false),
        ..MachineHealth::default()
    }
    .with_unsaved(Some((2, 5)))
    .with_snapshot(Some(now), now);
    let body = serde_json::to_value(
        Heartbeat::new(machine, vec![report], now)
            .with_pause(Some(now + Duration::from_secs(600)), now),
    )
    .unwrap();
    let stamp = |value: &Value| value.as_str().is_some_and(|s| s.len() == 24);
    assert!(body["machine"]["on_battery"].is_boolean());
    assert!(body["machine"]["reboot_required"].is_boolean());
    assert!(body["machine"]["uncommitted_repos"].is_u64());
    assert!(body["machine"]["unpushed_commits"].is_u64());
    assert!(stamp(&body["machine"]["last_snapshot_at"]));
    assert!(stamp(&body["paused_until"]));
    let environment = &body["environments"][0];
    assert!(environment["server_restarts"].is_u64());
    assert!(stamp(&environment["last_snapshot_at"]));
    assert!(environment["unsaved"]["uncommitted_repos"].is_u64());
    assert!(environment["unsaved"]["unpushed_commits"].is_u64());
    assert!(stamp(&environment["unsaved"]["counted_at"]));

    // The files a heartbeat reads are the box's own, and something else may
    // have written to them. Words where a number or a time belongs make the
    // file no count, no pause and no record of a snapshot.
    let rig = Rig::new();
    let mock = watch();
    rig.sign_in(&mock.url);
    let state = rig.home.state_dir();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("unsaved.json"),
        json!({
            "counted_at_ms": 1,
            "nuclei": [{
                "environments": ["client-a"],
                "unsaved": { "uncommitted_repos": sentence, "unpushed_commits": 0 },
                "last_snapshot_ms": sentence,
            }],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        state.join("pause.json"),
        json!({ "until_ms": sentence, "until": sentence, "set_at": sentence }).to_string(),
    )
    .unwrap();
    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            jitter: false,
            ..Timings::default()
        },
    );
    daemon.tick(Instant::now());
    let beat = mock.to(contract::HEARTBEATS)[0].json();
    assert!(beat.get("paused_until").is_none());
    assert!(beat["environments"][0].get("unsaved").is_none());
    assert!(beat["environments"][0].get("last_snapshot_at").is_none());
    assert!(beat["machine"].get("uncommitted_repos").is_none());

    // An outcome read back from the spool with words where an address or a
    // count belongs, or with a field the envelope does not have, is set
    // aside and never sent.
    let spool = rig.spool_dir();
    std::fs::create_dir_all(&spool).unwrap();
    let outcome = |id: &str, change: &dyn Fn(&mut Value)| {
        let mut event = json!({
            "event_id": id, "env_id": "client-a", "session_id": SESSION,
            "type": "outcome", "repos": ["your-org/web"], "client_ts": rfc3339(now),
        });
        change(&mut event);
        std::fs::write(
            spool.join(format!("{id}.json")),
            json!({ "event": event, "at_ms": 1 }).to_string(),
        )
        .unwrap();
    };
    outcome("0193f2a0-0000-7000-8000-00000000be01", &|event| {
        event["pull_requests"] = json!([sentence]);
    });
    outcome("0193f2a0-0000-7000-8000-00000000be02", &|event| {
        event["pull_requests"] = json!([format!(
            "https://github.com/your-org/web/pull/412?title={MARK}"
        )]);
    });
    outcome("0193f2a0-0000-7000-8000-00000000be03", &|event| {
        event["commits"] = json!(sentence);
    });
    outcome("0193f2a0-0000-7000-8000-00000000be04", &|event| {
        event["pull_request_titles"] = json!([sentence]);
    });
    outcome("0193f2a0-0000-7000-8000-00000000be05", &|event| {
        event["unpushed_commits"] = json!(100_001);
    });
    daemon.tick(Instant::now() + Duration::from_secs(1));
    daemon.tick(Instant::now() + Duration::from_secs(30));
    assert!(mock.to(contract::EVENTS).is_empty());
    assert!(!mock.everything().contains(MARK));
    assert!(rig.spooled().is_empty());
    assert_eq!(files_under(&rig.set_aside_dir()).len(), 5);
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
}

#[test]
fn a_checkout_outside_any_nucleus_gives_away_only_its_repo_and_branch() {
    // A plain clone, in a folder named with the sentinel, with a GitHub remote.
    let rig = Rig::new();
    let mock = watch();
    rig.sign_in(&mock.url);
    let checkout = rig.home.dir.join(format!("{MARK}_projects")).join("api");
    std::fs::create_dir_all(&checkout).unwrap();
    common::git(&checkout, &["init", "-q", "-b", "main"]);
    common::git(
        &checkout,
        &["remote", "add", "origin", "git@github.com:your-org/api.git"],
    );
    let deep = checkout.join(format!("{MARK}_src"));
    std::fs::create_dir_all(&deep).unwrap();
    hook(&rig, "Stop", &payload("Stop", &deep, &json!({})));

    let record = &rig.spooled()[0];
    assert_eq!(record["event"]["repos"], json!(["your-org/api"]));
    assert_eq!(record["event"]["branch"], "main");
    assert!(record.get("workspace").is_none());
    tick(&rig);
    assert!(!mock.everything().contains(MARK));
    assert_eq!(leaks_on_disk(&rig.home.state_dir()), Vec::<String>::new());
}
