//! The release gate for the privacy promise.
//!
//! Every hook event is fed a payload with a sentinel string in every
//! content-bearing field — prompt, message, title, tool input and output,
//! transcript path, file content and the rest — and in the working
//! directory's path and the process environment. The sentinel must then be
//! found nowhere: not in the spool, not in the local state, not in anything
//! the hook prints, and not in any request the daemon makes.
//!
//! If this test fails, content can leave the box. Do not release.

mod common;

use common::{files_under, run, Mock, Reply, Request, Rig};
use nucleate::daemon::{Daemon, Timings};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

/// The sentinel. It appears in every value that must stay on the box.
const MARK: &str = "ZZLEAKZZ";

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

/// Everywhere the sentinel turns up among the files under `dir`.
fn leaks_on_disk(dir: &Path) -> Vec<String> {
    files_under(dir)
        .into_iter()
        .filter(|(_, bytes)| String::from_utf8_lossy(bytes).contains(MARK))
        .map(|(path, bytes)| format!("{}: {}", path.display(), String::from_utf8_lossy(&bytes)))
        .collect()
}

fn watch() -> Mock {
    Mock::start(|request: &Request| match request.path.as_str() {
        "/v1/heartbeats:ingest" => Reply::json(
            200,
            json!({ "received_at": "2026-11-19T12:00:00.000Z", "heartbeat_interval_s": 60 }),
        ),
        _ => Reply::json(
            200,
            json!({ "received_at": "2026-11-19T12:00:00.000Z", "accepted": 0, "duplicates": 0, "rejected": [] }),
        ),
    })
}

fn tick(rig: &Rig) -> Daemon {
    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            jitter: false,
            ..Timings::default()
        },
    );
    daemon.tick(Instant::now());
    // Routine events wait for a batch; a later tick sends them.
    daemon.tick(Instant::now() + Duration::from_secs(30));
    daemon.write_state(true);
    daemon
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
        fixture.matches(MARK).count() >= 35,
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

    // Every field sent is one the envelope names. There is no other.
    let allowed = [
        "event_id",
        "env_id",
        "session_id",
        "bridge_session_id",
        "type",
        "repos",
        "branch",
        "error_kind",
        "client_ts",
    ];
    let mut types = std::collections::BTreeSet::new();
    for request in mock.to("/v1/events:ingest") {
        let body = request.json();
        assert_eq!(body.as_object().unwrap().len(), 1);
        for event in body["events"].as_array().unwrap() {
            for key in event.as_object().unwrap().keys() {
                assert!(allowed.contains(&key.as_str()), "an event carried {key}");
            }
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
    for key in heartbeat.as_object().unwrap().keys() {
        assert!(["machine", "environments", "daemon_version", "client_ts"].contains(&key.as_str()));
    }

    // The local state the daemon keeps: sessions, its own state, anything set aside.
    assert!(files_under(&rig.home.state_dir().join("sessions")).len() == 1);
    assert_eq!(
        leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the local state"
    );
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
