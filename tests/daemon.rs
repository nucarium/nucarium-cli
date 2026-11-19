//! The daemon's loop against a local stand-in for the Watch API: the
//! heartbeat, batching, "needs input" at once, retry with backoff, and replay
//! after an outage.

mod common;

use common::{dead_api, mode, Mock, Reply, Request, Rig, TOKEN};
use nucleate::daemon::{Daemon, Timings};
use nucleate::hook;
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
            "/v1/ingest/heartbeat" => match seen.heartbeat_status.load(Ordering::SeqCst) {
                0 => Reply::json(
                    200,
                    json!({
                        "received_at": "2026-11-19T12:00:00.000Z",
                        "heartbeat_interval_s": seen.heartbeat_interval.load(Ordering::SeqCst),
                    }),
                ),
                status => problem(status),
            },
            "/v1/ingest/events" => match seen.events_status.load(Ordering::SeqCst) {
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
    let mut payload = json!({ "session_id": session, "cwd": rig.root });
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        payload[key] = value;
    }
    let now = SystemTime::now() + Duration::from_millis(CLOCK.fetch_add(1, Ordering::SeqCst));
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

/// The batches of events the API received, in order.
fn batches(mock: &Mock) -> Vec<Vec<Value>> {
    mock.to("/v1/ingest/events")
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

    let beats = mock.to("/v1/ingest/heartbeat");
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
    let body = beat.json();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["client_ts", "daemon_version", "environments", "machine"]
    );
    assert_eq!(
        body["environments"],
        json!([{ "env_id": "client-a", "server_alive": true }])
    );
    assert_eq!(body["daemon_version"], env!("CARGO_PKG_VERSION"));
    let machine = body["machine"].as_object().unwrap();
    for key in machine.keys() {
        assert!(
            [
                "disk_used_pct",
                "load_1m",
                "uptime_s",
                "memory_pressure_pct"
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

#[test]
fn an_agent_server_that_is_not_running_is_reported_as_such() {
    let rig = Rig::new();
    let (mock, _) = api();
    rig.sign_in(&mock.url);
    rig.configure_with("check = \"false\"", "");
    daemon(&rig).tick(Instant::now());
    assert_eq!(
        mock.to("/v1/ingest/heartbeat")[0].json()["environments"],
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
    let beats = || mock.to("/v1/ingest/heartbeat").len();
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
    assert_eq!(mock.to("/v1/ingest/heartbeat").len(), 1);
    daemon.tick(start + secs(600));
    assert_eq!(mock.to("/v1/ingest/heartbeat").len(), 2);
}

#[test]
fn a_failed_heartbeat_is_tried_again_before_the_next_one_is_due() {
    let rig = Rig::new();
    let (mock, knobs) = api();
    knobs.heartbeat_status.store(503, Ordering::SeqCst);
    rig.sign_in(&mock.url);
    let mut daemon = daemon(&rig);
    let start = Instant::now();
    let beats = || mock.to("/v1/ingest/heartbeat").len();
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
    let beats = mock.to("/v1/ingest/heartbeat");
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

    let request = &mock.to("/v1/ingest/events")[0];
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
    let attempts = || mock.to("/v1/ingest/events").len();
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
    let attempts = || mock.to("/v1/ingest/events").len();
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
    assert_eq!(mock.to("/v1/ingest/events").len(), 1);
    daemon.tick(start + secs(31));
    assert_eq!(mock.to("/v1/ingest/events").len(), 2);
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
        mock.to("/v1/ingest/events").len(),
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
    let before = mock.to("/v1/ingest/events").len();
    daemon.tick(start + secs(60));
    assert_eq!(mock.to("/v1/ingest/events").len(), before);
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
    assert_eq!(mock.to("/v1/ingest/events").len(), 1);
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
