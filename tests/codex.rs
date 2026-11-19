//! Codex: what the adapter does, and its part of the privacy gate.
//!
//! The gate's shared pieces are in `common::gate`. This file holds a stand-in
//! for Codex's hook engine, built from what Codex's own source says a hooks
//! file and a hook payload are, and the adapter's fixture: a payload for
//! every Codex hook event with the sentinel in every field that carries
//! content.
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
                "tool_response" => json!({
                    "stdout": gate::mark("tool_response.stdout"),
                    "stderr": gate::mark("tool_response.stderr"),
                    "files": [{ "path": format!("/x/{MARK}_tool_response_path"), "content": gate::mark("tool_response.files.content") }],
                }),
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

    /// A lifecycle event happens: every command registered for it runs. A
    /// hook may never fail, print or complain. Returns how many ran.
    fn fire_with(&self, event: &str, cwd: &Path, payload: &Value) -> usize {
        let mut ran = 0;
        for group in self.hooks[event].as_array().into_iter().flatten() {
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
            "permission_prompt",
            "session_end",
            "session_start",
            "stop",
            "stop",
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
