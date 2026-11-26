//! Codex: what the adapter does, and its part of the privacy gate.
//!
//! The gate's shared pieces are in `common::gate`. This file holds a stand-in
//! for Codex's hook engine, built from what Codex's own source says a hooks
//! file and a hook payload are, and the adapter's fixtures: a payload for
//! every Codex hook event with the sentinel in every field that carries
//! content, and under `tests/fixtures/codex/` the payloads Codex 0.172.0 sent
//! in the spike, scrubbed, with the sentinel written into every field that
//! can carry content. The four events that never fired, and the
//! `SessionStart` sources that never came, are written there from Codex's
//! generated hook schemas at that version.
//!
//! If the gate test here fails, content can leave the box. Do not release.

mod common;

use common::gate::{self, MARK};
use common::{contract, path_with, run, Rig};
use nucleate::sessions::{Status, Store};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SESSION: &str = "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a";

/// Every hook event Codex has (`HookEventsToml`, codex-rs/protocol/src/hooks.rs).
const EVENTS: [&str; 14] = [
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "PreModelRequest",
    "PostModelRequest",
    "Interrupt",
    "SessionStart",
    "SessionEnd",
];

/// What `SessionStart` may say started it.
const SOURCES: [&str; 5] = ["startup", "resume", "clear", "compact", "fork"];

/// The tool Codex calls to put a question to its owner.
const QUESTION: &str = "request_user_input";

/// A box whose one environment runs Codex.
fn rig() -> Rig {
    let rig = Rig::new();
    rig.configure_with("check = \"true\"\nagent = \"codex\"", "");
    rig
}

/// The payload Codex hands a hook of this event on standard input, field for
/// field as `codex-rs/hooks/src/schema.rs` and the generated schemas beside
/// it list them, with the sentinel in everything that is content, a path or
/// otherwise not sent.
fn payload(event: &str, session: &str, cwd: &Path, source: &str) -> Value {
    let mut payload = json!({
        "session_id": session,
        "transcript_path": format!("/home/agent/.codex/sessions/{MARK}_transcript_path/rollout-{session}.jsonl"),
        "cwd": cwd,
        "hook_event_name": event,
    });
    let mut set = |fields: &[&str]| {
        for field in fields {
            payload[*field] = match *field {
                "permission_mode" => json!("default"),
                "stop_hook_active" => json!(false),
                "attempt_number" => json!(2),
                "source" => json!(source),
                "tool_input" => json!({
                    "command": gate::mark("tool_input.command"),
                    "description": gate::mark("tool_input.description"),
                    "file_path": format!("/home/agent/work/{MARK}_tool_input_file_path.rs"),
                    "content": gate::mark("tool_input.content: the file's content"),
                    "edits": [{ "old": gate::mark("tool_input.edits.old"), "new": [gate::mark("tool_input.edits.new")] }],
                }),
                // A string in every payload Codex sent: a command's output, or
                // a question's answers as JSON.
                "tool_response" => json!(gate::mark("tool_response: the command's output")),
                "agent_transcript_path" => {
                    json!(format!(
                        "/home/agent/.codex/sessions/{MARK}_agent_transcript_path.jsonl"
                    ))
                }
                "token_usage" => json!({ "input_tokens": 1200, "note": gate::mark("token_usage") }),
                other => json!(gate::mark(other)),
            };
        }
    };
    if event != "SessionEnd" {
        set(&["model"]);
    }
    if !["SessionStart", "SessionEnd"].contains(&event) {
        set(&["turn_id"]);
    }
    match event {
        "SessionStart" => set(&["permission_mode", "source"]),
        "SessionEnd" => set(&["reason"]),
        "UserPromptSubmit" => set(&["agent_id", "agent_type", "permission_mode", "prompt"]),
        "PreToolUse" => set(&[
            "agent_id",
            "agent_type",
            "permission_mode",
            "tool_name",
            "tool_input",
            "tool_use_id",
        ]),
        "PermissionRequest" => set(&[
            "agent_id",
            "agent_type",
            "permission_mode",
            "tool_name",
            "tool_input",
        ]),
        "PostToolUse" => set(&[
            "agent_id",
            "agent_type",
            "permission_mode",
            "tool_name",
            "tool_input",
            "tool_response",
            "tool_use_id",
        ]),
        "PreCompact" | "PostCompact" => set(&["agent_id", "agent_type", "trigger"]),
        "SubagentStart" => set(&["agent_id", "agent_type", "permission_mode"]),
        "SubagentStop" => set(&[
            "agent_id",
            "agent_type",
            "agent_transcript_path",
            "permission_mode",
            "stop_hook_active",
            "last_assistant_message",
        ]),
        "Stop" => set(&[
            "permission_mode",
            "stop_hook_active",
            "last_assistant_message",
        ]),
        "Interrupt" => set(&["permission_mode"]),
        "PreModelRequest" | "PostModelRequest" => set(&[
            "agent_id",
            "agent_type",
            "attempt_number",
            "kind",
            "request_id",
            "thread_id",
            "response_id",
            "token_usage",
            // What a failed attempt says: a word and a sentence.
            "status",
            "error",
        ]),
        _ => {}
    }
    payload
}

/// `PreToolUse` or `PostToolUse` around a question to the owner.
fn question(event: &str, session: &str, cwd: &Path) -> Value {
    let mut payload = payload(event, session, cwd, "startup");
    payload["tool_name"] = json!(QUESTION);
    payload
}

/// The gate's payload: what Codex sends for the event, and on top of it every
/// field any Codex hook carries, fields that do not exist yet, and fields
/// named as a guess would name them. All hold the sentinel.
fn stuffed(event: &str, cwd: &Path, source: &str) -> Value {
    let mut payload = payload(event, SESSION, cwd, source);
    for other in EVENTS {
        for (key, value) in self::payload(other, SESSION, cwd, source)
            .as_object()
            .cloned()
            .unwrap()
        {
            if payload.get(&key).is_none() {
                payload[key] = value;
            }
        }
    }
    // `source` is a short word only where it belongs. Anywhere else, and
    // every other word a hook carries, is a sentence here.
    for field in [
        "reason",
        "trigger",
        "status",
        "kind",
        "permission_mode",
        "model",
    ] {
        payload[field] = json!(gate::mark(field));
    }
    if event != "SessionStart" {
        payload["source"] = json!(gate::mark("source"));
    }
    payload["something_added_next_year"] =
        json!({ "deep": [gate::mark("a field that does not exist yet")] });
    // Where Codex might one day say what went wrong, when a limit resets,
    // when a login runs out, which version it is or where to open the
    // session. None is a field a Codex hook has, so none is read.
    for field in [
        "error_kind",
        "codexErrorInfo",
        "limit_resets_at",
        "resets_at",
        "retry_after",
        "expires_at",
        "login_expires_at",
        "last_refresh",
        "version",
        "agent_version",
        "thread_url",
        "bridge_session_id",
    ] {
        payload[field] = json!(gate::mark(field));
    }
    payload
}

/// A stand-in for Codex's hook engine: it loads a hooks file as Codex does
/// and, at each lifecycle event, runs the commands registered for it.
struct Codex {
    hooks: Value,
    /// This login's `CODEX_HOME`, if the session was started with one.
    home: Option<PathBuf>,
    path: OsString,
    user_home: PathBuf,
}

impl Codex {
    /// Load a `hooks.json`, refusing what Codex refuses and what would never
    /// run (codex-rs/config/src/hook_config.rs, codex-rs/protocol/src/hooks.rs).
    fn load(rig: &Rig, hooks_json: &str, home: Option<&Path>) -> Codex {
        let file: Value = serde_json::from_str(hooks_json).expect("hooks.json is JSON");
        for key in file.as_object().expect("an object").keys() {
            // `HooksFile` denies unknown fields.
            assert!(
                ["description", "hooks"].contains(&key.as_str()),
                "Codex refuses a hooks file with the key {key}"
            );
        }
        for (event, groups) in file["hooks"].as_object().expect("hooks") {
            assert!(
                EVENTS.contains(&event.as_str()),
                "{event} is not a Codex event"
            );
            for group in groups.as_array().expect("matcher groups") {
                for key in group.as_object().unwrap().keys() {
                    assert!(["matcher", "hooks"].contains(&key.as_str()), "{key}");
                }
                if let Some(matcher) = group.get("matcher") {
                    assert!(matcher.is_string(), "a matcher is a string");
                }
                for handler in group["hooks"].as_array().expect("handlers") {
                    assert_eq!(handler["type"], "command");
                    assert!(handler["command"].is_string());
                    for (key, value) in handler.as_object().unwrap() {
                        match key.as_str() {
                            "type" | "command" => {}
                            "timeout" => assert!(value.is_u64()),
                            "async" => assert!(value.is_boolean()),
                            other => panic!("a handler field Codex does not have: {other}"),
                        }
                    }
                    // Codex clamps these two to three seconds and says so in
                    // its log; a hooks file should not ask for more.
                    if ["SessionEnd", "Interrupt"].contains(&event.as_str()) {
                        assert!(handler["timeout"].as_u64().is_some_and(|s| s <= 3));
                    }
                }
            }
        }
        // `nucleated` on the PATH, as the commands expect it, and a `codex`
        // that leaves a mark if anything ever starts it.
        let mark = rig.home.dir.join("codex-was-started");
        let path = path_with(
            &rig.home,
            &[
                (
                    "nucleated",
                    &format!("exec \"{}\" \"$@\"", env!("CARGO_BIN_EXE_nucleated")),
                ),
                ("codex", &format!("touch \"{}\"\nexit 1", mark.display())),
            ],
        );
        Codex {
            hooks: file["hooks"].clone(),
            home: home.map(Path::to_path_buf),
            path,
            user_home: rig.home.dir.clone(),
        }
    }

    /// The hooks file `nucleated hooks print` writes for `args`.
    fn registered(rig: &Rig, args: &[&str], home: Option<&Path>) -> Codex {
        let out = run(rig.nucleated().args(["hooks", "print"]).args(args), "");
        assert!(out.status.success(), "{}", common::stderr(&out));
        Codex::load(rig, &String::from_utf8(out.stdout).unwrap(), home)
    }

    /// Run one hook command the way Codex does: through a shell, with the
    /// session's environment and the payload on standard input.
    fn command(&self, command: &str, cwd: &Path, payload: &Value) -> Output {
        let mut shell = Command::new("sh");
        shell
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &self.user_home)
            .env("LC_ALL", "C")
            // What else a session's environment holds.
            .env("OPENAI_API_KEY", format!("{MARK}_api_key_in_environment"))
            .env("GH_TOKEN", format!("{MARK}_github_token_in_environment"))
            .env("USER", format!("{MARK}_user"))
            .env("SSH_CONNECTION", format!("{MARK}_ip_address 22"));
        if let Some(home) = &self.home {
            shell.env("CODEX_HOME", home);
        }
        run(&mut shell, &payload.to_string())
    }

    /// A lifecycle event happens: every command registered for it, in a
    /// group whose matcher lets it through, runs. A hook may never fail,
    /// print or complain. Returns how many ran.
    fn fire_with(&self, event: &str, cwd: &Path, payload: &Value) -> usize {
        let mut ran = 0;
        for group in self.hooks[event].as_array().into_iter().flatten() {
            if !matched(event, group, payload) {
                continue;
            }
            for handler in group["hooks"].as_array().unwrap() {
                let out = self.command(handler["command"].as_str().unwrap(), cwd, payload);
                assert_eq!(out.status.code(), Some(0), "the hook failed for {event}");
                assert!(out.stdout.is_empty(), "the hook printed for {event}");
                assert!(out.stderr.is_empty(), "the hook complained for {event}");
                ran += 1;
            }
        }
        ran
    }

    fn fire(&self, event: &str, session: &str, cwd: &Path) -> usize {
        self.fire_with(event, cwd, &payload(event, session, cwd, "startup"))
    }
}

/// Whether a matcher group runs for this payload, as Codex decides it: no
/// matcher, an empty one or `*` runs for everything; one made of letters,
/// digits, `_` and `|` is a list of exact names; and the events that take no
/// matcher ignore one (codex-rs/hooks/src/engine/matcher.rs and
/// codex-rs/hooks/src/events/common.rs at rust-v0.172.0). The stand-in knows
/// what a tool event is matched on, and refuses what it does not know.
fn matched(event: &str, group: &Value, payload: &Value) -> bool {
    let Some(pattern) = group.get("matcher").and_then(Value::as_str) else {
        return true;
    };
    let input = match event {
        "PreToolUse" | "PermissionRequest" | "PostToolUse" => payload["tool_name"].as_str(),
        "UserPromptSubmit" | "Stop" | "Interrupt" | "PreModelRequest" | "PostModelRequest" => {
            return true
        }
        other => panic!("the stand-in does not know what Codex matches {other} on"),
    };
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    assert!(
        pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|'),
        "the stand-in matches names, not patterns: {pattern}"
    );
    input.is_some_and(|input| pattern.split('|').any(|name| name == input))
}

fn kinds(rig: &Rig) -> Vec<String> {
    rig.spooled()
        .iter()
        .map(|record| record["event"]["type"].as_str().unwrap().to_string())
        .collect()
}

fn status_of(rig: &Rig, env: &str) -> Option<Status> {
    let sessions = Store::new(&rig.home.state_dir()).list(env);
    assert!(sessions.len() <= 1);
    sessions.first().map(|session| session.status)
}

#[test]
fn an_event_codex_does_not_have_records_nothing_and_holds_nothing_up() {
    let rig = rig();
    assert_eq!(rig.settings().environments[0].agent, "codex");
    let payload = json!({
        "session_id": SESSION,
        "cwd": rig.root,
        "prompt": gate::mark("prompt"),
    });
    let out = run(
        rig.nucleated()
            .args(["hook", "SomethingNotInventedYet", "--agent", "codex"]),
        &payload.to_string(),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert!(rig.spooled().is_empty());
    assert_eq!(
        gate::leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new()
    );
}

#[test]
fn a_codex_session_is_followed_from_its_start_to_its_end() {
    let rig = rig();
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let cwd = rig.session("fix-login");
    let codex = Codex::registered(&rig, &["--env", "client-a"], None);

    // The session starts, is asked something, and wants to run a command it
    // needs an approval for.
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreModelRequest",
        "PostModelRequest",
        "PreToolUse",
        "PermissionRequest",
    ] {
        codex.fire(event, SESSION, &cwd);
    }
    assert_eq!(
        kinds(&rig),
        ["session_start", "user_prompt_submit", "permission_prompt"]
    );
    let first = &rig.spooled()[0];
    assert_eq!(first["event"]["session_id"], SESSION);
    assert_eq!(first["event"]["env_id"], "client-a");
    assert_eq!(first["event"]["repos"], json!(["web"]));
    assert_eq!(first["event"]["branch"], "workspace/fix-login");
    // Codex gives no second id for the session.
    assert!(first["event"].get("bridge_session_id").is_none());
    assert_eq!(first["workspace"], "fix-login");
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::NeedsInput));

    // The approval is given: the tool runs and the turn finishes.
    for event in ["PostToolUse", "SubagentStart", "SubagentStop", "Stop"] {
        codex.fire(event, SESSION, &cwd);
    }
    assert_eq!(kinds(&rig), ["activity", "stop"]);
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::Idle));

    // A second turn, which its owner cuts short. Codex runs no Stop for it.
    for event in ["UserPromptSubmit", "PreCompact", "PostCompact"] {
        codex.fire(event, SESSION, &cwd);
    }
    // A compaction starts the context again, not the session.
    codex.fire_with(
        "SessionStart",
        &cwd,
        &payload("SessionStart", SESSION, &cwd, "compact"),
    );
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::Working));
    codex.fire("Interrupt", SESSION, &cwd);
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::Idle));

    // A third turn puts a question to the owner. No approval is asked for,
    // so only the question tool's PreToolUse says the session is waiting.
    codex.fire("UserPromptSubmit", SESSION, &cwd);
    assert_eq!(
        codex.fire_with("PreToolUse", &cwd, &question("PreToolUse", SESSION, &cwd)),
        1
    );
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::NeedsInput));
    // Answered: the tool's PostToolUse, and the turn finishes.
    codex.fire_with("PostToolUse", &cwd, &question("PostToolUse", SESSION, &cwd));
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::Working));
    codex.fire("Stop", SESSION, &cwd);
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::Idle));

    // And it is closed.
    codex.fire("SessionEnd", SESSION, &cwd);
    gate::tick(&rig);
    assert_eq!(status_of(&rig, "client-a"), Some(Status::Ended));

    let sent: Vec<String> = mock
        .to(contract::EVENTS)
        .iter()
        .flat_map(|request| request.json()["events"].as_array().cloned().unwrap())
        .map(|event| event["type"].as_str().unwrap().to_string())
        .collect();
    let mut sorted = sent.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        [
            "activity",
            "activity",
            "input_prompt",
            "permission_prompt",
            "session_end",
            "session_start",
            "stop",
            "stop",
            "stop",
            "user_prompt_submit",
            "user_prompt_submit",
            "user_prompt_submit",
        ],
        "{sent:?}"
    );
    // What Codex gives the box no way to know is not said.
    for request in mock.to(contract::EVENTS) {
        for event in request.json()["events"].as_array().unwrap() {
            assert!(event.get("error_kind").is_none());
            assert!(event.get("limit_resets_at").is_none());
            assert!(event.get("bridge_session_id").is_none());
        }
    }
    for request in mock.to(contract::HEARTBEATS) {
        let environment = &request.json()["environments"][0];
        assert_eq!(environment["server_alive"], true);
        assert!(environment.get("agent_version").is_none());
        assert!(environment.get("login_expires_at").is_none());
    }
    gate::assert_clean(&rig, &mock);
    // Nothing started Codex to find any of this out.
    assert!(!rig.home.dir.join("codex-was-started").exists());
}

#[test]
fn the_daemon_itself_never_starts_codex() {
    let rig = rig();
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let cwd = rig.session("fix-login");
    let codex = Codex::registered(&rig, &["--env", "client-a"], None);
    let mark = rig.home.dir.join("codex-was-started");
    for event in ["SessionStart", "UserPromptSubmit", "PermissionRequest"] {
        codex.fire(event, SESSION, &cwd);
    }

    // The daemon itself, as a process of its own, with the stand-in `codex`
    // first on its PATH: it sends a heartbeat, which asks the adapter when
    // the login expires and which version runs, and the session's events.
    let waiting = || {
        mock.to(contract::EVENTS).iter().any(|request| {
            request.json()["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["type"] == "permission_prompt")
        })
    };
    let out = gate::daemon_until(
        &rig,
        Some(&codex.path),
        "a heartbeat and the session's events",
        &|| !mock.to(contract::HEARTBEATS).is_empty() && waiting(),
    );
    assert!(!mark.exists(), "the daemon started codex");
    assert_eq!(status_of(&rig, "client-a"), Some(Status::NeedsInput));
    let environment = &mock.to(contract::HEARTBEATS)[0].json()["environments"][0];
    assert_eq!(environment["server_alive"], true);
    assert!(environment.get("agent_version").is_none());
    assert!(environment.get("login_expires_at").is_none());
    let logged = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        common::stderr(&out)
    );
    assert!(!logged.contains(MARK), "{logged}");
    gate::assert_clean(&rig, &mock);

    // The net has no hole: the stand-in is what that PATH finds, and it
    // does leave its mark when something starts it.
    let found = run(
        Command::new("sh")
            .args(["-c", "codex --version; true"])
            .env("PATH", &codex.path),
        "",
    );
    assert!(found.status.success());
    assert!(mark.exists());
}

#[test]
fn the_hooks_file_registers_only_the_events_the_adapter_reads() {
    let rig = rig();
    let cwd = rig.session("fix-login");
    let codex = Codex::registered(&rig, &["--env", "client-a"], None);
    let mut heard = Vec::new();
    for event in EVENTS {
        let before = rig.spooled().len();
        let ran = codex.fire(event, SESSION, &cwd);
        // No command is registered for an event that would record nothing.
        assert_eq!(ran, rig.spooled().len() - before, "{event}");
        if ran > 0 {
            heard.push(event);
        }
    }
    heard.sort();
    assert_eq!(
        heard,
        [
            "Interrupt",
            "PermissionRequest",
            "PostToolUse",
            "SessionEnd",
            "SessionStart",
            "Stop",
            "UserPromptSubmit",
        ]
    );
    // `PreToolUse` runs for the question tool alone: a command, an edit or
    // an MCP tool starts no process at all.
    for tool in ["Bash", "apply_patch", "mcp__docs__request_user_input"] {
        let mut payload = payload("PreToolUse", SESSION, &cwd, "startup");
        payload["tool_name"] = json!(tool);
        assert_eq!(codex.fire_with("PreToolUse", &cwd, &payload), 0, "{tool}");
    }
    let before = rig.spooled().len();
    assert_eq!(
        codex.fire_with("PreToolUse", &cwd, &question("PreToolUse", SESSION, &cwd)),
        1
    );
    assert_eq!(rig.spooled().len(), before + 1);
    assert_eq!(kinds(&rig).last().unwrap(), "input_prompt");
    // With no environment named, the agent still is: a hook that names no
    // agent is Claude Code's.
    let out = run(
        rig.nucleated().args(["hooks", "print", "--agent", "codex"]),
        "",
    );
    let block: Value = serde_json::from_slice(&out.stdout).unwrap();
    for (event, groups) in block["hooks"].as_object().unwrap() {
        assert_eq!(
            groups[0]["hooks"][0]["command"],
            format!("nucleated hook {event} --agent codex")
        );
    }
}

/// A box with a Claude Code environment listed first and two Codex logins
/// that share one nucleus, each with a `CODEX_HOME` of its own.
fn mixed_box() -> (Rig, PathBuf, PathBuf, PathBuf) {
    let rig = Rig::new();
    let shared = rig.home.dir.join("codex-nucleus");
    let home_a = rig.home.dir.join(".codex-a");
    let home_b = rig.home.dir.join(".codex-b");
    for dir in [&shared, &home_a, &home_b] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let login = |name: &str, home: &Path| {
        format!(
            "[[environment]]\nname = \"{name}\"\nroot = \"{}\"\ncheck = \"true\"\n\
             agent = \"codex\"\nconfig_dir = \"{}\"\n",
            shared.display(),
            home.display()
        )
    };
    rig.configure(&format!(
        "{}{}",
        login("codex-a", &home_a),
        login("codex-b", &home_b)
    ));
    (rig, shared, home_a, home_b)
}

#[test]
fn a_box_wide_hooks_file_tells_one_codex_login_from_another_by_its_codex_home() {
    let (rig, shared, home_a, home_b) = mixed_box();
    let settings = rig.settings();
    assert_eq!(settings.environments[0].agent, "claude-code");
    // One hooks file for the whole box names no environment.
    let as_a = Codex::registered(&rig, &["--agent", "codex"], Some(&home_a));
    let as_b = Codex::registered(&rig, &["--agent", "codex"], Some(&home_b));
    let other = "0c9d8e7a-58b4-4d0c-9a6e-7f3c2a1e2b1f";

    // Events only Codex has, on a box whose first environment is Claude
    // Code's: they are read as Codex's, and land on the right login.
    as_a.fire("PermissionRequest", SESSION, &shared);
    as_b.fire("Interrupt", other, &shared);
    // Where the session is working does not outvote whose login it is.
    as_b.fire("PostToolUse", other, &rig.root);
    let spooled = rig.spooled();
    let seen: Vec<(&str, &str, &str)> = spooled
        .iter()
        .map(|record| {
            let event = &record["event"];
            (
                event["env_id"].as_str().unwrap(),
                event["type"].as_str().unwrap(),
                event["session_id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("codex-a", "permission_prompt", SESSION),
            ("codex-b", "stop", other),
            ("codex-b", "activity", other),
        ]
    );
}

#[test]
fn a_codex_hook_that_cannot_be_placed_is_not_guessed_at() {
    let (rig, shared, _, _) = mixed_box();
    // No CODEX_HOME, or one that is no configured login's: two logins share
    // the folder the session is in, so the hook belongs to neither.
    let nobody = Codex::registered(&rig, &["--agent", "codex"], None);
    let stranger = Codex::registered(
        &rig,
        &["--agent", "codex"],
        Some(&rig.home.dir.join(".codex-somebody-else")),
    );
    assert_eq!(nobody.fire("Stop", SESSION, &shared), 1);
    assert_eq!(stranger.fire("Stop", SESSION, &shared), 1);
    assert!(rig.spooled().is_empty());

    // A Codex payload whose command names no agent is read as Claude
    // Code's. Interrupt is not an event Claude Code has, so nothing is
    // recorded: it is never handed to another adapter to try.
    let cwd = rig.session("fix-login");
    let out = run(
        rig.nucleated().args(["hook", "Interrupt"]),
        &payload("Interrupt", SESSION, &cwd, "startup").to_string(),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(rig.spooled().is_empty());
    // An environment that is not there, and an agent that is not one.
    for args in [
        ["hook", "Stop", "--agent", "codex", "--env", "nowhere"],
        ["hook", "Stop", "--agent", "codex-next", "--env", "codex-a"],
    ] {
        let out = run(
            rig.nucleated().args(args),
            &payload("Stop", SESSION, &cwd, "startup").to_string(),
        );
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
    assert!(rig.spooled().is_empty());
}

#[test]
fn a_box_where_codex_is_absent_or_broken_reports_itself_and_nothing_else() {
    let rig = rig();
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    // Codex is not installed, or its hooks were never reviewed: no hook ever
    // runs. The box and its agent server are reported as they are.
    gate::tick(&rig);
    let beat = mock.to(contract::HEARTBEATS)[0].json();
    assert_eq!(beat["environments"][0]["env_id"], "client-a");
    assert_eq!(beat["environments"][0]["server_alive"], true);
    assert!(mock.to(contract::EVENTS).is_empty());

    // A Codex that hands over something that is not a hook payload: cut
    // short, empty, not JSON, a list, or with no session in it.
    let whole = payload("Stop", SESSION, &rig.root, "startup").to_string();
    for broken in [
        &whole[..whole.len() / 2],
        "",
        "codex: internal error",
        "[\"7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a\", \"/home/agent\"]",
        "{\"cwd\": \"/home/agent\"}",
        "null",
    ] {
        for event in ["SessionStart", "Stop", "PermissionRequest", "SessionEnd"] {
            let out = run(
                rig.nucleated()
                    .args(["hook", event, "--agent", "codex", "--env", "client-a"]),
                broken,
            );
            assert_eq!(out.status.code(), Some(0), "{event}: {broken}");
            assert!(out.stdout.is_empty() && out.stderr.is_empty());
        }
    }
    assert!(rig.spooled().is_empty());
    gate::tick(&rig);
    assert!(mock.to(contract::EVENTS).is_empty());
    gate::assert_clean(&rig, &mock);
}

/// A box for the gate: the nucleus under a folder named with the sentinel, a
/// session working in a subfolder named with it, and a Codex login whose
/// `CODEX_HOME` is named with it too. The hooks file is the box-wide one, so
/// that path is what places every hook.
fn gate_rig() -> (Rig, PathBuf, Codex) {
    let rig = Rig::under(&format!("{MARK}_parent_folder"));
    let home = rig.home.dir.join(format!("{MARK}_codex_home"));
    std::fs::create_dir_all(&home).unwrap();
    rig.configure_with(
        &format!(
            "check = \"true\"\nagent = \"codex\"\nconfig_dir = \"{}\"",
            home.display()
        ),
        "",
    );
    let worktree = rig.session("fix-login");
    let cwd = worktree.join("src").join(format!("{MARK}_subfolder"));
    std::fs::create_dir_all(&cwd).unwrap();
    let codex = Codex::registered(&rig, &["--agent", "codex"], Some(&home));
    (rig, cwd, codex)
}

/// Every event with every value of the one short word the adapter may read,
/// and an event that does not exist.
fn everything() -> Vec<(&'static str, &'static str)> {
    let mut all: Vec<(&str, &str)> = SOURCES
        .iter()
        .map(|source| ("SessionStart", *source))
        .collect();
    for event in EVENTS {
        if event != "SessionStart" {
            all.push((event, "startup"));
        }
    }
    all.push(("SomethingNotInventedYet", "startup"));
    all
}

/// Hand the entry point a payload directly, as a hooks file that registered
/// every event would, with the sentinel in the environment as well.
fn hook(codex: &Codex, event: &str, cwd: &Path, payload: &Value) {
    let out = codex.command(
        &format!("nucleated hook {event} --agent codex"),
        cwd,
        payload,
    );
    assert_eq!(out.status.code(), Some(0), "the hook failed for {event}");
    assert!(
        out.stdout.is_empty(),
        "the hook printed something for {event}"
    );
    assert!(out.stderr.is_empty(), "the hook complained for {event}");
}

#[test]
fn the_codex_fixture_would_catch_a_leak() {
    let fixture = stuffed("PostToolUse", Path::new("/x"), "startup").to_string();
    gate::assert_fixture_would_catch_a_leak(
        &fixture,
        40,
        &[
            "transcript_path",
            "agent_transcript_path",
            "prompt",
            "last_assistant_message",
            "tool_name",
            "tool_use_id",
            "tool_input",
            "tool_response",
            "description",
            "content",
            "model",
            "turn_id",
            "permission_mode",
            "agent_id",
            "agent_type",
            "reason",
            "trigger",
            "error",
            "status",
            "kind",
            "request_id",
            "response_id",
            "thread_id",
            "token_usage",
            "source",
            "something_added_next_year",
            "error_kind",
            "codexErrorInfo",
            "limit_resets_at",
            "resets_at",
            "expires_at",
            "login_expires_at",
            "version",
            "agent_version",
        ],
    );
    // Every event's own payload carries the sentinel too, and names the
    // event as Codex does.
    for event in EVENTS {
        let own = payload(event, SESSION, Path::new("/x"), "startup");
        assert_eq!(own["hook_event_name"], event);
        assert!(own.to_string().contains(MARK), "{event}");
    }
}

#[test]
fn no_content_from_a_codex_hook_reaches_the_spool_the_local_state_or_the_wire() {
    let (rig, cwd, codex) = gate_rig();
    let mock = gate::watch();
    rig.sign_in(&mock.url);

    for (event, source) in everything() {
        // The payload Codex sends for the event, then the same with every
        // other field there is on top.
        hook(&codex, event, &cwd, &payload(event, SESSION, &cwd, source));
        hook(&codex, event, &cwd, &stuffed(event, &cwd, source));
    }
    // The question to the owner, the one tool whose name is compared, with
    // everything else on top.
    for event in ["PreToolUse", "PostToolUse"] {
        hook(&codex, event, &cwd, &question(event, SESSION, &cwd));
        let mut asked = stuffed(event, &cwd, "startup");
        asked["tool_name"] = json!(QUESTION);
        hook(&codex, event, &cwd, &asked);
    }

    let spooled = rig.spooled();
    assert!(
        spooled.len() >= 20,
        "the hooks recorded too little to prove anything: {}",
        spooled.len()
    );
    assert_eq!(
        gate::leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the spool"
    );
    // What was recorded is the metadata, and it is right: the login was
    // found by its CODEX_HOME and the repo and branch by the folder.
    let first = &spooled[0];
    assert_eq!(first["event"]["session_id"], SESSION);
    assert_eq!(first["event"]["env_id"], "client-a");
    assert_eq!(first["event"]["repos"], json!(["web"]));
    assert_eq!(first["event"]["branch"], "workspace/fix-login");
    assert_eq!(first["workspace"], "fix-login");
    for record in &spooled {
        let event = record["event"].as_object().unwrap();
        // The guessed fields were not read, whatever they held.
        for unread in ["error_kind", "limit_resets_at", "bridge_session_id"] {
            assert!(!event.contains_key(unread), "{unread} was recorded");
        }
    }

    gate::tick(&rig);
    assert_eq!(mock.to(contract::HEARTBEATS).len(), 1);
    let mut types = std::collections::BTreeSet::new();
    for request in mock.to(contract::EVENTS) {
        for event in request.json()["events"].as_array().unwrap() {
            assert!(event.get("error_kind").is_none());
            assert!(event.get("limit_resets_at").is_none());
            assert!(event.get("bridge_session_id").is_none());
            types.insert(event["type"].as_str().unwrap().to_string());
        }
    }
    // Everything Codex can say went through, and nothing it cannot.
    assert_eq!(
        types.into_iter().collect::<Vec<_>>(),
        [
            "activity",
            "input_prompt",
            "permission_prompt",
            "session_end",
            "session_start",
            "stop",
            "user_prompt_submit",
        ]
    );
    let heartbeat = mock.to(contract::HEARTBEATS)[0].json();
    for environment in heartbeat["environments"].as_array().unwrap() {
        assert!(environment.get("agent_version").is_none());
        assert!(environment.get("login_expires_at").is_none());
    }
    gate::assert_clean(&rig, &mock);
    assert!(!rig.home.dir.join("codex-was-started").exists());
}

#[test]
fn content_where_a_codex_hook_has_a_short_word_is_dropped_not_sent() {
    let (rig, cwd, codex) = gate_rig();
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let sentence = format!("{MARK} this is a sentence, not a word");
    let base = |event: &str| stuffed(event, &cwd, "startup");

    // A session id that is not an id: no event at all.
    let mut odd = base("Stop");
    odd["session_id"] = json!(sentence);
    hook(&codex, "Stop", &cwd, &odd);
    // A source that is not a source: still a start, and nothing kept.
    let mut started = base("SessionStart");
    started["source"] = json!(sentence);
    hook(&codex, "SessionStart", &cwd, &started);
    // A working folder that is a sentence, and one that is not there.
    let mut wordy = base("Stop");
    wordy["cwd"] = json!(sentence);
    hook(&codex, "Stop", &cwd, &wordy);
    let mut nowhere = base("PostToolUse");
    nowhere["cwd"] = json!(format!("/nonexistent/{MARK}_folder/project"));
    hook(&codex, "PostToolUse", &cwd, &nowhere);
    // Fields of the wrong type altogether.
    let mut wrong = base("PermissionRequest");
    wrong["cwd"] = json!([sentence]);
    wrong["source"] = json!({ "why": sentence });
    hook(&codex, "PermissionRequest", &cwd, &wrong);
    let mut worse = base("SessionEnd");
    worse["session_id"] = json!({ "id": SESSION, "title": sentence });
    hook(&codex, "SessionEnd", &cwd, &worse);
    // The payload's own name for the event is not what decides the event.
    let mut renamed = base("PreToolUse");
    renamed["hook_event_name"] = json!("Stop");
    hook(&codex, "PreToolUse", &cwd, &renamed);

    assert_eq!(
        kinds(&rig),
        ["session_start", "stop", "activity", "permission_prompt"]
    );
    for record in rig.spooled() {
        assert_eq!(record["event"]["env_id"], "client-a");
        assert!(record["event"].get("error_kind").is_none());
    }
    // A folder that could not be read gives no repo and no branch.
    assert!(rig.spooled()[1]["event"].get("repos").is_none());
    assert!(rig.spooled()[1]["event"].get("branch").is_none());
    gate::tick(&rig);
    gate::assert_clean(&rig, &mock);
}

// ---- what Codex 0.172.0 sent ------------------------------------------

/// The payloads recorded in the spike, one file per run, in the order Codex
/// ran the hooks. Each payload's `cwd` is `{cwd}`, for the folder the test
/// puts the session in.
const RECORDED: [(&str, &str); 7] = [
    ("exec-turn", include_str!("fixtures/codex/exec-turn.json")),
    (
        "exec-failed-turn",
        include_str!("fixtures/codex/exec-failed-turn.json"),
    ),
    (
        "hosted-approval-refused",
        include_str!("fixtures/codex/hosted-approval-refused.json"),
    ),
    (
        "hosted-question",
        include_str!("fixtures/codex/hosted-question.json"),
    ),
    (
        "hosted-approval-given",
        include_str!("fixtures/codex/hosted-approval-given.json"),
    ),
    (
        "hosted-protocol-decline",
        include_str!("fixtures/codex/hosted-protocol-decline.json"),
    ),
    (
        "from-schema",
        include_str!("fixtures/codex/from-schema.json"),
    ),
];

/// The run's session, and Codex's title session beside it in
/// `hosted-approval-refused`, as the fixtures name them.
const RUN_SESSION: &str = "00000000-0000-7000-8000-000000000003";
const TITLE_SESSION: &str = "00000000-0000-7000-8000-000000000004";

/// Words a recorded payload holds as Codex sent them, not as the sentinel.
/// None of them may leave the hook either.
const RECORDED_WORDS: [&str; 4] = ["Bash", QUESTION, "bypassPermissions", "sampling"];

/// One run's hooks: each event's name and its payload, with `{cwd}` made the
/// folder given.
fn recorded(name: &str, cwd: &Path) -> Vec<(String, Value)> {
    let text = RECORDED
        .iter()
        .find(|(run, _)| *run == name)
        .map(|(_, text)| *text)
        .expect("a recorded run");
    let file: Value = serde_json::from_str(text).expect("the fixture is JSON");
    file["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| {
            let mut payload = event["payload"].clone();
            for value in payload.as_object_mut().unwrap().values_mut() {
                if value == "{cwd}" {
                    *value = json!(cwd);
                }
            }
            (event["event"].as_str().unwrap().to_string(), payload)
        })
        .collect()
}

/// Every file and folder name under the state folder, and what each file
/// holds, as one string.
fn state_text(rig: &Rig) -> String {
    common::files_under(&rig.home.state_dir())
        .into_iter()
        .map(|(path, bytes)| format!("{}\n{}", path.display(), String::from_utf8_lossy(&bytes)))
        .collect()
}

/// The records in the spool, as (session, type).
fn spooled_pairs(rig: &Rig) -> Vec<(String, String)> {
    rig.spooled()
        .iter()
        .map(|record| {
            let event = &record["event"];
            (
                event["session_id"].as_str().unwrap().to_string(),
                event["type"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn status_of_session(rig: &Rig, env: &str, session: &str) -> Option<Status> {
    Store::new(&rig.home.state_dir())
        .list(env)
        .into_iter()
        .find(|known| known.session_id == session)
        .map(|known| known.status)
}

#[test]
fn the_recorded_codex_fixtures_would_catch_a_leak() {
    let cwd = PathBuf::from(format!("/home/agent/{MARK}_folder/work"));
    let mut events = std::collections::BTreeSet::new();
    let mut sources = std::collections::BTreeSet::new();
    for (name, text) in RECORDED {
        // Scrubbed: no folder of the scratch box, and no id of the vendor's.
        for scrubbed in ["spike", "/home/agent/spike", "req_", "resp_", "-AKL"] {
            assert!(!text.contains(scrubbed), "{name} still holds {scrubbed}");
        }
        let run = recorded(name, &cwd);
        gate::assert_fixture_would_catch_a_leak(
            text,
            run.len(),
            &["session_id", "cwd", "hook_event_name"],
        );
        for (event, payload) in &run {
            assert_eq!(payload["hook_event_name"], *event, "{name}");
            // Every payload carries the sentinel, if only in its folder.
            assert!(payload.to_string().contains(MARK), "{name}: {event}");
            // Every string Codex sent is the sentinel, or one of the words
            // the payload carries as Codex sent them.
            for (key, value) in payload.as_object().unwrap() {
                let Some(text) = value.as_str() else { continue };
                assert!(
                    text.contains(MARK)
                        || [
                            "session_id",
                            "hook_event_name",
                            "permission_mode",
                            "source",
                            "kind",
                            "status",
                            "reason",
                            "trigger",
                            "tool_name",
                        ]
                        .contains(&key.as_str()),
                    "{name}: {event}: {key} = {text}"
                );
            }
            events.insert(event.clone());
            if event == "SessionStart" {
                sources.insert(payload["source"].as_str().unwrap().to_string());
            }
        }
    }
    // Every event Codex has, and every source a session can start from.
    assert_eq!(events, EVENTS.iter().map(|e| e.to_string()).collect());
    assert_eq!(sources, SOURCES.iter().map(|s| s.to_string()).collect());

    // The shapes the spike found, as it found them.
    let tools: Vec<(String, Value, Value)> = RECORDED
        .iter()
        .flat_map(|(name, _)| recorded(name, &cwd))
        .filter(|(event, _)| event == "PostToolUse")
        .map(|(_, payload)| {
            (
                payload["tool_name"].as_str().unwrap().to_string(),
                payload["tool_response"].clone(),
                payload["permission_mode"].clone(),
            )
        })
        .collect();
    assert!(tools
        .iter()
        .all(|(_, response, _)| response.as_str().is_some_and(|r| r.contains(MARK))));
    assert!(tools.iter().any(|(tool, _, _)| tool == "Bash"));
    assert!(tools.iter().any(|(tool, _, _)| tool == QUESTION));
    let mode = |name: &str| -> Vec<Value> {
        recorded(name, &cwd)
            .into_iter()
            .filter(|(_, payload)| payload["session_id"] != TITLE_SESSION)
            .filter_map(|(_, payload)| payload.get("permission_mode").cloned())
            .collect()
    };
    assert!(mode("exec-turn").iter().all(|m| m == "bypassPermissions"));
    assert!(mode("hosted-question").iter().all(|m| m == "default"));
    // The title session: no transcript, and no approvals.
    let title: Vec<Value> = recorded("hosted-approval-refused", &cwd)
        .into_iter()
        .map(|(_, payload)| payload)
        .filter(|payload| payload["session_id"] == TITLE_SESSION)
        .collect();
    assert_eq!(title.len(), 6);
    for payload in &title {
        assert_eq!(payload["transcript_path"], Value::Null);
        if let Some(mode) = payload.get("permission_mode") {
            assert_eq!(mode, "bypassPermissions");
        }
    }
}

#[test]
fn no_content_from_what_codex_sent_reaches_the_spool_the_local_state_or_the_wire() {
    let (rig, cwd, codex) = gate_rig();
    // A later Watch, so the agent's name goes out too.
    let mock = gate::watch_taking(&["agent"]);
    rig.sign_in(&mock.url);
    let main = |kind: &str| (RUN_SESSION.to_string(), kind.to_string());
    let each = |session: &str, kinds: &[&str]| -> Vec<(String, String)> {
        kinds
            .iter()
            .map(|kind| (session.to_string(), kind.to_string()))
            .collect()
    };

    let runs: Vec<(&str, Vec<(String, String)>)> = vec![
        (
            "exec-turn",
            each(
                "00000000-0000-7000-8000-000000000001",
                &[
                    "session_start",
                    "user_prompt_submit",
                    "activity",
                    "stop",
                    "session_end",
                ],
            ),
        ),
        // No hook says the turn failed.
        (
            "exec-failed-turn",
            each(
                "00000000-0000-7000-8000-000000000002",
                &["session_start", "user_prompt_submit", "session_end"],
            ),
        ),
        // Codex's hidden title session records nothing.
        (
            "hosted-approval-refused",
            vec![
                main("session_start"),
                main("user_prompt_submit"),
                main("permission_prompt"),
                main("stop"),
            ],
        ),
        (
            "hosted-question",
            vec![
                main("user_prompt_submit"),
                main("input_prompt"),
                main("activity"),
                main("stop"),
                main("session_end"),
            ],
        ),
        (
            "hosted-approval-given",
            each(
                "00000000-0000-7000-8000-000000000005",
                &[
                    "session_start",
                    "user_prompt_submit",
                    "permission_prompt",
                    "activity",
                    "stop",
                    "session_end",
                ],
            ),
        ),
        (
            "hosted-protocol-decline",
            each(
                "00000000-0000-7000-8000-000000000006",
                &[
                    "session_start",
                    "user_prompt_submit",
                    "permission_prompt",
                    "stop",
                    "session_end",
                ],
            ),
        ),
        // A compaction is not a start, and nothing else here says anything.
        (
            "from-schema",
            each(
                "00000000-0000-7000-8000-000000000099",
                &[
                    "session_start",
                    "session_start",
                    "session_start",
                    "session_start",
                ],
            ),
        ),
    ];
    for (name, expected) in runs {
        let before = rig.spooled().len();
        // Through the hooks file `nucleated hooks print` writes, so its
        // matcher decides which tools start a hook at all.
        for (event, payload) in recorded(name, &cwd) {
            codex.fire_with(&event, &cwd, &payload);
        }
        assert_eq!(spooled_pairs(&rig)[before..], expected[..], "{name}");
    }
    assert_eq!(
        gate::leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the spool"
    );

    gate::tick(&rig);
    // The agent's name went out with the second heartbeat, and nothing else
    // about Codex did.
    let said = gate::said(&mock);
    assert_eq!(said["environments"][0]["agent"], "codex");
    assert!(said["environments"][0].get("agent_version").is_none());
    assert!(said["environments"][0].get("login_expires_at").is_none());
    gate::assert_clean(&rig, &mock);
    let wire = mock.everything();
    let state = state_text(&rig);
    let shown = gate::status_shown(&rig).to_string();
    for word in RECORDED_WORDS {
        assert!(!wire.contains(word), "{word} on the wire");
        assert!(!state.contains(word), "{word} in the local state");
        assert!(!shown.contains(word), "{word} in nucleated status");
    }
    assert!(!rig.home.dir.join("codex-was-started").exists());
}

#[test]
fn each_recorded_codex_run_leaves_its_session_where_codex_left_it() {
    let rig = rig();
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let cwd = rig.session("fix-login");
    let codex = Codex::registered(&rig, &["--env", "client-a"], None);
    // Fire a run's hooks up to the first that matches `until`, then tick.
    let play = |run: &[(String, Value)], from: usize, until: &dyn Fn(&str, &Value) -> bool| {
        let mut at = from;
        while at < run.len() {
            let (event, payload) = &run[at];
            codex.fire_with(event, &cwd, payload);
            at += 1;
            if until(event, payload) {
                break;
            }
        }
        gate::tick(&rig);
        at
    };
    let is = |name: &'static str| move |event: &str, _: &Value| event == name;
    let status = |session: &str| status_of_session(&rig, "client-a", session);

    // A question in Plan mode: needs input until it is answered.
    let run = recorded("hosted-question", &cwd);
    let at = play(&run, 0, &is("PreToolUse"));
    assert_eq!(status(RUN_SESSION), Some(Status::NeedsInput));
    let at = play(&run, at, &is("PostToolUse"));
    assert_eq!(status(RUN_SESSION), Some(Status::Working));
    let at = play(&run, at, &is("Stop"));
    assert_eq!(status(RUN_SESSION), Some(Status::Idle));
    play(&run, at, &is("SessionEnd"));
    assert_eq!(status(RUN_SESSION), Some(Status::Ended));

    // An approval refused with Esc ends the turn: Interrupt, idle.
    let run = recorded("hosted-approval-refused", &cwd);
    let at = play(&run, 0, &is("PermissionRequest"));
    assert_eq!(status(RUN_SESSION), Some(Status::NeedsInput));
    let at = play(&run, at, &is("Interrupt"));
    assert_eq!(status(RUN_SESSION), Some(Status::Idle));
    // The title session beside it, run to its end, never shows.
    play(&run, at, &|_, _| false);
    assert_eq!(status(TITLE_SESSION), None);
    assert!(!mock.everything().contains(TITLE_SESSION));

    // A decline that lets the turn go on: needs input until Stop.
    let session = "00000000-0000-7000-8000-000000000006";
    let run = recorded("hosted-protocol-decline", &cwd);
    let at = play(&run, 0, &is("PermissionRequest"));
    assert_eq!(status(session), Some(Status::NeedsInput));
    let at = play(&run, at, &is("PostModelRequest"));
    assert_eq!(status(session), Some(Status::NeedsInput));
    play(&run, at, &is("Stop"));
    assert_eq!(status(session), Some(Status::Idle));

    // A turn that fails runs no Stop: the session stays working until it
    // ends.
    let session = "00000000-0000-7000-8000-000000000002";
    let run = recorded("exec-failed-turn", &cwd);
    let at = play(&run, 0, &|event, payload| {
        event == "PostModelRequest" && payload["attempt_number"] == 12
    });
    assert_eq!(status(session), Some(Status::Working));
    play(&run, at, &is("SessionEnd"));
    assert_eq!(status(session), Some(Status::Ended));
    gate::assert_clean(&rig, &mock);
}

#[test]
fn a_codex_environment_names_its_agent_to_a_watch_that_takes_it_and_only_then() {
    let (rig, ..) = mixed_box();
    let later = gate::watch_taking(&["agent"]);
    rig.sign_in(&later.url);
    gate::tick(&rig);
    let beats = later.to(contract::HEARTBEATS);
    assert_eq!(beats.len(), 2);
    // The first heartbeat is one every Watch takes; its answer named the
    // field, so the next carries it, for each environment's own agent.
    for environment in beats[0].json()["environments"].as_array().unwrap() {
        assert!(environment.get("agent").is_none());
    }
    let agents: Vec<(String, String)> = beats[1].json()["environments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|environment| {
            (
                environment["env_id"].as_str().unwrap().to_string(),
                environment["agent"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        agents,
        [
            ("client-a".to_string(), "claude-code".to_string()),
            ("codex-a".to_string(), "codex".to_string()),
            ("codex-b".to_string(), "codex".to_string()),
        ]
    );
    gate::assert_clean(&rig, &later);

    // A Watch that does not say it takes the field is never sent it.
    let (rig, ..) = mixed_box();
    let today = gate::watch();
    rig.sign_in(&today.url);
    gate::tick(&rig);
    assert!(!today.to(contract::HEARTBEATS).is_empty());
    for beat in today.to(contract::HEARTBEATS) {
        for environment in beat.json()["environments"].as_array().unwrap() {
            assert!(environment.get("agent").is_none());
        }
    }
    gate::assert_clean(&rig, &today);
}
