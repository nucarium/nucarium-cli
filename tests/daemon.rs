//! The daemon's loop against a local stand-in for the Watch API: the
//! heartbeat, batching, "needs input" at once, retry with backoff, and replay
//! after an outage.

mod common;

use common::{contract, dead_api, mode, Mock, Reply, Request, Rig, TOKEN};
use nucleate::daemon::{Daemon, Timings};
use nucleate::envelope::{unix_ms, Draft, Envelope, EventType};
use nucleate::hook;
use nucleate::spool::{Record, Spool};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// What the stand-in API does next. A status of 0 means "answer normally".
#[derive(Default)]
struct Knobs {
    heartbeat_status: AtomicU16,
    events_status: AtomicU16,
    retry_after: AtomicU64,
    heartbeat_interval: AtomicU64,
    delay_ms: AtomicU64,
}

fn api() -> (Mock, Arc<Knobs>) {
    let knobs = Arc::new(Knobs::default());
    knobs.heartbeat_interval.store(60, Ordering::SeqCst);
    let seen = knobs.clone();
    let mock = Mock::start(move |request: &Request| {
        // As Watch does, refuse anything the contract does not allow.
        if let Some(refused) = contract::refusal(request) {
            return refused;
        }
        let delay = Duration::from_millis(seen.delay_ms.load(Ordering::SeqCst));
        let problem = |status: u16| match status {
            401 => Reply::problem(401, "unauthorized"),
            429 => Reply::problem(429, "rate_limited").header(
                "Retry-After",
                &seen.retry_after.load(Ordering::SeqCst).to_string(),
            ),
            other => Reply::problem(other, "internal_error"),
        };
        let reply = match request.path.as_str() {
            "/v1/heartbeats:ingest" => match seen.heartbeat_status.load(Ordering::SeqCst) {
                0 => Reply::json(
                    200,
                    json!({
                        "received_at": "2026-11-19T12:00:00.000Z",
                        "heartbeat_interval_s": seen.heartbeat_interval.load(Ordering::SeqCst),
                    }),
                ),
                status => problem(status),
            },
            "/v1/events:ingest" => match seen.events_status.load(Ordering::SeqCst) {
                0 => {
                    let events = request.json()["events"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    // One session Watch will not take in any batch, and one
                    // whose environment it does not know.
                    if events.iter().any(|event| event["session_id"] == "poison") {
                        Reply::problem(400, "validation_failed")
                    } else {
                        let rejected: Vec<Value> = events
                            .iter()
                            .filter(|event| event["session_id"] == "stranger")
                            .map(|event| json!({ "event_id": event["event_id"], "reason": "unknown_environment" }))
                            .collect();
                        Reply::json(
                            200,
                            json!({
                                "received_at": "2026-11-19T12:00:00.000Z",
                                "accepted": events.len() - rejected.len(),
                                "duplicates": 0,
                                "rejected": rejected,
                            }),
                        )
                    }
                }
                status => problem(status),
            },
            _ => Reply::problem(404, "not_found"),
        };
        reply.after(delay)
    });
    (mock, knobs)
}

fn timings() -> Timings {
    Timings {
        jitter: false,
        http_timeout: Duration::from_secs(2),
        ..Timings::default()
    }
}

fn daemon(rig: &Rig) -> Daemon {
    Daemon::new(rig.settings(), timings())
}

/// Record a hook event as the entry point would, each a millisecond after the last.
fn emit(rig: &Rig, event: &str, session: &str, extra: Value) {
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now() + Duration::from_millis(CLOCK.fetch_add(1, Ordering::SeqCst));
    emit_at(rig, event, session, extra, now);
}

/// Record a hook event as if the hook had run at `now`.
fn emit_at(rig: &Rig, event: &str, session: &str, extra: Value, now: SystemTime) {
    let mut payload = json!({ "session_id": session, "cwd": rig.root });
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        payload[key] = value;
    }
    let written = hook::record(
        &rig.settings(),
        event,
        Some("client-a"),
        &mut payload.to_string().as_bytes(),
        now,
    );
    assert!(written.is_some(), "the hook recorded nothing for {event}");
}

fn needs_input(rig: &Rig, session: &str) {
    emit(
        rig,
        "Notification",
        session,
        json!({ "notification_type": "permission_prompt" }),
    );
}

/// The batches of events the API received, in order. None of them, and no
/// heartbeat, may be one Watch would have refused.
fn batches(mock: &Mock) -> Vec<Vec<Value>> {
    assert_eq!(contract::violations(mock), Vec::<String>::new());
    mock.to("/v1/events:ingest")
        .iter()
        .map(|request| request.json()["events"].as_array().cloned().unwrap())
        .collect()
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ---- heartbeat --------------------------------------------------------

#[test]
fn the_first_tick_sends_a_heartbeat_with_health_and_live_agent_servers() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    daemon(&rig).tick(Instant::now());

    let beats = mock.to("/v1/heartbeats:ingest");
    assert_eq!(beats.len(), 1);
    let beat = &beats[0];
    assert_eq!(beat.method, "POST");
    assert_eq!(
        beat.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(beat
        .header("content-type")
        .unwrap()
        .starts_with("application/json"));
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    let body = beat.json();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "activity_beat_s",
            "client_ts",
            "daemon_version",
            "environments",
            "machine"
        ]
    );
    // Claude Code gives no version and no login expiry to read, so the
    // environment says nothing about either.
    assert_eq!(
        body["environments"],
        json!([{ "env_id": "client-a", "server_alive": true }])
    );
    assert_eq!(body["daemon_version"], env!("CARGO_PKG_VERSION"));
    // The promise Watch needs before it calls a session stuck for no tool activity.
    assert_eq!(body["activity_beat_s"], 300);
    let machine = body["machine"].as_object().unwrap();
    for key in machine.keys() {
        assert!(
            [
                "disk_used_pct",
                "load_1m",
                "uptime_s",
                "memory_pressure_pct",
                // Said only by a system that has a way to say it.
                "reboot_required",
            ]
            .contains(&key.as_str()),
            "unexpected health field {key}"
        );
    }
    assert!((0.0..=100.0).contains(&machine["disk_used_pct"].as_f64().unwrap()));
    assert!(machine["uptime_s"].as_u64().unwrap() > 0);
    // 2026-11-19T12:00:00.000Z
    let stamp = body["client_ts"].as_str().unwrap();
    assert_eq!(stamp.len(), 24);
    assert!(stamp.ends_with('Z'));
}

/// A box with more to say than the contract has fields for: a second
/// environment watched through a driver whose links need its id, and a
/// config sync waiting for its first dry run.
fn box_with_more_to_say() -> Rig {
    let rig = Rig::new();
    rig.configure(&format!(
        "[[environment]]\nname = \"t3\"\nroot = \"{root}\"\ndriver = \"t3code\"\n\
         check = \"true\"\ndriver_home = \"{home}\"\ndriver_poll = \"off\"\n\n\
         [config_sync]\nrepository = \"{repository}\"\nbranch = \"main\"\ninterval = \"off\"\n",
        root = rig.root.display(),
        home = rig.home.dir.join("no-t3-here").display(),
        repository = rig.home.dir.join("no-repository-here").display(),
    ));
    let state = rig.home.state_dir();
    std::fs::create_dir_all(state.join("drivers")).unwrap();
    std::fs::write(
        state.join("drivers/t3.json"),
        json!({ "driver": "t3code", "condition": "ok", "link_ref": "env_01HZX" }).to_string(),
    )
    .unwrap();
    std::fs::write(
        state.join("config-sync.json"),
        json!({ "environments": { "client-a": {
            "state": "pending",
            "checked_at_ms": unix_ms(SystemTime::now()),
        } } })
        .to_string(),
    )
    .unwrap();
    rig
}

/// The fields ahead of the contract that a heartbeat carried, over all of
/// its environments.
fn ahead_in(beat: &Request) -> Vec<String> {
    let body = beat.json();
    let mut found: Vec<String> = body["environments"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|environment| environment.as_object().unwrap().keys().cloned())
        .filter(|key| contract::AHEAD.contains(&key.as_str()))
        .collect();
    found.sort();
    found
}

#[test]
fn watch_as_it_is_refuses_nothing_from_a_box_with_drivers_and_config_sync() {
    let rig = box_with_more_to_say();
    // Watch as it is: any field its contract does not name is a 400.
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    needs_input(&rig, "s1");
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    daemon.tick(start + secs(61));
    daemon.write_state(true);

    // Every heartbeat was one Watch takes, so the box is in sight...
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    let beats = mock.to(contract::HEARTBEATS);
    assert_eq!(beats.len(), 2);
    for beat in &beats {
        assert_eq!(ahead_in(beat), Vec::<String>::new());
        let named: Vec<Value> = beat.json()["environments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|environment| environment["env_id"].clone())
            .collect();
        assert_eq!(named, [json!("client-a"), json!("t3")]);
    }
    let state = daemon.state(true);
    assert_eq!(state.heartbeat.last_error, None);
    assert!(state.heartbeat.last_ok_at.is_some());
    // ...and what waits on its owner has left it.
    assert_eq!(types(&batches(&mock)[0]), ["permission_prompt"]);
    // What stayed behind is said on the box, by name.
    assert_eq!(
        state.heartbeat.withheld,
        ["config_state", "driver", "driver_ref"]
    );
    let shown = common::gate::status_shown(&rig);
    assert_eq!(
        shown["watch"]["withheld"],
        json!(["config_state", "driver", "driver_ref"])
    );
    assert_eq!(shown["environments"][1]["driver"]["link_ref"], "env_01HZX");
    assert_eq!(shown["environments"][0]["config"]["state"], "pending");
    // `nucleated check` holds to the same rule, and says what stayed.
    let check = common::stdout_json(&common::run(rig.nucleated().arg("check"), ""));
    let heartbeat = check["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "heartbeat")
        .unwrap();
    assert_eq!(heartbeat["ok"], true, "{heartbeat}");
    assert!(
        heartbeat["detail"].as_str().unwrap().contains("driver"),
        "{heartbeat}"
    );
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
}

#[test]
fn a_watch_that_says_it_takes_the_newer_fields_is_sent_them_without_waiting_a_beat() {
    let rig = box_with_more_to_say();
    let mock = common::gate::watch_ahead();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    // Nothing is sent on a guess: the first heartbeat is one every Watch takes.
    let beats = mock.to(contract::HEARTBEATS);
    assert_eq!(beats.len(), 1);
    assert_eq!(ahead_in(&beats[0]), Vec::<String>::new());
    // Its answer named the fields, so the rest follows at the next turn of
    // the loop, and then at the usual pace.
    daemon.tick(start + Duration::from_millis(500));
    daemon.tick(start + secs(30));
    let beats = mock.to(contract::HEARTBEATS);
    assert_eq!(beats.len(), 2);
    assert_eq!(
        ahead_in(&beats[1]),
        ["config_state", "driver", "driver_ref"]
    );
    let said = beats[1].json();
    assert_eq!(said["environments"][0]["config_state"], "pending");
    assert_eq!(said["environments"][1]["driver"], "t3code");
    assert_eq!(said["environments"][1]["driver_ref"], "env_01HZX");
    assert_eq!(daemon.state(true).heartbeat.withheld, Vec::<String>::new());
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    daemon.tick(start + secs(61));
    let beats = mock.to(contract::HEARTBEATS);
    assert_eq!(beats.len(), 3);
    assert_eq!(ahead_in(&beats[2]), ahead_in(&beats[1]));

    // A Watch that takes some of them is sent those and no other.
    let rig = box_with_more_to_say();
    let mock = common::gate::watch_taking(&["driver"]);
    rig.sign_in(&mock.url);
    common::gate::tick(&rig);
    assert_eq!(
        ahead_in(mock.to(contract::HEARTBEATS).last().unwrap()),
        ["driver"]
    );
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
}

#[test]
fn plan_usage_is_the_last_good_reading_and_goes_only_to_a_watch_that_takes_it() {
    let rig = Rig::new();
    let config_dir = rig.home.dir.join(".claude-client-a");
    std::fs::create_dir_all(&config_dir).unwrap();
    rig.configure_with(
        &format!(
            "check = \"true\"\nconfig_dir = \"{}\"",
            config_dir.display()
        ),
        "",
    );
    // What the usage thread kept an hour ago; later reads have failed.
    let read_at = unix_ms(SystemTime::now()) - 3_600_000;
    let state = rig.home.state_dir();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("usage.json"),
        json!({ "environments": { "client-a": {
            "login": nucleate::usage::login(&config_dir),
            "read_at_ms": read_at, "limits": [
            { "window": "five_hour", "used_pct": 23.0, "resets_at_ms": read_at + 7_200_000 },
            { "window": "seven_day", "scope": "Fable", "used_pct": 71.0 },
        ] } } })
        .to_string(),
    )
    .unwrap();

    // Watch as it is is never sent it, and the box says what it held back.
    let mock = common::gate::watch();
    rig.sign_in(&mock.url);
    let mut plain = daemon(&rig);
    let start = Instant::now();
    plain.tick(start);
    plain.tick(start + Duration::from_millis(500));
    let beats = mock.to(contract::HEARTBEATS);
    assert!(beats.iter().all(|beat| ahead_in(beat).is_empty()));
    assert_eq!(plain.state(true).heartbeat.withheld, ["usage"]);
    assert_eq!(contract::violations(&mock), Vec::<String>::new());

    // A Watch that takes it is sent the reading, with the time it was read.
    let mock = common::gate::watch_taking(&["usage"]);
    rig.sign_in(&mock.url);
    let mut taking = daemon(&rig);
    taking.tick(start);
    taking.tick(start + Duration::from_millis(500));
    let said = common::gate::said(&mock);
    assert_eq!(
        said["environments"][0]["usage"],
        json!({
            "read_at": nucleate::envelope::rfc3339(
                std::time::UNIX_EPOCH + Duration::from_millis(read_at)
            ),
            "limits": [
                {
                    "window": "five_hour",
                    "used_pct": 23.0,
                    "resets_at": nucleate::envelope::rfc3339(
                        std::time::UNIX_EPOCH + Duration::from_millis(read_at + 7_200_000)
                    ),
                },
                { "window": "seven_day", "scope": "Fable", "used_pct": 71.0 },
            ],
        })
    );
    assert_eq!(contract::violations(&mock), Vec::<String>::new());
    common::gate::only_listed_fields(&mock);
}

#[test]
fn a_watch_that_refuses_a_field_it_said_it_takes_still_hears_from_the_box() {
    let rig = box_with_more_to_say();
    // It names every newer field, and then will not have `driver_ref`.
    let mock = Mock::start(|request: &Request| match request.path.as_str() {
        contract::HEARTBEATS if request.body.contains("driver_ref") => {
            Reply::problem(400, "validation_failed")
        }
        contract::HEARTBEATS => Reply::json(
            200,
            json!({
                "received_at": "2026-11-19T12:00:00.000Z",
                "heartbeat_interval_s": 60,
                "accepts": contract::AHEAD,
            }),
        ),
        _ => Reply::json(
            200,
            json!({ "received_at": "2026-11-19T12:00:00.000Z", "accepted": 1, "duplicates": 0, "rejected": [] }),
        ),
    });
    rig.sign_in(&mock.url);
    needs_input(&rig, "s1");
    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            ahead_hold: secs(600),
            ..timings()
        },
    );
    let start = Instant::now();
    daemon.tick(start);
    daemon.tick(start + Duration::from_millis(500));
    // Plain, then in full and refused, then plain again in the same breath.
    let refused = |beats: &[Request]| -> Vec<bool> {
        beats
            .iter()
            .map(|beat| beat.body.contains("driver_ref"))
            .collect()
    };
    let beats = mock.to(contract::HEARTBEATS);
    assert_eq!(refused(&beats), [false, true, false]);
    assert_eq!(ahead_in(&beats[2]), Vec::<String>::new());
    let state = daemon.state(true);
    assert_eq!(state.heartbeat.last_error, None);
    assert_eq!(
        state.heartbeat.withheld,
        ["config_state", "driver", "driver_ref"]
    );
    // The box never went dark, and what waits on its owner left it.
    assert_eq!(mock.to(contract::EVENTS).len(), 1);
    // The refused field is not tried at every beat...
    daemon.tick(start + secs(61));
    daemon.tick(start + secs(122));
    assert_eq!(
        refused(&mock.to(contract::HEARTBEATS)),
        [false, true, false, false, false]
    );
    // ...but it is tried again later, in case Watch has changed its mind,
    // and a second refusal costs the box nothing either.
    daemon.tick(start + secs(700));
    assert_eq!(
        refused(&mock.to(contract::HEARTBEATS)),
        [false, true, false, false, false, true, false]
    );
    assert_eq!(daemon.state(true).heartbeat.last_error, None);
}

#[test]
fn an_agent_server_that_is_not_running_is_reported_as_such() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    rig.configure_with("check = \"false\"", "");
    daemon(&rig).tick(Instant::now());
    assert_eq!(
        mock.to("/v1/heartbeats:ingest")[0].json()["environments"],
        json!([{ "env_id": "client-a", "server_alive": false }])
    );
}

#[test]
fn heartbeats_follow_the_longer_of_the_configured_and_the_servers_interval() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    knobs.heartbeat_interval.store(300, Ordering::SeqCst);
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    let beats = || mock.to("/v1/heartbeats:ingest").len();
    daemon.tick(start);
    assert_eq!(beats(), 1);
    // The config says 60 seconds; the account's floor is five minutes.
    daemon.tick(start + secs(61));
    daemon.tick(start + secs(299));
    assert_eq!(beats(), 1);
    daemon.tick(start + secs(300));
    assert_eq!(beats(), 2);

    // The other way round: the config asks for less often than Watch needs.
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    rig.configure("[heartbeat]\ninterval = \"10m\"\n");
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    daemon.tick(start + secs(300));
    assert_eq!(mock.to("/v1/heartbeats:ingest").len(), 1);
    daemon.tick(start + secs(600));
    assert_eq!(mock.to("/v1/heartbeats:ingest").len(), 2);
}

#[test]
fn a_failed_heartbeat_is_tried_again_before_the_next_one_is_due() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    knobs.heartbeat_status.store(503, Ordering::SeqCst);
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    let beats = || mock.to("/v1/heartbeats:ingest").len();
    daemon.tick(start);
    daemon.tick(start + secs(4));
    assert_eq!(beats(), 1);
    daemon.tick(start + secs(5));
    assert_eq!(beats(), 2);
    daemon.tick(start + secs(14));
    assert_eq!(beats(), 2);
    daemon.tick(start + secs(15));
    assert_eq!(beats(), 3);
    assert_eq!(
        daemon.state(true).heartbeat.last_error.as_deref(),
        Some("server_error")
    );
    knobs.heartbeat_status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(35));
    let state = daemon.state(true);
    assert_eq!(state.heartbeat.last_error, None);
    assert!(state.heartbeat.last_ok_at.is_some());
}

#[test]
fn a_changed_config_is_picked_up_without_a_restart() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    // Make sure the file's timestamp moves on filesystems that keep whole seconds.
    std::thread::sleep(Duration::from_millis(1100));
    rig.configure(&format!(
        "[watch]\ntoken_file = \"{}\"\n[[environment]]\nname = \"ops\"\nroot = \"{}\"\ncheck = \"false\"\n",
        rig.home.token_file().display(),
        rig.root.display()
    ));
    daemon.tick(start + secs(2));
    let beats = mock.to("/v1/heartbeats:ingest");
    assert_eq!(beats.len(), 2);
    assert_eq!(
        beats[1].json()["environments"],
        json!([
            { "env_id": "client-a", "server_alive": true },
            { "env_id": "ops", "server_alive": false },
        ])
    );
}

// ---- events -----------------------------------------------------------

#[test]
fn needs_input_is_sent_at_once_and_routine_events_wait_for_a_batch() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);

    emit(&rig, "SessionStart", "s1", json!({ "source": "startup" }));
    emit(&rig, "UserPromptSubmit", "s1", json!({}));
    daemon.tick(start + secs(1));
    daemon.tick(start + secs(9));
    assert!(
        batches(&mock).is_empty(),
        "routine events were not held for a batch"
    );
    daemon.tick(start + secs(11));
    assert_eq!(
        types(&batches(&mock)[0]),
        ["session_start", "user_prompt_submit"]
    );

    // "Needs input" does not wait, and takes what is waiting with it.
    emit(&rig, "Stop", "s1", json!({}));
    needs_input(&rig, "s1");
    daemon.tick(start + secs(12));
    assert_eq!(types(&batches(&mock)[1]), ["stop", "permission_prompt"]);
    assert!(rig.spooled().is_empty());
}

#[test]
fn an_event_carries_only_the_envelope() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let mut daemon = daemon(&rig);
    daemon.tick(Instant::now());
    emit(
        &rig,
        "Notification",
        "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a",
        json!({ "notification_type": "permission_prompt", "cwd": worktree }),
    );
    daemon.tick(Instant::now());

    let request = &mock.to("/v1/events:ingest")[0];
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    let body = request.json();
    assert_eq!(
        body.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["events"]
    );
    let event = body["events"][0].as_object().unwrap();
    let mut keys: Vec<&str> = event.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "branch",
            "client_ts",
            "env_id",
            "event_id",
            "repos",
            "session_id",
            "type"
        ]
    );
    assert_eq!(event["env_id"], "client-a");
    assert_eq!(event["session_id"], "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a");
    assert_eq!(event["type"], "permission_prompt");
    assert_eq!(event["repos"], json!(["web"]));
    assert_eq!(event["branch"], "workspace/fix-login");
    // A UUIDv7, made on the box.
    let id = event["event_id"].as_str().unwrap();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "7");
}

#[test]
fn fifty_waiting_events_are_sent_without_waiting_for_the_window() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    for i in 0..49 {
        emit(&rig, "Stop", &format!("s{i}"), json!({}));
    }
    daemon.tick(start + secs(1));
    assert!(batches(&mock).is_empty());
    emit(&rig, "Stop", "s49", json!({}));
    daemon.tick(start + secs(2));
    assert_eq!(batches(&mock)[0].len(), 50);
}

#[test]
fn a_request_holds_at_most_a_hundred_events_in_the_order_they_happened() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    for i in 0..250 {
        emit(&rig, "Stop", &format!("s{i:03}"), json!({}));
    }
    daemon.tick(Instant::now());
    let sent = batches(&mock);
    assert_eq!(
        sent.iter().map(Vec::len).collect::<Vec<_>>(),
        [100, 100, 50]
    );
    let sessions: Vec<String> = sent
        .concat()
        .iter()
        .map(|e| e["session_id"].as_str().unwrap().to_string())
        .collect();
    let mut sorted = sessions.clone();
    sorted.sort();
    assert_eq!(sessions, sorted);
    assert!(rig.spooled().is_empty());
}

#[test]
fn a_tool_call_is_sent_only_when_it_changes_what_the_session_is_doing() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    emit(&rig, "UserPromptSubmit", "s1", json!({}));
    for _ in 0..5 {
        emit(&rig, "PostToolUse", "s1", json!({}));
    }
    needs_input(&rig, "s1");
    // The permission is granted, the tool runs, and more tools follow.
    for _ in 0..5 {
        emit(&rig, "PostToolUse", "s1", json!({}));
    }
    daemon.tick(start + secs(1));
    daemon.tick(start + secs(20));
    assert_eq!(
        types(&batches(&mock).concat()),
        ["user_prompt_submit", "permission_prompt", "activity"]
    );
    assert!(rig.spooled().is_empty());
}

#[test]
fn a_working_sessions_tool_calls_reach_watch_once_in_five_minutes() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    // The hooks' clock, moved by hand: a prompt, then a tool call a minute
    // for twelve minutes.
    let began = SystemTime::now();
    emit_at(&rig, "UserPromptSubmit", "s1", json!({}), began);
    for minute in 1..=12 {
        emit_at(
            &rig,
            "PostToolUse",
            "s1",
            json!({}),
            began + secs(60 * minute),
        );
    }
    daemon.tick(start + secs(1));
    daemon.tick(start + secs(20));
    let sent = batches(&mock).concat();
    assert_eq!(types(&sent), ["user_prompt_submit", "activity", "activity"]);
    // The fifth minute's and the tenth's: never more than five minutes apart.
    let at = |event: &Value| event["client_ts"].as_str().unwrap().to_string();
    assert_eq!(
        [at(&sent[1]), at(&sent[2])],
        [
            nucleate::envelope::rfc3339(began + secs(300)),
            nucleate::envelope::rfc3339(began + secs(600))
        ]
    );
    assert!(rig.spooled().is_empty());
}

#[test]
fn the_five_minutes_survive_a_restart_of_the_daemon() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let began = SystemTime::now();
    let start = Instant::now();
    let mut first = daemon(&rig);
    first.tick(start);
    emit_at(&rig, "UserPromptSubmit", "s1", json!({}), began);
    first.tick(start + secs(1));
    first.tick(start + secs(20));
    drop(first);

    let mut second = daemon(&rig);
    second.tick(start + secs(21));
    emit_at(&rig, "PostToolUse", "s1", json!({}), began + secs(120));
    emit_at(&rig, "PostToolUse", "s1", json!({}), began + secs(301));
    second.tick(start + secs(22));
    second.tick(start + secs(40));
    assert_eq!(
        types(&batches(&mock).concat()),
        ["user_prompt_submit", "activity"]
    );
}

#[test]
fn a_limit_reset_time_goes_out_with_the_failed_stop_when_an_agent_gives_one() {
    // No agent this box knows gives one yet, so the event is written as the
    // hook would write it for an adapter that did.
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    let now = SystemTime::now();
    let draft = |session: &str| Draft {
        env_id: "client-a".into(),
        session_id: session.into(),
        error_kind: Some("rate_limit".into()),
        limit_resets_at: Some(now + secs(4 * 3_600)),
        ..Draft::default()
    };
    let spool = Spool::new(&rig.spool_dir());
    for (kind, session) in [(EventType::StopFailure, "s1"), (EventType::Stop, "s2")] {
        spool
            .write(&Record {
                event: Envelope::new(kind, draft(session), now).unwrap(),
                at_ms: unix_ms(now),
                workspace: None,
                process: None,
            })
            .unwrap();
    }
    daemon.tick(start + secs(1));
    daemon.tick(start + secs(20));

    let sent = batches(&mock).concat();
    assert_eq!(sent.len(), 2);
    let failed = sent.iter().find(|e| e["type"] == "stop_failure").unwrap();
    assert_eq!(failed["error_kind"], "rate_limit");
    assert_eq!(
        failed["limit_resets_at"],
        nucleate::envelope::rfc3339(now + secs(4 * 3_600))
    );
    // Only a failed stop carries one.
    let stopped = sent.iter().find(|e| e["type"] == "stop").unwrap();
    assert!(stopped.get("limit_resets_at").is_none());
    assert!(stopped.get("error_kind").is_none());

    // The box keeps the session as failed, with the kind and the time.
    let sessions = nucleate::sessions::Store::new(&rig.home.state_dir()).list("client-a");
    let kept = sessions.iter().find(|s| s.session_id == "s1").unwrap();
    assert_eq!(kept.status, nucleate::sessions::Status::Failed);
    assert_eq!(kept.error_kind.as_deref(), Some("rate_limit"));
    assert_eq!(
        kept.limit_resets_at.as_deref(),
        failed["limit_resets_at"].as_str()
    );
    assert_eq!(daemon.state(true).events.rejected, 0);
}

#[test]
fn the_stand_in_api_holds_the_contract_as_watch_does() {
    let heartbeat = || {
        json!({
            "machine": {
                "disk_used_pct": 41.5, "load_1m": 0.62, "uptime_s": 864000,
                "memory_pressure_pct": 3.1, "on_battery": false, "reboot_required": false,
                "uncommitted_repos": 2, "unpushed_commits": 5,
                "last_snapshot_at": "2026-11-19T11:10:04.000Z",
            },
            "environments": [
                {
                    "env_id": "client-a", "server_alive": true, "server_restarts": 4,
                    "agent_version": "2.1.0", "login_expires_at": "2026-12-01T09:00:00.000Z",
                    "last_snapshot_at": "2026-11-19T11:10:04.000Z",
                    "unsaved": {
                        "uncommitted_repos": 2, "unpushed_commits": 5,
                        "counted_at": "2026-11-19T11:52:00.000Z",
                    },
                },
                { "env_id": "personal", "server_alive": true, "server_restarts": 0 },
            ],
            "daemon_version": "0.1.0",
            "activity_beat_s": 300,
            "paused_until": "2026-11-19T13:00:00.000Z",
            "client_ts": "2026-11-19T12:00:00.000Z",
        })
    };
    assert_eq!(contract::heartbeat(&heartbeat()), Ok(()));
    // An older daemon's heartbeat is still one the contract takes.
    assert_eq!(
        contract::heartbeat(&json!({
            "machine": { "disk_used_pct": 41.5, "load_1m": 0.62, "uptime_s": 864000 },
            "environments": [{ "env_id": "client-a", "server_alive": true }],
            "daemon_version": "0.1.0",
            "client_ts": "2026-11-19T12:00:00.000Z",
        })),
        Ok(())
    );
    // Each field ahead of the contract is one Watch refuses as it is, and
    // one a Watch that names it takes, in its shape and beside what it
    // belongs to.
    let ahead = |field: &str, value: Value, taking: &[&str]| {
        let mut body = heartbeat();
        body["environments"][0][field] = value;
        contract::heartbeat_taking(&body, taking)
    };
    for (field, value) in [
        ("driver", json!("t3code")),
        ("config_commit", json!("a1".repeat(20))),
        ("config_state", json!("pending")),
    ] {
        assert!(ahead(field, value.clone(), &[]).is_err(), "{field}");
        assert_eq!(ahead(field, value.clone(), &[field]), Ok(()), "{field}");
        assert!(ahead(field, value, &["another"]).is_err(), "{field}");
        assert!(
            ahead(field, json!("words, not a value"), &[field]).is_err(),
            "{field}"
        );
    }
    for (field, beside) in [
        ("driver_ref", "driver"),
        ("config_modified", "config_commit"),
    ] {
        let value = if field == "driver_ref" {
            json!("env_01HZX")
        } else {
            json!(true)
        };
        assert!(ahead(field, value.clone(), &[]).is_err(), "{field}");
        assert!(
            ahead(field, value, &[field, beside]).is_err(),
            "{field} with no {beside}"
        );
    }
    let spoiled = |change: &dyn Fn(&mut Value)| {
        let mut body = heartbeat();
        change(&mut body);
        contract::heartbeat(&body)
    };
    for (what, refused) in [
        (
            "an unknown field",
            spoiled(&|b| b["hostname"] = json!("agentbox")),
        ),
        (
            "an unknown machine field",
            spoiled(&|b| b["machine"]["kernel"] = json!("6.8")),
        ),
        (
            "an unknown environment field",
            spoiled(&|b| b["environments"][0]["sessions"] = json!(3)),
        ),
        (
            "a beat below a minute",
            spoiled(&|b| b["activity_beat_s"] = json!(30)),
        ),
        (
            "a beat that is not a whole number",
            spoiled(&|b| b["activity_beat_s"] = json!("300")),
        ),
        (
            "a version that is a sentence",
            spoiled(&|b| b["environments"][0]["agent_version"] = json!("2.1.0 (Claude Code)")),
        ),
        (
            "an expiry that is not a time",
            spoiled(&|b| b["environments"][0]["login_expires_at"] = json!("next Tuesday")),
        ),
        (
            "a pause in words",
            spoiled(&|b| b["paused_until"] = json!("until the new disk is in")),
        ),
        (
            "a battery state in words",
            spoiled(&|b| b["machine"]["on_battery"] = json!("OB DISCHRG")),
        ),
        (
            "more unpushed commits than the contract counts",
            spoiled(&|b| b["machine"]["unpushed_commits"] = json!(1_000_001)),
        ),
        (
            "a restart count below zero",
            spoiled(&|b| b["environments"][0]["server_restarts"] = json!(-1)),
        ),
        (
            "an unsaved count without the time it was counted",
            spoiled(&|b| {
                let unsaved = b["environments"][0]["unsaved"].as_object_mut().unwrap();
                unsaved.remove("counted_at");
            }),
        ),
        (
            "a file name beside an unsaved count",
            spoiled(&|b| b["environments"][0]["unsaved"]["files"] = json!(["notes.txt"])),
        ),
        (
            "a snapshot time in words",
            spoiled(&|b| b["environments"][0]["last_snapshot_at"] = json!("an hour ago")),
        ),
        (
            "no clock",
            spoiled(&|b| {
                b.as_object_mut().unwrap().remove("client_ts");
            }),
        ),
        (
            "an environment named twice",
            spoiled(&|b| b["environments"][1]["env_id"] = json!("client-a")),
        ),
    ] {
        assert!(refused.is_err(), "the stand-in took {what}");
    }

    let event = || {
        json!({
            "event_id": "01JD3KB7T4N9W2Y6C8M1Q3R5S7",
            "env_id": "client-a",
            "session_id": "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a",
            "type": "stop_failure",
            "error_kind": "rate_limit",
            "limit_resets_at": "2026-11-20T03:10:00.000Z",
            "repos": ["your-org/web", "your-org/api"],
            "branch": "agent/checkout-fix",
            "client_ts": "2026-11-19T22:41:17.305Z",
        })
    };
    let batch = |event: Value| contract::events(&json!({ "events": [event] }));
    assert_eq!(batch(event()), Ok(()));
    let mut outcome = event();
    outcome["type"] = json!("outcome");
    outcome["commits"] = json!(3);
    outcome["uncommitted_repos"] = json!(0);
    outcome["unpushed_commits"] = json!(0);
    outcome["pull_requests"] = json!(["https://github.com/Your-Org/web/pull/412"]);
    assert_eq!(batch(outcome.clone()), Ok(()));
    let spoiled = |change: &dyn Fn(&mut Value)| {
        let mut body = outcome.clone();
        change(&mut body);
        batch(body)
    };
    for (what, refused) in [
        (
            "an unknown field",
            spoiled(&|e| e["title"] = json!("Fix the checkout")),
        ),
        (
            "a reset time in words",
            spoiled(&|e| e["limit_resets_at"] = json!("in four hours")),
        ),
        (
            "an error message",
            spoiled(&|e| e["error_kind"] = json!("Rate limited: try later")),
        ),
        ("an unknown type", spoiled(&|e| e["type"] = json!("stuck"))),
        (
            "a pull request on a repo the event does not name",
            spoiled(&|e| e["pull_requests"] = json!(["https://github.com/other/web/pull/1"])),
        ),
        (
            "a pull request address with a query",
            spoiled(&|e| {
                e["pull_requests"] = json!(["https://github.com/your-org/web/pull/1?x=1"])
            }),
        ),
        (
            "a count out of range",
            spoiled(&|e| e["uncommitted_repos"] = json!(33)),
        ),
        (
            "no batch at all",
            contract::events(&json!({ "events": [] })),
        ),
        (
            "a field beside the batch",
            contract::events(&json!({ "events": [event()], "machine": "agentbox" })),
        ),
    ] {
        assert!(refused.is_err(), "the stand-in took {what}");
    }

    // And over the wire it answers as the API does.
    let (mock, _) = api();
    let post = |path: &str, body: Value| {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        agent
            .post(format!("{}{path}", mock.url))
            .send_json(body)
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(post(contract::HEARTBEATS, heartbeat()), 200);
    let mut unknown = heartbeat();
    unknown["hostname"] = json!("agentbox");
    assert_eq!(post(contract::HEARTBEATS, unknown), 400);
    assert_eq!(post(contract::EVENTS, json!({ "events": [event()] })), 200);
    let mut unknown = event();
    unknown["title"] = json!("Fix the checkout");
    assert_eq!(post(contract::EVENTS, json!({ "events": [unknown] })), 400);
}

// ---- failure, backoff and replay --------------------------------------

#[test]
fn a_failed_send_is_retried_with_a_doubling_wait_and_then_replayed_in_order() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);

    knobs.events_status.store(503, Ordering::SeqCst);
    emit(&rig, "UserPromptSubmit", "s1", json!({}));
    needs_input(&rig, "s1");
    let attempts = || mock.to("/v1/events:ingest").len();
    // Fails at 0s; retried after 1s, then 2s, then 4s.
    for (at_ms, expected) in [
        (0, 1),
        (900, 1),
        (1000, 2),
        (2900, 2),
        (3000, 3),
        (6900, 3),
        (7000, 4),
    ] {
        daemon.tick(start + Duration::from_millis(at_ms));
        assert_eq!(attempts(), expected, "after {at_ms}ms");
    }
    assert_eq!(
        rig.spooled().len(),
        2,
        "nothing is lost while Watch is failing"
    );
    assert_eq!(
        daemon.state(true).events.last_error.as_deref(),
        Some("server_error")
    );

    // More happens during the outage.
    emit(&rig, "Stop", "s1", json!({}));
    emit(&rig, "SessionEnd", "s1", json!({}));

    knobs.events_status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(15));
    let sent = batches(&mock);
    let delivered = sent.last().unwrap();
    assert_eq!(
        types(delivered),
        [
            "user_prompt_submit",
            "permission_prompt",
            "stop",
            "session_end"
        ]
    );
    // The same events, with the same ids, as the attempts that failed: Watch
    // can tell a retry from a new event.
    assert_eq!(delivered[..2], sent[0][..]);
    assert!(rig.spooled().is_empty());
    let state = daemon.state(true);
    assert_eq!(state.events.last_error, None);
    assert_eq!(state.events.sent, 4);
}

#[test]
fn the_wait_between_retries_stops_growing_at_five_minutes() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    knobs.events_status.store(503, Ordering::SeqCst);
    // Keep the heartbeat from succeeding and clearing the wait.
    knobs.heartbeat_status.store(503, Ordering::SeqCst);
    needs_input(&rig, "s1");
    let attempts = || mock.to("/v1/events:ingest").len();
    let mut at = 0;
    for wait in [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300] {
        daemon.tick(start + secs(at));
        let before = attempts();
        daemon.tick(start + secs(at + wait - 1));
        assert_eq!(attempts(), before, "retried before {wait}s had passed");
        at += wait;
        daemon.tick(start + secs(at));
        assert_eq!(attempts(), before + 1, "not retried after {wait}s");
    }
}

#[test]
fn the_spool_is_replayed_after_the_daemon_restarts() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    rig.sign_in(&mock.url);
    knobs.events_status.store(503, Ordering::SeqCst);
    let mut first = daemon(&rig);
    first.tick(Instant::now());
    emit(&rig, "UserPromptSubmit", "s1", json!({}));
    needs_input(&rig, "s1");
    first.tick(Instant::now());
    drop(first);
    assert_eq!(rig.spooled().len(), 2);

    knobs.events_status.store(0, Ordering::SeqCst);
    let mut second = daemon(&rig);
    second.tick(Instant::now());
    assert_eq!(
        types(batches(&mock).last().unwrap()),
        ["user_prompt_submit", "permission_prompt"]
    );
    assert!(rig.spooled().is_empty());
}

#[test]
fn events_written_while_watch_is_unreachable_are_kept() {
    let rig = Rig::new();
    rig.sign_in(&dead_api());
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    needs_input(&rig, "s1");
    emit(&rig, "Stop", "s1", json!({}));
    daemon.tick(start + secs(1));
    daemon.tick(start + secs(30));
    assert_eq!(rig.spooled().len(), 2);
    let state = daemon.state(true);
    assert_eq!(state.heartbeat.last_error.as_deref(), Some("unreachable"));
    assert_eq!(state.events.pending, 2);
    assert_eq!(state.target, "hosted");
}

#[test]
fn a_slow_watch_is_given_up_on_and_tried_again() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    rig.sign_in(&mock.url);
    let mut daemon = Daemon::new(
        rig.settings(),
        Timings {
            http_timeout: Duration::from_millis(300),
            ..timings()
        },
    );
    let start = Instant::now();
    daemon.tick(start);
    knobs.delay_ms.store(1200, Ordering::SeqCst);
    needs_input(&rig, "s1");
    let began = Instant::now();
    daemon.tick(start + secs(1));
    assert!(
        began.elapsed() < Duration::from_millis(1100),
        "the daemon waited for a slow answer"
    );
    assert_eq!(rig.spooled().len(), 1);
    assert_eq!(
        daemon.state(true).events.last_error.as_deref(),
        Some("unreachable")
    );

    // Let the stand-in finish the answer nobody is waiting for.
    knobs.delay_ms.store(0, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1300));
    daemon.tick(start + secs(3));
    assert!(rig.spooled().is_empty());
}

#[test]
fn a_rate_limit_is_waited_out() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    knobs.events_status.store(429, Ordering::SeqCst);
    knobs.retry_after.store(30, Ordering::SeqCst);
    needs_input(&rig, "s1");
    daemon.tick(start + secs(1));
    knobs.events_status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(20));
    assert_eq!(mock.to("/v1/events:ingest").len(), 1);
    daemon.tick(start + secs(31));
    assert_eq!(mock.to("/v1/events:ingest").len(), 2);
    assert!(rig.spooled().is_empty());
}

#[test]
fn a_rejected_token_stops_the_sending_and_a_new_login_resumes_it() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    knobs.events_status.store(401, Ordering::SeqCst);
    needs_input(&rig, "s1");
    daemon.tick(start + secs(1));
    daemon.tick(start + secs(2));
    daemon.tick(start + secs(30));
    assert_eq!(
        mock.to("/v1/events:ingest").len(),
        1,
        "kept sending a rejected token"
    );
    assert_eq!(daemon.state(true).token, "rejected");
    assert_eq!(rig.spooled().len(), 1);

    // nucleate login writes a new token; the daemon needs no restart.
    knobs.events_status.store(0, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1100));
    rig.sign_in(&mock.url);
    daemon.tick(start + secs(31));
    assert_eq!(daemon.state(true).token, "ok");
    assert!(rig.spooled().is_empty());
}

#[test]
fn an_event_watch_refuses_is_set_aside_and_the_rest_are_delivered() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    daemon.tick(start);
    for session in ["s1", "s2", "poison", "s3", "s4"] {
        emit(&rig, "Stop", session, json!({}));
    }
    needs_input(&rig, "s5");
    daemon.tick(start + secs(1));

    let delivered: Vec<Value> = batches(&mock)
        .into_iter()
        .filter(|batch| batch.iter().all(|event| event["session_id"] != "poison"))
        .flatten()
        .collect();
    let mut sessions: Vec<&str> = delivered
        .iter()
        .map(|e| e["session_id"].as_str().unwrap())
        .collect();
    sessions.sort();
    assert_eq!(sessions, ["s1", "s2", "s3", "s4", "s5"]);
    assert!(rig.spooled().is_empty());
    let aside = common::files_under(&rig.spool_dir().join("rejected"));
    assert_eq!(aside.len(), 1);
    assert!(String::from_utf8_lossy(&aside[0].1).contains("poison"));
    assert_eq!(daemon.state(true).events.rejected, 1);
    // It is not sent again.
    let before = mock.to("/v1/events:ingest").len();
    daemon.tick(start + secs(60));
    assert_eq!(mock.to("/v1/events:ingest").len(), before);
}

#[test]
fn an_event_in_the_responses_rejected_list_is_set_aside() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    daemon.tick(Instant::now());
    emit(&rig, "Stop", "stranger", json!({}));
    needs_input(&rig, "s1");
    daemon.tick(Instant::now());
    assert_eq!(mock.to("/v1/events:ingest").len(), 1);
    assert!(rig.spooled().is_empty());
    assert_eq!(
        common::files_under(&rig.spool_dir().join("rejected")).len(),
        1
    );
    assert_eq!(daemon.state(true).events.sent, 1);
}

#[test]
fn no_event_is_sent_before_a_heartbeat_has_named_its_environment() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    knobs.heartbeat_status.store(503, Ordering::SeqCst);
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    needs_input(&rig, "s1");
    daemon.tick(start);
    daemon.tick(start + secs(1));
    assert!(batches(&mock).is_empty());
    knobs.heartbeat_status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(5));
    assert_eq!(batches(&mock).len(), 1);
}

#[test]
fn past_its_limit_the_spool_drops_routine_events_before_needs_input() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    rig.sign_in(&mock.url);
    rig.configure(&format!(
        "[watch]\ntoken_file = \"{}\"\n[spool]\nmax_events = 5\n",
        rig.home.token_file().display()
    ));
    knobs.events_status.store(503, Ordering::SeqCst);
    let mut daemon = daemon(&rig);
    for session in ["s1", "s2", "s3"] {
        emit(&rig, "Stop", session, json!({}));
    }
    needs_input(&rig, "waiting");
    for session in ["s4", "s5", "s6", "s7"] {
        emit(&rig, "Stop", session, json!({}));
    }
    daemon.tick(Instant::now());
    let kept: Vec<String> = rig
        .spooled()
        .iter()
        .map(|record| record["event"]["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kept, ["waiting", "s4", "s5", "s6", "s7"]);
}

// ---- no target --------------------------------------------------------

#[test]
fn a_box_that_is_not_signed_in_sends_nothing_and_still_tracks_its_sessions() {
    let rig = Rig::new();
    let mut daemon = daemon(&rig);
    emit(&rig, "UserPromptSubmit", "s1", json!({}));
    needs_input(&rig, "s1");
    daemon.tick(Instant::now());
    // Applied to the local state, then gone from the spool.
    assert!(rig.spooled().is_empty());
    let sessions = nucleate::sessions::Store::new(&rig.home.state_dir()).list("client-a");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, nucleate::sessions::Status::NeedsInput);
    let state = daemon.state(true);
    assert_eq!(state.target, "none");
    assert_eq!(state.token, "missing");
    assert!(state.machine.is_some());
    assert_eq!(state.environments[0].name, "client-a");
}

#[test]
fn target_none_keeps_a_signed_in_box_from_sending() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    rig.configure(&format!(
        "target = \"none\"\n[watch]\ntoken_file = \"{}\"\n",
        rig.home.token_file().display()
    ));
    let mut daemon = daemon(&rig);
    needs_input(&rig, "s1");
    daemon.tick(Instant::now());
    daemon.tick(Instant::now() + secs(120));
    assert!(mock.requests().is_empty());
    assert!(rig.spooled().is_empty());
}

#[test]
fn a_token_file_others_can_read_is_not_sent_anywhere() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    std::fs::set_permissions(
        rig.home.token_file(),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let mut daemon = daemon(&rig);
    needs_input(&rig, "s1");
    daemon.tick(Instant::now());
    assert!(mock.requests().is_empty());
    assert_eq!(daemon.state(true).token, "insecure");
    assert_eq!(daemon.state(true).target, "none");
}

// ---- what the daemon writes down --------------------------------------

#[test]
fn the_daemons_state_file_is_private_and_holds_no_token() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    needs_input(&rig, "s1");
    daemon.tick(Instant::now());
    daemon.write_state(true);
    let file = rig.home.state_dir().join("daemon.json");
    assert_eq!(mode(&file), 0o600);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.contains(TOKEN));
    let state: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(state["running"], true);
    assert_eq!(state["target"], "hosted");
    assert_eq!(state["api_url"], mock.url);
    assert_eq!(state["events"]["sent"], 1);
    for (path, bytes) in common::files_under(&rig.home.state_dir()) {
        assert_eq!(mode(&path), 0o600, "{}", path.display());
        assert!(
            !String::from_utf8_lossy(&bytes).contains(TOKEN),
            "{}",
            path.display()
        );
    }
}

// ---- sessions whose agent has gone ------------------------------------

/// A stand-in for `/proc`, already holding one process: the daemon's own.
struct Procs(tempfile::TempDir);

impl Procs {
    fn new() -> Procs {
        let procs = Procs(tempfile::tempdir().unwrap());
        procs.start(&[7], 500);
        procs
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }

    /// Start a process: its pid in each pid namespace, outermost first, and
    /// its start time in clock ticks after boot.
    fn start(&self, pids: &[u32], start: u64) {
        let dir = self.path().join(pids[0].to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let nspid: Vec<String> = pids.iter().map(u32::to_string).collect();
        std::fs::write(
            dir.join("status"),
            format!(
                "Name:\tnode\nPid:\t{}\nNSpid:\t{}\n",
                pids[0],
                nspid.join("\t")
            ),
        )
        .unwrap();
        let filler = vec!["0"; 17].join(" ");
        std::fs::write(
            dir.join("stat"),
            format!("{} (node) S 1 {filler} {start} 0 0\n", pids[0]),
        )
        .unwrap();
    }

    fn stop(&self, host_pid: u32) {
        std::fs::remove_dir_all(self.path().join(host_pid.to_string())).unwrap();
    }
}

/// Write an event to the spool as a hook in a nucleus session would, from
/// an agent the hook saw as `pid`, started at `start`.
fn emit_from(rig: &Rig, kind: EventType, session: &str, pid: u32, start: u64) {
    let process = nucleate::process::Identity { pid, start };
    spool_event(rig, kind, session, Some(process));
}

/// Write an event to the spool as a hook in a nucleus session would, from
/// the agent `process`, if the hook could tell which that was.
fn spool_event(
    rig: &Rig,
    kind: EventType,
    session: &str,
    process: Option<nucleate::process::Identity>,
) {
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now() + Duration::from_millis(CLOCK.fetch_add(1, Ordering::SeqCst));
    let draft = Draft {
        env_id: "client-a".into(),
        session_id: session.into(),
        repos: vec!["web".into()],
        branch: Some("workspace/fix-login".into()),
        ..Draft::default()
    };
    Spool::new(&rig.spool_dir())
        .write(&Record {
            event: Envelope::new(kind, draft, now).unwrap(),
            at_ms: unix_ms(now),
            workspace: Some("fix-login".into()),
            process,
        })
        .unwrap();
}

fn status_of(rig: &Rig, session: &str) -> nucleate::sessions::Status {
    nucleate::sessions::Store::new(&rig.home.state_dir())
        .load("client-a", session)
        .unwrap()
        .status
}

#[test]
fn a_session_whose_agent_is_killed_ends_after_two_looks_and_once() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let procs = Procs::new();
    // In a container: 40 to the hook, 5040 to the daemon on the host.
    procs.start(&[5040, 40], 1_000);
    let mut daemon = daemon(&rig).with_proc(procs.path());
    let start = Instant::now();
    daemon.tick(start);
    emit_from(&rig, EventType::UserPromptSubmit, "s1", 40, 1_000);
    daemon.tick(start + secs(1));
    daemon.tick(start + secs(15));
    assert_eq!(types(&batches(&mock).concat()), ["user_prompt_submit"]);

    // Killed: no SessionEnd hook ran.
    procs.stop(5040);
    daemon.tick(start + secs(61));
    daemon.tick(start + secs(75));
    assert_eq!(
        status_of(&rig, "s1"),
        nucleate::sessions::Status::Working,
        "one look is not enough"
    );
    daemon.tick(start + secs(121));
    assert_eq!(status_of(&rig, "s1"), nucleate::sessions::Status::Ended);
    for later in [135, 181, 241, 301] {
        daemon.tick(start + secs(later));
    }

    let sent = batches(&mock).concat();
    assert_eq!(types(&sent), ["user_prompt_submit", "session_end"]);
    // The end is an envelope like any other, and says nothing of the process.
    let ended = sent[1].as_object().unwrap();
    let mut keys: Vec<&str> = ended.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "branch",
            "client_ts",
            "env_id",
            "event_id",
            "repos",
            "session_id",
            "type"
        ]
    );
    assert_eq!(ended["session_id"], "s1");
    assert_eq!(ended["repos"], json!(["web"]));
    assert_ne!(ended["event_id"], sent[0]["event_id"]);
    // The reporter is asked what the session left behind, as for a real end.
    assert!(rig
        .home
        .state_dir()
        .join("outcomes/client-a/s1.json")
        .exists());
    assert!(rig.spooled().is_empty());
}

#[test]
fn a_session_whose_agent_still_runs_or_whose_pid_was_not_recorded_is_never_ended() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let procs = Procs::new();
    procs.start(&[40], 1_000);
    let mut daemon = daemon(&rig).with_proc(procs.path());
    let start = Instant::now();
    daemon.tick(start);
    emit_from(&rig, EventType::UserPromptSubmit, "alive", 40, 1_000);
    // No process recorded, as from a hook on another system.
    spool_event(&rig, EventType::UserPromptSubmit, "unknown", None);
    for at in (1..=601).step_by(60) {
        daemon.tick(start + secs(at));
    }
    assert_eq!(
        status_of(&rig, "alive"),
        nucleate::sessions::Status::Working
    );
    assert_eq!(
        status_of(&rig, "unknown"),
        nucleate::sessions::Status::Working
    );
    assert!(!types(&batches(&mock).concat()).contains(&"session_end".to_string()));
}

#[test]
fn a_pid_used_again_by_another_process_is_not_the_agent() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let procs = Procs::new();
    // Something else now runs as 40, started after the agent did.
    procs.start(&[40], 9_000);
    let mut daemon = daemon(&rig).with_proc(procs.path());
    let start = Instant::now();
    daemon.tick(start);
    emit_from(&rig, EventType::Stop, "s1", 40, 1_000);
    for at in [1, 61, 121, 135] {
        daemon.tick(start + secs(at));
    }
    assert_eq!(status_of(&rig, "s1"), nucleate::sessions::Status::Ended);
    assert_eq!(types(&batches(&mock).concat()), ["stop", "session_end"]);
}

#[test]
fn a_session_that_ended_itself_is_not_ended_again_when_its_agent_exits() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let procs = Procs::new();
    procs.start(&[40], 1_000);
    let mut daemon = daemon(&rig).with_proc(procs.path());
    let start = Instant::now();
    daemon.tick(start);
    emit_from(&rig, EventType::UserPromptSubmit, "s1", 40, 1_000);
    daemon.tick(start + secs(1));
    // The SessionEnd hook runs, then the agent exits, before the next look.
    emit_from(&rig, EventType::SessionEnd, "s1", 40, 1_000);
    procs.stop(40);
    for at in [61, 121, 181, 241] {
        daemon.tick(start + secs(at));
    }
    assert_eq!(
        types(&batches(&mock).concat()),
        ["user_prompt_submit", "session_end"]
    );
}

#[test]
fn a_session_seen_again_by_a_new_agent_is_looked_for_as_that_one() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    let procs = Procs::new();
    procs.start(&[40], 1_000);
    let mut daemon = daemon(&rig).with_proc(procs.path());
    let start = Instant::now();
    daemon.tick(start);
    emit_from(&rig, EventType::UserPromptSubmit, "s1", 40, 1_000);
    daemon.tick(start + secs(1));
    // The agent goes, and the session is resumed by another before the
    // second look: that one is the session's agent now.
    procs.stop(40);
    daemon.tick(start + secs(61));
    procs.start(&[41], 2_000);
    emit_from(&rig, EventType::UserPromptSubmit, "s1", 41, 2_000);
    for at in [62, 121, 181, 241] {
        daemon.tick(start + secs(at));
    }
    assert_eq!(status_of(&rig, "s1"), nucleate::sessions::Status::Working);
    assert!(!types(&batches(&mock).concat()).contains(&"session_end".to_string()));
}

#[test]
fn a_hook_records_its_agents_process_on_the_box_and_only_on_linux() {
    let rig = Rig::new();
    let out = rig.hook(
        "UserPromptSubmit",
        &json!({ "session_id": "s1", "cwd": rig.root }),
    );
    assert!(out.status.success());
    let spooled = rig.spooled();
    assert_eq!(spooled.len(), 1);
    // The test is the agent here: the process that ran the hook.
    if cfg!(target_os = "linux") {
        assert_eq!(spooled[0]["process"]["pid"], std::process::id());
    } else {
        assert!(spooled[0].get("process").is_none());
    }
    assert!(spooled[0]["event"].get("process").is_none());
}
