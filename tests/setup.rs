//! Environment setup: what the box recipe says an environment is set up
//! with, read from its two files and carried in the heartbeat to a Watch that
//! takes it; and its part of the privacy gate.
//!
//! The test plays the recipe and the runner: it writes `<name>.json` and
//! `<name>.setup.json` into the rig's setup folder, as they would.

mod common;

use common::gate::{self, MARK};
use common::{contract, stderr, Mock, Rig};
use nucleate::daemon::{Daemon, Timings};
use nucleate::envelope::{rfc3339, unix_ms};
use serde_json::{json, Value};
use std::fs;
use std::time::{Duration, Instant, SystemTime};

const HASH: &str = "9f2c1e7a5b3d4c6e8f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6";

fn defined() -> Value {
    json!({
        "network": "full",
        "allowed_hosts": 0,
        "repos": ["your-org/web", "your-org/api"],
        "variables": 2,
        "secrets": 0,
        "setup_script": true,
    })
}

/// Write the environment's two files, as the recipe and the runner do.
fn write(rig: &Rig, defined: Option<&Value>, run: Option<&Value>) {
    let dir = rig.home.setup_dir();
    fs::create_dir_all(&dir).unwrap();
    let put = |name: &str, value: Option<&Value>| {
        let path = dir.join(name);
        match value {
            Some(value) => fs::write(path, value.to_string()).unwrap(),
            None => {
                let _ = fs::remove_file(path);
            }
        }
    };
    put("client-a.json", defined);
    put("client-a.setup.json", run);
}

/// A run that started `ago` minutes ago and took 42 seconds.
fn run(state: &str, ago: u64, exit_code: Option<u8>) -> Value {
    let started = unix_ms(SystemTime::now()) - ago * 60_000;
    let mut run = json!({
        "state": state,
        "started_at_ms": started,
        "script_sha256": HASH,
    });
    if state != "running" {
        run["finished_at_ms"] = json!(started + 42_000);
    }
    if let Some(code) = exit_code {
        run["exit_code"] = json!(code);
    }
    run
}

fn daemon(rig: &Rig) -> Daemon {
    Daemon::new(
        rig.settings(),
        Timings {
            jitter: false,
            ..Timings::default()
        },
    )
}

fn heartbeats(mock: &Mock) -> Vec<Value> {
    mock.to(contract::HEARTBEATS)
        .iter()
        .map(|beat| beat.json())
        .collect()
}

#[test]
fn the_setup_stays_on_the_box_until_watch_says_it_takes_it() {
    let rig = Rig::new();
    write(&rig, Some(&defined()), Some(&run("failed", 5, Some(3))));
    // Watch as it is: a heartbeat with `setup` in it would be a 400.
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    daemon.tick(start + Duration::from_millis(500));
    daemon.write_state(true);
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    for beat in heartbeats(&mock) {
        assert!(beat["environments"][0].get("setup").is_none(), "{beat}");
    }
    let state = daemon.state(true);
    assert_eq!(state.heartbeat.last_error, None);
    assert_eq!(state.heartbeat.withheld, ["setup"]);

    // On the box it is shown all the same, and so is its being held back.
    let shown = gate::status_shown(&rig);
    assert_eq!(shown["watch"]["withheld"], json!(["setup"]));
    let setup = &shown["environments"][0]["setup"];
    assert_eq!(setup["network"], "full");
    assert_eq!(setup["repos"], json!(["your-org/web", "your-org/api"]));
    assert_eq!(setup["script"]["state"], "failed");
    assert_eq!(setup["script"]["exit_code"], 3);
    let text = common::run(rig.nucleated().arg("status"), "");
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("not sent, Watch does not take it yet: setup"),
        "{text}"
    );
    assert!(
        text.contains(
            "setup network full, 2 repos, 2 variables, 0 secrets, script failed with exit 3 at "
        ),
        "{text}"
    );
}

#[test]
fn a_watch_that_takes_the_setup_is_sent_it_read_afresh_each_heartbeat() {
    let rig = Rig::new();
    write(&rig, Some(&defined()), Some(&run("running", 1, None)));
    let mock = gate::watch_taking(&["setup"]);
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    // Its answer named `setup`, so the next turn of the loop says it.
    daemon.tick(start + Duration::from_millis(500));
    let beats = heartbeats(&mock);
    assert_eq!(beats.len(), 2);
    assert!(beats[0]["environments"][0].get("setup").is_none());
    let setup = &beats[1]["environments"][0]["setup"];
    assert_eq!(
        setup,
        &json!({
            "network": "full",
            "allowed_hosts": 0,
            "repos": ["your-org/web", "your-org/api"],
            "variables": 2,
            "secrets": 0,
            "script": {
                "state": "running",
                "started_at": setup["script"]["started_at"],
            },
        })
    );

    // The run ends, failed, and the recipe narrows the network: the next
    // heartbeat says so without a restart.
    let mut narrowed = defined();
    narrowed["network"] = json!("limited");
    narrowed["allowed_hosts"] = json!(3);
    write(&rig, Some(&narrowed), Some(&run("failed", 1, Some(124))));
    daemon.tick(start + Duration::from_secs(61));
    let beats = heartbeats(&mock);
    assert_eq!(beats.len(), 3);
    let setup = &beats[2]["environments"][0]["setup"];
    assert_eq!(setup["network"], "limited");
    assert_eq!(setup["allowed_hosts"], 3);
    assert_eq!(setup["script"]["state"], "failed");
    assert_eq!(setup["script"]["exit_code"], 124);
    assert!(setup["script"]["finished_at"].is_string());

    // The recipe takes the environment's setup away: the heartbeat stops
    // carrying it, and stays one Watch takes.
    write(&rig, None, None);
    daemon.tick(start + Duration::from_secs(122));
    let beats = heartbeats(&mock);
    assert_eq!(beats.len(), 4);
    assert!(beats[3]["environments"][0].get("setup").is_none());
    assert_eq!(beats[3]["environments"][0]["server_alive"], true);
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
}

#[test]
fn a_file_the_box_cannot_vouch_for_never_costs_the_heartbeat() {
    let rig = Rig::new();
    let mock = gate::watch_taking(&["setup"]);
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    let dir = rig.home.setup_dir();
    // One beat a minute, each with whatever is in the folder by then.
    let mut minute = 0;
    let mut said = || -> Value {
        let at = start + Duration::from_secs(61 * minute);
        minute += 1;
        daemon.tick(at);
        daemon.tick(at + Duration::from_millis(500));
        let beat = heartbeats(&mock).pop().unwrap();
        assert_eq!(beat["environments"][0]["server_alive"], true, "{beat}");
        beat["environments"][0].clone()
    };
    // No file, a file that is not JSON, and one out of range: no setup.
    assert!(said().get("setup").is_none());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("client-a.json"), "network = \"full\"").unwrap();
    assert!(said().get("setup").is_none());
    let mut over = defined();
    over["variables"] = json!(1_001);
    write(&rig, Some(&over), None);
    assert!(said().get("setup").is_none());
    // A run file that is not one the runner writes costs only the script.
    write(&rig, Some(&defined()), Some(&json!({ "state": "done" })));
    let environment = said();
    assert_eq!(environment["setup"]["variables"], 2);
    assert!(environment["setup"].get("script").is_none());
    // A run dated ahead of the clock is sent as now, and so is never ahead
    // of the heartbeat that carries it.
    let ahead = unix_ms(SystemTime::now()) + 3_600_000;
    write(
        &rig,
        Some(&defined()),
        Some(&json!({ "state": "running", "started_at_ms": ahead })),
    );
    let environment = said();
    let started = environment["setup"]["script"]["started_at"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(started <= rfc3339(SystemTime::now()), "{started}");
    // Every one of those heartbeats was one Watch takes.
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
}

#[test]
fn what_keeps_the_setup_out_is_logged_once_and_quotes_nothing() {
    let rig = Rig::new();
    let mut defined = defined();
    defined["repos"] = json!(["your-org/web", format!("{MARK} a repo the user named")]);
    write(
        &rig,
        Some(&defined),
        Some(&json!({ "state": format!("{MARK} it went badly"), "started_at_ms": 1 })),
    );
    let mock = gate::watch_taking(&["setup"]);
    rig.sign_in(&mock.url);
    // The first heartbeat, and the one at once after it with `setup`: two
    // heartbeats, each reading the files.
    let out = gate::daemon_until(&rig, None, "a heartbeat with the setup", &|| {
        mock.to(contract::HEARTBEATS)
            .iter()
            .any(|beat| beat.body.contains("\"setup\""))
    });
    let logged = stderr(&out);
    assert_eq!(
        logged
            .matches(
                "environment client-a: its setup script's run file is not one the runner writes"
            )
            .count(),
        1,
        "{logged}"
    );
    assert_eq!(
        logged
            .matches("environment client-a: a repository in its setup file")
            .count(),
        1,
        "{logged}"
    );
    assert!(!logged.contains(MARK), "content in the log:\n{logged}");
    let beat = heartbeats(&mock).pop().unwrap();
    assert_eq!(
        beat["environments"][0]["setup"]["repos"],
        json!(["your-org/web"])
    );
    // Status names why, in fixed words.
    let shown = gate::status_shown(&rig);
    assert_eq!(
        shown["environments"][0]["setup_problems"],
        json!(["repo_left_out", "run_malformed"])
    );
    gate::assert_clean(&rig, &mock);
}

/// The gate. The recipe's files are full of the sentinel wherever a value
/// that is not one of the listed fields could ride along: in repository
/// names that are sentences, in keys the recipe does not write, in the
/// script's hash, and in what a runner might be tempted to keep, its output
/// and its script. Then the daemon ticks against a Watch that takes every
/// field. Nothing of the sentinel may leave, be written to the state
/// folder, or be shown by `nucleated status`, and the hash leaves neither.
#[test]
fn nothing_from_the_setup_files_leaves_but_the_listed_fields() {
    let rig = Rig::new();
    let mut defined = defined();
    // A repository's name is a listed field: one that fits the pattern is a
    // name and is sent. Only what cannot be a name may carry the sentinel.
    defined["repos"] = json!([
        "your-org/web",
        format!("{MARK} the repo"),
        format!("{MARK}/web/extra"),
    ]);
    defined["variables_named"] = json!({ format!("{MARK}_KEY"): gate::mark("value") });
    defined["setup_script_text"] = json!(gate::mark("setup_script"));
    let run = json!({
        "state": "failed",
        "started_at_ms": unix_ms(SystemTime::now()) - 60_000,
        "finished_at_ms": unix_ms(SystemTime::now()) - 18_000,
        "exit_code": 2,
        "script_sha256": format!("{MARK}{HASH}"),
        "output": gate::mark("output"),
        "script": gate::mark("script"),
        "path": format!("/root/{MARK}/setup.sh"),
    });
    let fixture = format!("{defined}{run}");
    gate::assert_fixture_would_catch_a_leak(
        &fixture,
        7,
        &["repos", "script_sha256", "output", "script", "path"],
    );
    write(&rig, Some(&defined), Some(&run));
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    gate::tick(&rig);

    let beat = gate::said(&mock);
    let setup = &beat["environments"][0]["setup"];
    assert_eq!(setup["repos"], json!(["your-org/web"]));
    assert_eq!(setup["script"]["state"], "failed");
    assert_eq!(setup["script"]["exit_code"], 2);
    assert!(!mock.everything().contains(HASH), "the script's hash left");
    gate::assert_clean(&rig, &mock);
    assert!(!gate::status_shown(&rig).to_string().contains(HASH));
}
