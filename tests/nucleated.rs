//! The `nucleated` binary as the box runs it: the hook entry point, the
//! daemon end to end, the local status view, `check`, the unit and hook
//! blocks, and housekeeping against real repositories.

mod common;

use common::{contract, git, mode, run, stderr, stdout_json, Home, Mock, Reply, Request, Rig};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn watch() -> Mock {
    Mock::start(|request: &Request| match request.path.as_str() {
        // As Watch does, refuse anything the contract does not allow.
        _ if contract::violation(request).is_some() => Reply::problem(400, "validation_failed"),
        "/v1/heartbeats:ingest" => Reply::json(
            200,
            json!({ "received_at": "2026-11-19T12:00:00.000Z", "heartbeat_interval_s": 60 }),
        ),
        "/v1/events:ingest" => Reply::json(
            200,
            json!({ "received_at": "2026-11-19T12:00:00.000Z", "accepted": 1, "duplicates": 0, "rejected": [] }),
        ),
        _ => Reply::problem(404, "not_found"),
    })
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn signal(pid: u32, name: &str) {
    // `kill` is a shell builtin everywhere; not every system has it as a program.
    let sent = Command::new("sh")
        .args(["-c", &format!("kill {name} {pid}")])
        .status()
        .unwrap();
    assert!(sent.success());
}

/// The text of the first fenced block of `language` after `heading` in the README.
fn readme_block(heading: &str, language: &str) -> String {
    let readme =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap();
    let section = readme
        .split(heading)
        .nth(1)
        .expect("the heading is in the README");
    let block = section
        .split(&format!("```{language}\n"))
        .nth(1)
        .expect("the block is in the section");
    block.split("```").next().unwrap().to_string()
}

// ---- the hook entry point ---------------------------------------------

#[test]
fn the_hook_writes_one_private_spool_file_and_prints_nothing() {
    let rig = Rig::new();
    let worktree = rig.session("fix-login");
    let out = rig.hook(
        "Notification",
        &json!({ "session_id": "s1", "cwd": worktree, "notification_type": "permission_prompt" }),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
    let spooled = rig.spooled();
    assert_eq!(spooled.len(), 1);
    assert_eq!(spooled[0]["event"]["type"], "permission_prompt");
    let files = common::files_under(&rig.spool_dir());
    assert_eq!(files.len(), 1);
    assert_eq!(mode(&files[0].0), 0o600);
    // Named for its event id, with nothing left behind from the write.
    let name = files[0]
        .0
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(
        name,
        format!("{}.json", spooled[0]["event"]["event_id"].as_str().unwrap())
    );
}

#[test]
fn the_hook_exits_0_whatever_it_is_given() {
    let rig = Rig::new();
    for (event, input) in [
        ("Stop", "not json at all"),
        ("Stop", ""),
        ("Stop", "{\"session_id\": \"s1\""),
        ("Stop", "[]"),
        ("Stop", "{\"session_id\": 12}"),
        ("NoSuchEvent", "{\"session_id\": \"s1\"}"),
        ("", "{}"),
    ] {
        let out = run(rig.nucleated().args(["hook", event]), input);
        assert_eq!(out.status.code(), Some(0), "{event:?} {input:?}");
        assert!(
            out.stdout.is_empty() && out.stderr.is_empty(),
            "{event:?} {input:?}"
        );
    }
    assert!(rig.spooled().is_empty());
}

#[test]
fn the_hook_reads_its_input_to_the_end_even_for_an_event_it_has_no_use_for() {
    use std::io::Write;
    let rig = Rig::new();
    // More than a pipe holds, so the write cannot finish unless it is read.
    let payload = json!({ "session_id": "s1", "filler": "x".repeat(1 << 20) }).to_string();
    for event in ["SomethingNotInventedYet", "Stop"] {
        let mut child = rig
            .nucleated()
            .args(["hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let written = child.stdin.take().unwrap().write_all(payload.as_bytes());
        let out = child.wait_with_output().unwrap();
        assert!(written.is_ok(), "{event}: the agent was told {written:?}");
        assert_eq!(out.status.code(), Some(0), "{event}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{event}");
    }
}

#[test]
fn the_hook_exits_0_with_no_config_a_broken_config_or_an_unknown_environment() {
    let home = Home::new();
    let payload = json!({ "session_id": "s1" }).to_string();
    let quiet = |out: std::process::Output| {
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    };
    quiet(run(home.nucleated().args(["hook", "Stop"]), &payload));
    home.write_config("this is not [ toml");
    quiet(run(home.nucleated().args(["hook", "Stop"]), &payload));
    let rig = Rig::new();
    quiet(run(
        rig.nucleated().args(["hook", "Stop", "--env", "nowhere"]),
        &payload,
    ));
    assert!(rig.spooled().is_empty());
    // A state folder it cannot write to is not the session's problem either.
    rig.home.write_config(&format!(
        "state_dir = \"/proc/nonexistent/state\"\n[[environment]]\nname = \"client-a\"\nroot = \"{}\"\n",
        rig.root.display()
    ));
    quiet(run(rig.nucleated().args(["hook", "Stop"]), &payload));
}

#[test]
fn the_hook_gives_up_within_its_limit_when_its_input_never_ends() {
    let rig = Rig::new();
    let started = Instant::now();
    let mut child = rig
        .nucleated()
        .args(["hook", "Stop"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Hold the pipe open and never write to it.
    let held = child.stdin.take();
    let status = child.wait().unwrap();
    drop(held);
    assert_eq!(status.code(), Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_hook_finds_its_environment_by_flag_config_folder_or_location() {
    let rig = Rig::new();
    let other = rig.home.dir.join("ops");
    fs::create_dir_all(other.join(".repos")).unwrap();
    let ops_config = rig.home.dir.join("claude-ops");
    fs::create_dir_all(&ops_config).unwrap();
    rig.configure(&format!(
        "[[environment]]\nname = \"ops\"\nroot = \"{}\"\nconfig_dir = \"{}\"\n",
        other.display(),
        ops_config.display()
    ));
    let payload = |cwd: &Path| json!({ "session_id": "s1", "cwd": cwd }).to_string();
    let env_of = |n: usize| {
        rig.spooled()[n]["event"]["env_id"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // By where the session is working.
    run(rig.nucleated().args(["hook", "Stop"]), &payload(&rig.root));
    assert_eq!(env_of(0), "client-a");
    run(rig.nucleated().args(["hook", "Stop"]), &payload(&other));
    assert_eq!(env_of(1), "ops");
    // By that login's config folder, wherever the session is working.
    run(
        rig.nucleated()
            .args(["hook", "Stop"])
            .env("CLAUDE_CONFIG_DIR", &ops_config),
        &payload(&rig.root),
    );
    assert_eq!(env_of(2), "ops");
    // By name, over both.
    run(
        rig.nucleated()
            .args(["hook", "Stop", "--env", "client-a"])
            .env("CLAUDE_CONFIG_DIR", &ops_config),
        &payload(&other),
    );
    assert_eq!(env_of(3), "client-a");
    // With two environments and nothing to tell them apart, nothing is recorded.
    run(
        rig.nucleated().args(["hook", "Stop"]),
        &payload(&rig.home.dir),
    );
    assert_eq!(rig.spooled().len(), 4);
}

// ---- the daemon, end to end -------------------------------------------

#[test]
fn the_daemon_reports_a_heartbeat_and_a_waiting_session_then_stops_cleanly() {
    let rig = Rig::new();
    let mock = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let child = rig
        .nucleated()
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for("the first heartbeat", || {
        !mock.to("/v1/heartbeats:ingest").is_empty()
    });

    let out = rig.hook(
        "Notification",
        &json!({ "session_id": "s1", "cwd": worktree, "notification_type": "permission_prompt" }),
    );
    assert_eq!(out.status.code(), Some(0));
    let asked = Instant::now();
    wait_for("the event", || !mock.to("/v1/events:ingest").is_empty());
    // "At once": well inside the ten seconds a routine event would wait.
    assert!(asked.elapsed() < Duration::from_secs(5));
    let event = mock.to("/v1/events:ingest")[0].json()["events"][0].clone();
    assert_eq!(event["type"], "permission_prompt");
    assert_eq!(event["env_id"], "client-a");
    assert_eq!(event["branch"], "workspace/fix-login");
    wait_for("the spool to empty", || rig.spooled().is_empty());

    // The local view agrees with what was sent.
    let view = stdout_json(&run(rig.nucleated().args(["status", "--json"]), ""));
    assert_eq!(view["daemon"]["running"], true);
    assert_eq!(view["watch"]["target"], "hosted");
    assert_eq!(view["inbox"][0]["attention"], "needs_input");
    assert_eq!(view["inbox"][0]["needs_kind"], "permission");

    signal(child.id(), "-TERM");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    let log = stderr(&out);
    assert!(log.contains("heartbeat accepted"));
    assert!(log.contains("stopped"));
    assert!(!log.contains(common::TOKEN));
    let state: Value = serde_json::from_str(
        &fs::read_to_string(rig.home.state_dir().join("daemon.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["running"], false);
}

#[test]
fn the_daemon_exits_1_when_its_config_cannot_be_read() {
    let home = Home::new();
    home.write_config("[[environment]]\nname = \"Not A Valid Name\"\nroot = \"/x\"\n");
    let out = run(home.nucleated().arg("run"), "");
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("environment name"));
}

// ---- status -----------------------------------------------------------

/// Apply what the hooks wrote, as the running daemon would.
fn apply(rig: &Rig) {
    let mut daemon =
        nucleate::daemon::Daemon::new(rig.settings(), nucleate::daemon::Timings::default());
    daemon.tick(Instant::now());
}

#[test]
fn status_shows_what_watch_would_for_a_box_with_no_account() {
    let rig = Rig::new();
    let worktree = rig.session("fix-login");
    let event = |event: &str, session: &str, extra: Value| {
        let mut payload = json!({ "session_id": session, "cwd": worktree });
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            payload[key] = value;
        }
        assert!(rig.hook(event, &payload).status.success());
    };
    event("UserPromptSubmit", "working-session", json!({}));
    event("Stop", "idle-session", json!({}));
    event(
        "StopFailure",
        "failed-session",
        json!({ "error": "rate_limit" }),
    );
    event(
        "Notification",
        "waiting-session",
        json!({ "notification_type": "permission_prompt" }),
    );
    event("SessionEnd", "ended-session", json!({}));
    apply(&rig);

    let out = run(rig.nucleated().args(["status", "--json"]), "");
    assert!(out.status.success(), "{}", stderr(&out));
    let view = stdout_json(&out);
    // Most urgent first, as Watch orders its inbox; ended sessions are gone.
    let order: Vec<(&str, &str)> = view["inbox"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["attention"].as_str().unwrap(),
                item["session_id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("needs_input", "waiting-session"),
            ("failed", "failed-session"),
            ("working", "working-session"),
            ("idle", "idle-session"),
        ]
    );
    assert_eq!(view["inbox"][1]["error_kind"], "rate_limit");
    assert_eq!(view["inbox"][0]["repos"], json!(["web"]));
    assert_eq!(view["inbox"][0]["branch"], "workspace/fix-login");
    assert_eq!(view["watch"]["target"], "none");
    assert_eq!(view["watch"]["signed_in"], false);
    assert_eq!(view["machine"]["name"], "agentbox");
    assert!(view["machine"]["disk_used_pct"].as_f64().unwrap() > 0.0);
    assert_eq!(view["environments"][0]["name"], "client-a");
    assert_eq!(view["environments"][0]["server_alive"], true);

    let text = String::from_utf8(run(rig.nucleated().arg("status"), "").stdout).unwrap();
    assert!(text.starts_with("agentbox · disk "), "{text}");
    assert!(text.contains("Watch: not signed in. Nothing leaves this box."));
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle} is not in:\n{text}"))
    };
    assert!(
        at("Needs you") < at("Failed")
            && at("Failed") < at("Working")
            && at("Working") < at("Idle")
    );
    assert!(text.contains("client-a  waiting-  permission"));
    assert!(text.contains("rate_limit"));
    assert!(!text.contains("ended-se"));
}

#[test]
fn status_says_when_an_agent_server_is_down_and_when_nothing_is_configured() {
    let rig = Rig::new();
    rig.configure_with("check = \"false\"", "");
    let view = stdout_json(&run(rig.nucleated().args(["status", "--json"]), ""));
    assert_eq!(view["environments"][0]["server_alive"], false);
    assert_eq!(view["inbox"][0]["attention"], "down");
    let text = String::from_utf8(run(rig.nucleated().arg("status"), "").stdout).unwrap();
    assert!(
        text.contains("client-a  agent server is not running"),
        "{text}"
    );

    let empty = Home::new();
    let out = run(empty.nucleated().arg("status"), "");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("No environments are configured"));
}

// ---- check ------------------------------------------------------------

#[test]
fn check_passes_on_a_box_that_is_set_up_and_sends_one_heartbeat() {
    let rig = Rig::new();
    let mock = watch();
    rig.sign_in(&mock.url);
    let out = run(rig.nucleated().arg("check"), "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let report = stdout_json(&out);
    assert_eq!(report["ok"], true);
    let names: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "config",
            "token",
            "client-a: nucleus",
            "client-a: agent server",
            "heartbeat"
        ]
    );
    assert_eq!(mock.to("/v1/heartbeats:ingest").len(), 1);
}

#[test]
fn check_names_what_is_wrong_and_exits_1() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let mock = Mock::start(|_: &Request| Reply::problem(401, "unauthorized"));
    rig.sign_in(&mock.url);
    let out = run(rig.nucleated().arg("check"), "");
    assert_eq!(out.status.code(), Some(1));
    let failed = |report: &Value| -> Vec<String> {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|check| check["ok"] == false)
            .map(|check| check["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(failed(&stdout_json(&out)), ["heartbeat"]);

    fs::set_permissions(rig.home.token_file(), fs::Permissions::from_mode(0o644)).unwrap();
    rig.home.write_config(&format!(
        "state_dir = \"{}\"\n[[environment]]\nname = \"client-a\"\nroot = \"{}\"\ncheck = \"true\"\n",
        rig.home.state_dir().display(),
        rig.home.dir.join("not-a-nucleus").display()
    ));
    let out = run(rig.nucleated().arg("check"), "");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(failed(&stdout_json(&out)), ["token", "client-a: nucleus"]);

    rig.home.write_config("target = \"sideways\"\n");
    let out = run(rig.nucleated().arg("check"), "");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(failed(&stdout_json(&out)), ["config"]);
}

#[test]
fn check_on_a_box_with_no_account_passes_and_sends_nothing() {
    let rig = Rig::new();
    let out = run(rig.nucleated().arg("check"), "");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stdout_json(&out)["checks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["name"] != "heartbeat"));
}

// ---- init, unit, hooks print ------------------------------------------

#[test]
fn init_writes_a_first_config_the_daemon_can_read() {
    let home = Home::new();
    let root = home.dir.join("Client A");
    fs::create_dir_all(&root).unwrap();
    let out = run(
        home.nucleated().args([
            "init",
            "--root",
            root.to_str().unwrap(),
            "--unit",
            "agents.service",
        ]),
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["created"], true);
    assert_eq!(
        stdout_json(&out)["config"],
        home.config_file().to_str().unwrap()
    );
    let settings = nucleate::settings::load_from(&home.config_file()).unwrap();
    assert_eq!(settings.environments.len(), 1);
    // A name Watch accepts, made from the folder's.
    assert_eq!(settings.environments[0].name, "client-a");
    assert_eq!(settings.environments[0].root, root);
    assert_eq!(settings.environments[0].unit, "agents.service");

    let before = fs::read_to_string(home.config_file()).unwrap();
    let again = run(home.nucleated().args(["init", "--env", "other"]), "");
    assert_eq!(stdout_json(&again)["created"], false);
    assert_eq!(fs::read_to_string(home.config_file()).unwrap(), before);
}

#[test]
fn the_unit_runs_the_daemon_as_a_user_service_and_asks_for_no_privileges() {
    let home = Home::new();
    let unit = String::from_utf8(run(home.nucleated().arg("unit"), "").stdout).unwrap();
    let has = |line: &str| unit.lines().any(|l| l == line);
    assert!(has("ExecStart=%h/.local/bin/nucleated run"));
    assert!(has("Type=simple"));
    assert!(has("Restart=always"));
    assert!(has("RestartSec=5"));
    assert!(has("NoNewPrivileges=yes"));
    // A user unit: started for the agent user, never as root or another user.
    assert!(has("WantedBy=default.target"));
    for forbidden in [
        "User=",
        "Group=",
        "sudo",
        "multi-user.target",
        "CapabilityBoundingSet",
        "AmbientCapabilities",
    ] {
        assert!(!unit.contains(forbidden), "the unit has {forbidden}");
    }
    // Every line is a section header, a key=value, or blank.
    for line in unit.lines().filter(|line| !line.is_empty()) {
        assert!(
            line.starts_with('[') && line.ends_with(']') || line.contains('='),
            "{line}"
        );
    }
    assert_eq!(
        unit,
        readme_block("### The systemd user unit", "ini"),
        "the README's unit has drifted"
    );
}

#[test]
fn hooks_print_is_the_block_the_readme_documents() {
    let rig = Rig::new();
    let printed = stdout_json(&run(rig.nucleated().args(["hooks", "print"]), ""));
    let documented: Value = serde_json::from_str(&readme_block("### Hooks", "json")).unwrap();
    assert_eq!(printed, documented, "the README's hooks block has drifted");

    let named = stdout_json(&run(
        rig.nucleated()
            .args(["hooks", "print", "--env", "client-a"]),
        "",
    ));
    assert_eq!(
        named["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "nucleated hook SessionStart --env client-a"
    );
    let unknown = run(
        rig.nucleated().args(["hooks", "print", "--env", "nowhere"]),
        "",
    );
    assert_eq!(unknown.status.code(), Some(1));
}

// ---- housekeeping -----------------------------------------------------

fn housekeeping(rig: &Rig, args: &[&str]) -> Value {
    let out = run(rig.nucleated().args(["housekeeping", "run"]).args(args), "");
    assert!(
        out.status.success(),
        "{}\n{}",
        stderr(&out),
        String::from_utf8_lossy(&out.stdout)
    );
    stdout_json(&out)[0].clone()
}

/// Set a file's modification time well into the past, so it no longer looks
/// like something being written.
fn age(path: &Path) {
    assert!(Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(path)
        .status()
        .unwrap()
        .success());
}

#[test]
fn refresh_fetches_every_repo() {
    let rig = Rig::new();
    fs::write(rig.upstream.join("NEW.md"), "new\n").unwrap();
    git(&rig.upstream, &["add", "."]);
    git(&rig.upstream, &["commit", "-q", "-m", "more"]);
    let report = housekeeping(&rig, &["refresh"]);
    assert_eq!(report["job"], "refresh");
    assert_eq!(report["environment"], "client-a");
    assert_eq!(report["repos"], json!([{ "repo": "web", "ok": true }]));
    assert_eq!(
        git(&rig.bare(), &["rev-parse", "refs/remotes/origin/main"]),
        git(&rig.upstream, &["rev-parse", "HEAD"])
    );
}

#[test]
fn refresh_moves_a_nucleus_made_before_owner_folders() {
    let rig = Rig::new();
    let worktree = rig.session("fix-login");
    let flat = rig.flatten();
    let report = housekeeping(&rig, &["refresh"]);
    assert_eq!(report["repos"], json!([{ "repo": "web", "ok": true }]));
    assert!(!flat.exists());
    let moved = rig.root.join(".repos/_/web.git");
    assert!(report["moved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line.as_str().unwrap().contains(moved.to_str().unwrap())));
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    // A dry run moves nothing.
    rig.flatten();
    let out = run(
        rig.nucleated()
            .args(["housekeeping", "run", "refresh", "--dry-run"]),
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(flat.is_dir());
}

#[test]
fn the_daemon_moves_a_nucleus_made_before_owner_folders_when_it_starts() {
    let rig = Rig::new();
    let mock = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let flat = rig.flatten();
    let child = rig
        .nucleated()
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let moved = rig.root.join(".repos/_/web.git");
    wait_for("the move", || moved.is_dir() && !flat.exists());
    wait_for("the first heartbeat", || {
        !mock.to("/v1/heartbeats:ingest").is_empty()
    });
    signal(child.id(), "-TERM");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains(&format!("client-a: moved {}", flat.display())));
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
}

#[test]
fn refresh_signs_in_to_github_with_the_environments_token_file() {
    let rig = Rig::new();
    fs::write(rig.upstream.join("NEW.md"), "new\n").unwrap();
    git(&rig.upstream, &["add", "."]);
    git(&rig.upstream, &["commit", "-q", "-m", "more"]);
    git(&rig.upstream, &["update-server-info"]);
    // GitHub's stand-in takes `x-access-token` with this environment's token
    // and nothing else.
    let github = common::git_host(
        &rig.upstream.join(".git"),
        "Basic eC1hY2Nlc3MtdG9rZW46Z2hzX2NsaWVudF9hX3Rva2Vu",
    );
    let bare = rig.bare();
    git(
        &bare,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{github}/octo/web.git"),
        ],
    );
    let token_file = rig.home.dir.join("client-a-github-token");
    fs::write(&token_file, "ghs_client_a_token\n").unwrap();
    rig.configure_with(
        &format!(
            "check = \"true\"\ngithub_token_file = \"{}\"",
            token_file.display()
        ),
        "",
    );
    // The box-wide helper signs in as another account.
    fs::write(
        rig.home.gitconfig(),
        format!(
            "[credential \"{github}\"]\n\thelper = \"!f() {{ echo username=x-access-token; \
             echo password=ghs_another_accounts_token; }}; f\"\n"
        ),
    )
    .unwrap();

    let out = run(
        rig.nucleated()
            .env("NUCLEATE_GITHUB_API_URL", &github)
            .args(["housekeeping", "run", "refresh"]),
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let report = &stdout_json(&out)[0];
    assert_eq!(report["repos"], json!([{ "repo": "web", "ok": true }]));
    assert_eq!(
        git(&bare, &["rev-parse", "refs/remotes/origin/main"]),
        git(&rig.upstream, &["rev-parse", "HEAD"])
    );
}

#[test]
fn snapshot_pushes_uncommitted_work_to_a_hidden_ref_and_touches_nothing() {
    let rig = Rig::new();
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("README.md"), "web, edited\n").unwrap();
    fs::write(worktree.join("notes.txt"), "untracked work\n").unwrap();
    fs::write(worktree.join(".gitignore"), "secret.env\n").unwrap();
    fs::write(worktree.join("secret.env"), "ignored\n").unwrap();
    for file in ["README.md", "notes.txt", ".gitignore", "secret.env"] {
        age(&worktree.join(file));
    }
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    let status = git(&worktree, &["status", "--porcelain"]);

    // A dry run says what it would do and pushes nothing.
    let planned = housekeeping(&rig, &["snapshot", "--dry-run"]);
    assert_eq!(planned["worktrees"][0]["action"], "would_snapshot");
    assert!(git(&rig.upstream, &["for-each-ref", "refs/wip"]).is_empty());

    let report = housekeeping(&rig, &["snapshot"]);
    let entry = &report["worktrees"][0];
    assert_eq!(entry["action"], "snapshot", "{report}");
    assert_eq!(entry["branch"], "workspace/fix-login");
    assert_eq!(entry["ref"], "refs/wip/workspace-fix-login");

    // The remote now holds the work, on a ref that is not a branch.
    let reference = "refs/wip/workspace-fix-login";
    assert_eq!(
        git(&rig.upstream, &["show", &format!("{reference}:README.md")]),
        "web, edited"
    );
    assert_eq!(
        git(&rig.upstream, &["show", &format!("{reference}:notes.txt")]),
        "untracked work"
    );
    assert_eq!(
        git(&rig.upstream, &["rev-parse", &format!("{reference}^")]),
        head
    );
    assert!(git(&rig.upstream, &["branch", "--list", "*wip*"]).is_empty());
    // Ignored files are not captured.
    let tree = git(&rig.upstream, &["ls-tree", "--name-only", reference]);
    assert!(!tree.contains("secret.env"), "{tree}");

    // The session is exactly as it was: same commit, same uncommitted changes.
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&worktree, &["status", "--porcelain"]), status);
    assert_eq!(
        git(&worktree, &["branch", "--show-current"]),
        "workspace/fix-login"
    );

    // Nothing changed since, so nothing is pushed again.
    let again = housekeeping(&rig, &["snapshot"]);
    assert_eq!(again["worktrees"][0]["action"], "skipped");
    assert_eq!(again["worktrees"][0]["reason"], "already snapshotted");
}

#[test]
fn snapshot_leaves_alone_a_worktree_that_is_clean_or_being_written_to() {
    let rig = Rig::new();
    let worktree = rig.session("fix-login");
    let clean = housekeeping(&rig, &["snapshot"]);
    assert_eq!(clean["worktrees"][0]["action"], "skipped");
    assert_eq!(clean["worktrees"][0]["reason"], "nothing uncommitted");

    // Written a moment ago: the agent may be in the middle of something.
    fs::write(worktree.join("half-written.txt"), "half").unwrap();
    let busy = housekeeping(&rig, &["snapshot"]);
    assert_eq!(busy["worktrees"][0]["action"], "skipped");
    assert_eq!(busy["worktrees"][0]["reason"], "being written to");
    assert!(git(&rig.upstream, &["for-each-ref", "refs/wip"]).is_empty());
}

#[test]
fn cleanup_keeps_a_session_whose_agent_is_still_live_or_whose_work_is_unsaved() {
    let rig = Rig::new();
    rig.configure("[housekeeping]\ncleanup_min_age = \"0s\"\n");
    let live = rig.session("live");
    let dirty = rig.session("dirty");
    fs::write(dirty.join("unsaved.txt"), "only here\n").unwrap();
    assert!(rig
        .hook(
            "UserPromptSubmit",
            &json!({ "session_id": "s1", "cwd": live })
        )
        .status
        .success());
    apply(&rig);

    let report = housekeeping(&rig, &["cleanup"]);
    let verdict = |name: &str| -> Value {
        report["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["session"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(verdict("live")["action"], "kept");
    assert_eq!(verdict("live")["reason"], "its agent session has not ended");
    assert_eq!(verdict("dirty")["action"], "kept");
    assert!(live.is_dir() && dirty.join("unsaved.txt").is_file());
}

#[test]
fn cleanup_keeps_a_session_that_changed_too_recently() {
    let rig = Rig::new();
    rig.session("fresh");
    // The default: nothing younger than seven days is touched.
    let report = housekeeping(&rig, &["cleanup"]);
    assert_eq!(report["sessions"][0]["action"], "kept");
    assert!(rig.root.join(".worktrees/fresh").is_dir());
}

/// Whether a process is inside a session is read from /proc, so removal can
/// only be shown where there is one.
#[cfg(target_os = "linux")]
#[test]
fn cleanup_removes_a_finished_session_that_holds_nothing_unsaved() {
    let rig = Rig::new();
    rig.configure("[housekeeping]\ncleanup_min_age = \"0s\"\n");
    let finished = rig.session("finished");
    let dirty = rig.session("dirty");
    fs::write(dirty.join("unsaved.txt"), "only here\n").unwrap();
    for (event, session) in [("UserPromptSubmit", "s1"), ("SessionEnd", "s1")] {
        assert!(rig
            .hook(event, &json!({ "session_id": session, "cwd": finished }))
            .status
            .success());
    }
    apply(&rig);

    let planned = housekeeping(&rig, &["cleanup", "--dry-run"]);
    assert_eq!(planned["sessions"][1]["action"], "would_remove");
    assert!(finished.is_dir());

    let report = housekeeping(&rig, &["cleanup"]);
    assert_eq!(
        report["sessions"],
        json!([
            { "session": "dirty", "action": "kept", "reason": "web: uncommitted changes" },
            { "session": "finished", "action": "removed" },
        ])
    );
    assert!(!rig.root.join(".worktrees/finished").exists());
    assert!(dirty.join("unsaved.txt").is_file());
    // The bare repo no longer lists the worktree that was removed.
    let listed = git(&rig.bare(), &["worktree", "list"]);
    assert!(!listed.contains("finished"), "{listed}");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn cleanup_removes_nothing_where_it_cannot_tell_what_is_in_use() {
    let rig = Rig::new();
    rig.configure("[housekeeping]\ncleanup_min_age = \"0s\"\n");
    let finished = rig.session("finished");
    let report = housekeeping(&rig, &["cleanup"]);
    assert_eq!(report["sessions"][0]["action"], "kept");
    assert!(finished.is_dir());
}

#[test]
fn an_unknown_job_or_environment_is_an_error() {
    let rig = Rig::new();
    for args in [vec!["polish"], vec!["refresh", "--env", "nowhere"]] {
        let out = run(
            rig.nucleated().args(["housekeeping", "run"]).args(&args),
            "",
        );
        assert_eq!(out.status.code(), Some(1), "{args:?}");
    }
}

// ---- two logins, one nucleus ------------------------------------------

/// A box where a second login, `client-b`, shares `client-a`'s nucleus.
fn shared_nucleus() -> Rig {
    let rig = Rig::new();
    rig.configure(&format!(
        "[housekeeping]\ncleanup_min_age = \"0s\"\n[[environment]]\nname = \"client-b\"\nroot = \"{}\"\ncheck = \"true\"\n",
        rig.root.display()
    ));
    rig
}

#[test]
fn a_session_of_another_login_sharing_the_nucleus_is_never_cleaned_up_as_finished() {
    let rig = shared_nucleus();
    let theirs = rig.session("theirs");
    let payload = json!({ "session_id": "s1", "cwd": theirs }).to_string();
    let out = run(
        rig.nucleated()
            .args(["hook", "UserPromptSubmit", "--env", "client-b"]),
        &payload,
    );
    assert!(out.status.success());
    apply(&rig);
    // Asked about client-a's nucleus, which is also client-b's.
    let report = housekeeping(&rig, &["cleanup", "--env", "client-a"]);
    assert_eq!(report["sessions"][0]["action"], "kept");
    assert_eq!(
        report["sessions"][0]["reason"],
        "its agent session has not ended"
    );
    assert!(theirs.is_dir());
}

#[test]
fn a_job_over_a_shared_nucleus_runs_once() {
    let rig = shared_nucleus();
    let out = run(rig.nucleated().args(["housekeeping", "run", "refresh"]), "");
    let reports = stdout_json(&out);
    assert_eq!(reports.as_array().unwrap().len(), 1);
    assert_eq!(reports[0]["environment"], "client-a");
}

#[test]
fn a_hook_cannot_tell_two_logins_sharing_a_nucleus_apart_by_location() {
    let rig = shared_nucleus();
    let payload = json!({ "session_id": "s1", "cwd": rig.root }).to_string();
    run(rig.nucleated().args(["hook", "Stop"]), &payload);
    assert!(rig.spooled().is_empty());
    run(
        rig.nucleated().args(["hook", "Stop", "--env", "client-b"]),
        &payload,
    );
    assert_eq!(rig.spooled()[0]["event"]["env_id"], "client-b");
}
