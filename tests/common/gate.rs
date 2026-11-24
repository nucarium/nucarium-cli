//! The release gate for the privacy promise, as the parts every test file
//! shares.
//!
//! `tests/privacy.rs` holds the gate for what an agent's hooks hand over and
//! for what the box finds out by itself. Each adapter for a third party, an
//! agent or a driver, and the config sync, adds its own part from its own
//! test file, with these pieces: put [`MARK`] in every content-bearing field
//! its third party can hand over, run the adapter and the daemon against
//! [`watch`], and call [`assert_clean`].
//!
//! If any test built on this fails, content can leave the box. Do not
//! release.

use super::{contract, files_under, run, Mock, Reply, Request, Rig};
use nucleate::daemon::{Daemon, Timings};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

/// The sentinel. It appears in every value that must stay on the box.
pub const MARK: &str = "ZZLEAKZZ";

/// A value for one content-bearing field: the sentinel, the field's name so
/// a leak says where it came from, and words, because content is a sentence.
pub fn mark(field: &str) -> String {
    format!("{MARK} {field} the user typed this")
}

/// Every field an event may carry. There is no other.
pub const EVENT_FIELDS: [&str; 14] = [
    "event_id",
    "env_id",
    "session_id",
    "bridge_session_id",
    "type",
    "repos",
    "branch",
    "error_kind",
    "limit_resets_at",
    "commits",
    "uncommitted_repos",
    "unpushed_commits",
    "pull_requests",
    "client_ts",
];

/// Every field a heartbeat may carry, at its top, about the machine, about
/// an environment and about an environment's unsaved work.
pub const HEARTBEAT_FIELDS: [&str; 6] = [
    "machine",
    "environments",
    "daemon_version",
    "activity_beat_s",
    "paused_until",
    "client_ts",
];
pub const MACHINE_FIELDS: [&str; 9] = [
    "disk_used_pct",
    "load_1m",
    "uptime_s",
    "memory_pressure_pct",
    "on_battery",
    "reboot_required",
    "uncommitted_repos",
    "unpushed_commits",
    "last_snapshot_at",
];
pub const ENVIRONMENT_FIELDS: [&str; 7] = [
    "env_id",
    "server_alive",
    "server_restarts",
    "agent_version",
    "login_expires_at",
    "last_snapshot_at",
    "unsaved",
];
pub const UNSAVED_FIELDS: [&str; 3] = ["uncommitted_repos", "unpushed_commits", "counted_at"];
/// Every field of an environment's setup, and of its script's last run.
pub const SETUP_FIELDS: [&str; 6] = [
    "network",
    "allowed_hosts",
    "repos",
    "variables",
    "secrets",
    "script",
];
pub const SCRIPT_FIELDS: [&str; 4] = ["state", "started_at", "finished_at", "exit_code"];
/// Every field of an environment's plan usage, and of one of its limits.
pub const USAGE_FIELDS: [&str; 2] = ["read_at", "limits"];
pub const LIMIT_FIELDS: [&str; 4] = ["window", "scope", "used_pct", "resets_at"];

/// Every key of every object the box sent is one this file lists, or, about
/// an environment, a field ahead of the contract that the stand-in it was
/// sent to had said it takes.
pub fn only_listed_fields(mock: &Mock) {
    let environment_fields: Vec<&str> = ENVIRONMENT_FIELDS
        .iter()
        .chain(
            contract::AHEAD
                .iter()
                .filter(|field| mock.ahead.contains(field)),
        )
        .copied()
        .collect();
    let listed = |what: &str, object: &Value, fields: &[&str]| {
        for key in object.as_object().expect("an object").keys() {
            assert!(fields.contains(&key.as_str()), "{what} carried {key}");
        }
    };
    for request in mock.to(contract::EVENTS) {
        let body = request.json();
        assert_eq!(body.as_object().unwrap().len(), 1);
        for event in body["events"].as_array().unwrap() {
            listed("an event", event, &EVENT_FIELDS);
        }
    }
    for request in mock.to(contract::HEARTBEATS) {
        let body = request.json();
        listed("the heartbeat", &body, &HEARTBEAT_FIELDS);
        listed("the machine", &body["machine"], &MACHINE_FIELDS);
        for environment in body["environments"].as_array().unwrap() {
            listed("an environment", environment, &environment_fields);
            if let Some(unsaved) = environment.get("unsaved") {
                listed("unsaved", unsaved, &UNSAVED_FIELDS);
            }
            if let Some(setup) = environment.get("setup") {
                listed("setup", setup, &SETUP_FIELDS);
                if let Some(script) = setup.get("script") {
                    listed("setup.script", script, &SCRIPT_FIELDS);
                }
            }
            if let Some(usage) = environment.get("usage") {
                listed("usage", usage, &USAGE_FIELDS);
                for limit in usage["limits"].as_array().into_iter().flatten() {
                    listed("usage.limit", limit, &LIMIT_FIELDS);
                }
            }
        }
    }
}

/// The name of every file and folder under `dir`, as its path from `dir`.
fn names_under(dir: &Path, from: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if let Ok(name) = path.strip_prefix(from) {
            found.push(name.to_string_lossy().to_string());
        }
        // A link is a name and no more: what it points at is not under here.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            names_under(&path, from, found);
        }
    }
}

/// Everywhere the sentinel turns up under `dir`: in what a file holds, and in
/// what a file or a folder is called. A key made from a title or a path, and
/// used as a file's name, is a leak no reading of the files would find.
pub fn leaks_on_disk(dir: &Path) -> Vec<String> {
    let mut leaks: Vec<String> = files_under(dir)
        .into_iter()
        .filter(|(_, bytes)| String::from_utf8_lossy(bytes).contains(MARK))
        .map(|(path, bytes)| format!("{}: {}", path.display(), String::from_utf8_lossy(&bytes)))
        .collect();
    let mut names = Vec::new();
    names_under(dir, dir, &mut names);
    leaks.extend(
        names
            .into_iter()
            .filter(|name| name.contains(MARK))
            .map(|name| format!("a name under {}: {name}", dir.display())),
    );
    leaks
}

/// A stand-in for Watch as it is: it refuses anything the contract does not
/// allow, a field ahead of the contract included.
pub fn watch() -> Mock {
    watch_taking(&[])
}

/// A stand-in for a later Watch, one that takes every field ahead of the
/// contract and says so in its answer to a heartbeat.
pub fn watch_ahead() -> Mock {
    watch_taking(&contract::AHEAD)
}

/// A stand-in for a Watch that takes the `ahead` fields beside the contract,
/// names them in `accepts`, and refuses everything else as Watch does.
pub fn watch_taking(ahead: &'static [&'static str]) -> Mock {
    let mut mock = Mock::start(move |request: &Request| match request.path.as_str() {
        _ if contract::violation_taking(request, ahead).is_some() => {
            Reply::problem(400, "validation_failed")
        }
        contract::HEARTBEATS => {
            let mut answer =
                json!({ "received_at": "2026-11-19T12:00:00.000Z", "heartbeat_interval_s": 60 });
            if !ahead.is_empty() {
                answer["accepts"] = json!(ahead);
            }
            Reply::json(200, answer)
        }
        _ => Reply::json(
            200,
            json!({ "received_at": "2026-11-19T12:00:00.000Z", "accepted": 0, "duplicates": 0, "rejected": [] }),
        ),
    });
    mock.ahead = ahead;
    mock
}

/// What the box last told the stand-in in a heartbeat. To a Watch that takes
/// fields ahead of the contract the first heartbeat goes without them, and
/// the one after its answer carries them.
pub fn said(mock: &Mock) -> Value {
    mock.to(contract::HEARTBEATS)
        .pop()
        .expect("a heartbeat")
        .json()
}

/// Run the daemon's loop over what is in the spool: one turn, and a later
/// one that sends the routine events a batch was waiting for.
pub fn tick(rig: &Rig) -> Daemon {
    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            jitter: false,
            ..Timings::default()
        },
    );
    daemon.tick(Instant::now());
    daemon.tick(Instant::now() + Duration::from_secs(30));
    daemon.write_state(true);
    daemon
}

/// Run the daemon itself, `nucleated run`, as a process of its own until
/// `ready` says so, then stop it as systemd would and hand back what it
/// printed. `path` is the `PATH` it runs with, for a test that puts a
/// stand-in program there to prove the daemon never starts it; with none, it
/// is the test's own.
///
/// [`tick`] turns the daemon's loop inside the test's own process, where no
/// stand-in on a `PATH` can see what it starts, and where the driver and
/// config-sync threads do not run at all. This runs all of it.
pub fn daemon_until(
    rig: &Rig,
    path: Option<&OsStr>,
    what: &str,
    ready: &dyn Fn() -> bool,
) -> Output {
    let mut command = rig.nucleated();
    command.arg("run");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("nucleated run starts");
    let started = Instant::now();
    let mut in_time = true;
    while !ready() {
        if started.elapsed() >= Duration::from_secs(30) {
            in_time = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // `kill` is a shell builtin everywhere; not every system has it as a
    // program.
    let stopped = std::process::Command::new("sh")
        .args(["-c", &format!("kill -TERM {}", child.id())])
        .status()
        .expect("sh runs");
    let out = child.wait_with_output().expect("the daemon exits");
    assert!(
        in_time,
        "timed out waiting for {what}:\n{}",
        super::stderr(&out)
    );
    assert!(stopped.success());
    assert_eq!(out.status.code(), Some(0), "{}", super::stderr(&out));
    out
}

/// What `nucleated status --json` prints, less each environment's `root`:
/// that path is the owner's own config, shown back to them.
pub fn status_shown(rig: &Rig) -> Value {
    let status = run(rig.nucleated().args(["status", "--json"]), "");
    assert!(status.status.success(), "{}", super::stderr(&status));
    let mut shown: Value = serde_json::from_slice(&status.stdout).unwrap();
    for environment in shown["environments"].as_array_mut().into_iter().flatten() {
        environment["root"] = Value::Null;
    }
    shown
}

/// The gate itself. After a fixture full of the sentinel has been through an
/// adapter and the daemon has ticked against `mock`: the sentinel is in no
/// request, in no file under the state folder and in nothing `nucleated
/// status` prints; nothing sent is something Watch would refuse; and every
/// field sent is one this file lists.
pub fn assert_clean(rig: &Rig, mock: &Mock) {
    let sent = mock.everything();
    assert!(!sent.contains(MARK), "content on the wire:\n{sent}");
    assert_eq!(contract::violations(mock), Vec::<String>::new());
    only_listed_fields(mock);
    assert_eq!(
        leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new(),
        "content in the local state"
    );
    let shown = status_shown(rig);
    assert!(
        !shown.to_string().contains(MARK),
        "content in nucleated status:\n{shown}"
    );
}

/// A gate is only worth something if its net has no holes: a fixture really
/// is full of the sentinel, in every field named, and the scanner really
/// finds a sentinel on disk. Call this with each fixture before trusting it.
pub fn assert_fixture_would_catch_a_leak(fixture: &str, at_least: usize, fields: &[&str]) {
    assert!(
        fixture.matches(MARK).count() >= at_least,
        "the fixture lost its sentinels"
    );
    for field in fields {
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
    // And a sentinel in the name of a file, or of an empty folder.
    std::fs::write(dir.path().join(format!("spool/{MARK}-key.json")), "{}").unwrap();
    assert_eq!(leaks_on_disk(dir.path()).len(), 2);
    std::fs::create_dir_all(dir.path().join("claims").join(format!("wt-{MARK}"))).unwrap();
    assert_eq!(leaks_on_disk(dir.path()).len(), 3);
}
