//! What the box reports beyond its sessions' statuses: unsaved work, what a
//! session left behind, the supervisor's signals, the maintenance pause, and
//! what the box posts for itself while Watch cannot be reached.
//!
//! Everything timed is tested by moving the clock: the daemon's loop and the
//! reporter are each given the time, and nothing here waits for one.

mod common;

use common::{
    contract, git, github, path_with, run, stderr, stdout_json, Mock, Reply, Request, Rig, TOKEN,
};
use nucleate::daemon::{Daemon, Timings};
use nucleate::envelope::rfc3339;
use nucleate::reporter::Reporter;
use nucleate::sessions::{Status, Store};
use nucleate::{hook, housekeeping, pause};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What the stand-in Watch does next. A status of 0 means "answer normally".
#[derive(Default)]
struct Knobs {
    status: AtomicU16,
}

/// A stand-in for Watch that holds the contract as Watch does, and can be
/// made to fail.
fn watch() -> (Mock, Arc<Knobs>) {
    let knobs = Arc::new(Knobs::default());
    let seen = knobs.clone();
    let mock = Mock::start(move |request: &Request| {
        if let Some(refused) = contract::refusal(request) {
            return refused;
        }
        let received = json!("2026-11-19T12:00:00.000Z");
        match (seen.status.load(Ordering::SeqCst), request.path.as_str()) {
            (0, contract::HEARTBEATS) => Reply::json(
                200,
                json!({ "received_at": received, "heartbeat_interval_s": 60 }),
            ),
            (0, contract::EVENTS) => Reply::json(
                200,
                json!({ "received_at": received, "accepted": 1, "duplicates": 0, "rejected": [] }),
            ),
            (0, _) => Reply::problem(404, "not_found"),
            (401, _) => Reply::problem(401, "unauthorized"),
            (429, _) => Reply::problem(429, "rate_limited").header("Retry-After", "30"),
            (status, _) => Reply::problem(status, "internal_error"),
        }
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

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Record a hook event for a session working in `cwd`, as the entry point
/// would, each a millisecond after the last.
fn emit(rig: &Rig, event: &str, session: &str, cwd: &Path, extra: Value) {
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now() + Duration::from_millis(CLOCK.fetch_add(1, Ordering::SeqCst));
    let mut payload = json!({ "session_id": session, "cwd": cwd });
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

fn heartbeats(mock: &Mock) -> Vec<Value> {
    assert_eq!(contract::violations(mock), Vec::<String>::new());
    mock.to(contract::HEARTBEATS)
        .iter()
        .map(Request::json)
        .collect()
}

/// Every event the stand-in Watch was sent, in order.
fn events(mock: &Mock) -> Vec<Value> {
    assert_eq!(contract::violations(mock), Vec::<String>::new());
    mock.to(contract::EVENTS)
        .iter()
        .flat_map(|request| request.json()["events"].as_array().cloned().unwrap())
        .collect()
}

fn outcomes(mock: &Mock) -> Vec<Value> {
    events(mock)
        .into_iter()
        .filter(|event| event["type"] == "outcome")
        .collect()
}

/// Send one heartbeat from a daemon made for the purpose, and return it.
fn one_heartbeat(rig: &Rig, mock: &Mock) -> Value {
    Daemon::new(rig.settings(), timings()).tick(Instant::now());
    heartbeats(mock).pop().expect("a heartbeat")
}

fn commit(worktree: &Path, file: &str) {
    fs::write(worktree.join(file), "work\n").unwrap();
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-q", "-m", "work"]);
}

/// Set a file's modification time well into the past, so it no longer looks
/// like something being written.
fn age(path: &Path) {
    assert!(std::process::Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(path)
        .status()
        .unwrap()
        .success());
}

fn parsed(stamp: &Value) -> SystemTime {
    // `2026-11-19T12:00:00.000Z`, as the daemon writes every time it sends.
    let stamp = stamp.as_str().expect("a timestamp");
    let number = |range: std::ops::Range<usize>| stamp[range].parse::<u64>().unwrap();
    let (year, month, day) = (number(0..4), number(5..7), number(8..10));
    let days_before_month = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap_days = |y: u64| (y - 1969) / 4 - (y - 1901) / 100 + (y - 1601) / 400;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = (year - 1970) * 365
        + leap_days(year)
        + days_before_month[month as usize - 1]
        + u64::from(leap && month > 2)
        + day
        - 1;
    let seconds = days * 86_400 + number(11..13) * 3_600 + number(14..16) * 60 + number(17..19);
    UNIX_EPOCH + Duration::from_millis(seconds * 1_000 + number(20..23))
}

fn near(stamp: &Value, want: SystemTime) -> bool {
    let got = parsed(stamp);
    let apart = got
        .duration_since(want)
        .unwrap_or_else(|before| before.duration());
    apart < secs(30)
}

#[test]
fn the_test_clock_reads_back_what_the_daemon_writes() {
    let now = UNIX_EPOCH + Duration::from_millis(1_795_089_603_120);
    assert_eq!(parsed(&json!(rfc3339(now))), now);
    let leap = UNIX_EPOCH + Duration::from_millis(951_782_400_000);
    assert_eq!(parsed(&json!(rfc3339(leap))), leap);
}

// ---- unsaved work ---------------------------------------------------------

#[test]
fn unsaved_work_is_counted_on_a_schedule_and_a_heartbeat_reads_the_count() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let clean = rig.session("clean");

    // Before any count, a heartbeat says nothing about unsaved work.
    let before = one_heartbeat(&rig, &mock);
    assert!(before["environments"][0].get("unsaved").is_none());
    assert!(before["machine"].get("uncommitted_repos").is_none());

    // Two commits nobody has, and a file not committed.
    commit(&worktree, "a.txt");
    commit(&worktree, "b.txt");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let start = SystemTime::now();
    let mut reporter = Reporter::default();
    reporter.tick(&rig.settings(), start);

    let beat = one_heartbeat(&rig, &mock);
    let unsaved = &beat["environments"][0]["unsaved"];
    assert_eq!(unsaved["uncommitted_repos"], 1);
    assert_eq!(unsaved["unpushed_commits"], 2);
    assert!(near(&unsaved["counted_at"], start));
    assert_eq!(beat["machine"]["uncommitted_repos"], 1);
    assert_eq!(beat["machine"]["unpushed_commits"], 2);
    // Counts and a time. There is no room for a name.
    let keys: Vec<&String> = unsaved.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["counted_at", "uncommitted_repos", "unpushed_commits"]
    );
    // The box shows its owner the same thing it tells Watch.
    let view = stdout_json(&run(rig.nucleated().args(["status", "--json"]), ""));
    assert_eq!(view["environments"][0]["unsaved"]["unpushed_commits"], 2);
    assert_eq!(view["machine"]["uncommitted_repos"], 1);
    let text = String::from_utf8(run(rig.nucleated().arg("status"), "").stdout).unwrap();
    assert!(
        text.contains("1 repo with uncommitted changes · 2 commits not pushed (counted "),
        "{text}"
    );

    // More work, and no count yet: the heartbeat goes on reading the last
    // count. It runs no git of its own, however often it is sent.
    fs::write(clean.join("more.txt"), "more\n").unwrap();
    reporter.tick(&rig.settings(), start + secs(599));
    let stale = one_heartbeat(&rig, &mock);
    assert_eq!(stale["environments"][0]["unsaved"], *unsaved);

    // Ten minutes after the last count, the next one.
    reporter.tick(&rig.settings(), start + secs(600));
    let fresh = one_heartbeat(&rig, &mock);
    assert_eq!(fresh["environments"][0]["unsaved"]["uncommitted_repos"], 2);
    assert_eq!(fresh["environments"][0]["unsaved"]["unpushed_commits"], 2);

    // Pushed and committed, it is saved.
    rig.push(&worktree);
    fs::remove_file(worktree.join("notes.txt")).unwrap();
    fs::remove_file(clean.join("more.txt")).unwrap();
    reporter.tick(&rig.settings(), start + secs(1_200));
    let saved = one_heartbeat(&rig, &mock);
    assert_eq!(saved["environments"][0]["unsaved"]["uncommitted_repos"], 0);
    assert_eq!(saved["environments"][0]["unsaved"]["unpushed_commits"], 0);
    assert_eq!(saved["machine"]["unpushed_commits"], 0);
}

#[test]
fn the_count_follows_the_interval_the_config_sets_and_stops_when_it_is_off() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    rig.configure("[report]\nunsaved = \"1m\"\n");
    let start = SystemTime::now();
    let mut reporter = Reporter::default();
    reporter.tick(&rig.settings(), start);
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    reporter.tick(&rig.settings(), start + secs(59));
    assert_eq!(
        one_heartbeat(&rig, &mock)["environments"][0]["unsaved"]["uncommitted_repos"],
        0
    );
    reporter.tick(&rig.settings(), start + secs(60));
    assert_eq!(
        one_heartbeat(&rig, &mock)["environments"][0]["unsaved"]["uncommitted_repos"],
        1
    );

    // Off: no count is made, and the last one is no longer reported.
    rig.configure("[report]\nunsaved = \"off\"\n");
    reporter.tick(&rig.settings(), start + secs(3_600));
    let off = one_heartbeat(&rig, &mock);
    assert!(off["environments"][0].get("unsaved").is_none());
    assert!(off["machine"].get("uncommitted_repos").is_none());
    assert!(!rig.home.state_dir().join("unsaved.json").exists());
}

#[test]
fn two_logins_sharing_a_nucleus_report_the_same_counts_and_the_box_counts_them_once() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    rig.home.write_config(&format!(
        "machine = \"agentbox\"\nstate_dir = \"{state}\"\n\
         [[environment]]\nname = \"client-a\"\nroot = \"{root}\"\ncheck = \"true\"\n\
         [[environment]]\nname = \"client-b\"\nroot = \"{root}\"\ncheck = \"true\"\n",
        state = rig.home.state_dir().display(),
        root = rig.root.display(),
    ));
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    Reporter::default().tick(&rig.settings(), SystemTime::now());

    let beat = one_heartbeat(&rig, &mock);
    let environments = beat["environments"].as_array().unwrap();
    assert_eq!(environments.len(), 2);
    assert_eq!(environments[0]["unsaved"]["unpushed_commits"], 1);
    assert_eq!(environments[0]["unsaved"], environments[1]["unsaved"]);
    assert_eq!(beat["machine"]["uncommitted_repos"], 1);
    assert_eq!(beat["machine"]["unpushed_commits"], 1);
}

#[test]
fn a_worktree_that_cannot_be_read_makes_the_count_unknown_and_not_zero() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    // The worktree's link to its repository is gone: git can say nothing
    // about it, and neither does the box.
    fs::remove_file(worktree.join(".git")).unwrap();
    Reporter::default().tick(&rig.settings(), SystemTime::now());
    let beat = one_heartbeat(&rig, &mock);
    assert!(beat["environments"][0].get("unsaved").is_none());
    assert!(beat["machine"].get("uncommitted_repos").is_none());
    assert!(beat["machine"].get("unpushed_commits").is_none());
}

#[test]
fn the_last_good_snapshot_is_reported_and_a_failed_one_does_not_move_it() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let settings = rig.settings();
    let start = SystemTime::now() - secs(7_200);
    let mut reporter = Reporter::default();

    // Never snapshotted: nothing is said.
    reporter.tick(&settings, start);
    let never = one_heartbeat(&rig, &mock);
    assert!(never["environments"][0].get("last_snapshot_at").is_none());
    assert!(never["machine"].get("last_snapshot_at").is_none());

    // A run with nothing to push fails at nothing, so it is a good one.
    housekeeping::run_due(&settings, start + secs(60)).unwrap();
    reporter.tick(&settings, start + secs(600));
    let good = one_heartbeat(&rig, &mock);
    assert!(near(
        &good["environments"][0]["last_snapshot_at"],
        start + secs(60)
    ));
    assert_eq!(
        good["machine"]["last_snapshot_at"],
        good["environments"][0]["last_snapshot_at"]
    );

    // The remote goes away with work waiting: the next run fails, and the
    // time of the last good one stands.
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    age(&worktree.join("notes.txt"));
    fs::rename(&rig.upstream, rig.upstream.with_extension("gone")).unwrap();
    housekeeping::run_due(&settings, start + secs(3_700)).unwrap();
    let ledger = housekeeping::read_ledger(&settings.state_dir);
    assert_eq!(ledger["client-a/snapshot"].summary, "1 failed");
    reporter.tick(&settings, start + secs(3_800));
    let after = one_heartbeat(&rig, &mock);
    assert_eq!(
        after["environments"][0]["last_snapshot_at"],
        good["environments"][0]["last_snapshot_at"]
    );
}

// ---- what a session left behind -------------------------------------------

/// The pull requests GitHub lists for a commit: one that is open with this
/// commit as its head, and others that must not be reported.
fn listed(slug: &str, sha: &str) -> Value {
    let pull = |number: u32, state: &str, head: &str, on: &str| {
        json!({
            "html_url": format!("https://github.com/{on}/pull/{number}"),
            "number": number,
            "state": state,
            "merged_at": null,
            "title": "A title that must stay on GitHub",
            "body": "A description that must stay on GitHub",
            "user": { "login": "someone" },
            "head": { "sha": head, "ref": "workspace/fix-login" },
        })
    };
    let mut odd = pull(9, "open", sha, slug);
    odd["html_url"] = json!(format!("https://github.com/{slug}/pull/9?title=A+title"));
    json!([
        pull(412, "open", sha, slug),
        pull(400, "closed", sha, slug),
        pull(401, "open", &"0".repeat(40), slug),
        pull(7, "open", sha, "someone-else/web"),
        odd,
    ])
}

fn stopped_session(rig: &Rig, worktree: &Path) -> Daemon {
    emit(rig, "UserPromptSubmit", "s1", worktree, json!({}));
    emit(rig, "Stop", "s1", worktree, json!({}));
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(Instant::now());
    daemon
}

#[test]
fn a_session_that_stops_reports_what_it_left_behind() {
    let rig = Rig::new();
    rig.as_github("your-org/web");
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let stub = github(|slug, sha| Reply::json(200, listed(slug, sha)));
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    commit(&worktree, "b.txt");
    commit(&worktree, "c.txt");
    rig.push(&worktree);
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();

    let start = Instant::now();
    let mut daemon = stopped_session(&rig, &worktree);
    assert!(outcomes(&mock).is_empty());
    let mut reporter = Reporter::default().asking_github_at(&stub.url);
    reporter.tick(&rig.settings(), SystemTime::now() + secs(11));
    daemon.tick(start + secs(30));

    let sent = outcomes(&mock);
    assert_eq!(sent.len(), 1, "{:?}", events(&mock));
    let outcome = &sent[0];
    assert_eq!(outcome["env_id"], "client-a");
    assert_eq!(outcome["session_id"], "s1");
    assert_eq!(outcome["repos"], json!(["your-org/web"]));
    assert_eq!(outcome["branch"], "workspace/fix-login");
    assert_eq!(outcome["commits"], 3);
    assert_eq!(outcome["uncommitted_repos"], 1);
    assert_eq!(outcome["unpushed_commits"], 0);
    // The one that is open, on this repo, with this commit as its head.
    assert_eq!(
        outcome["pull_requests"],
        json!(["https://github.com/your-org/web/pull/412"])
    );
    // GitHub was asked the one question, about the session's own commit.
    let asked = stub.requests();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].path,
        format!(
            "/repos/your-org/web/commits/{}/pulls",
            git(&worktree, &["rev-parse", "HEAD"])
        )
    );

    // The session is where it was: an outcome moves nothing.
    let session = Store::new(&rig.home.state_dir())
        .load("client-a", "s1")
        .unwrap();
    assert_eq!(session.status, Status::Idle);
    let view = stdout_json(&run(rig.nucleated().args(["status", "--json"]), ""));
    assert_eq!(view["inbox"][0]["status"], "idle");
    assert_eq!(view["inbox"][0]["outcome"]["commits"], 3);
    assert_eq!(
        view["inbox"][0]["outcome"]["pull_requests"],
        json!(["https://github.com/your-org/web/pull/412"])
    );
    let text = String::from_utf8(run(rig.nucleated().arg("status"), "").stdout).unwrap();
    assert!(
        text.contains("3 commits · 1 pull request · 1 repo with uncommitted changes"),
        "{text}"
    );
}

#[test]
fn an_outcome_waits_ten_seconds_and_comes_at_most_once_a_minute_for_a_session() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let (start, wall) = (Instant::now(), SystemTime::now());
    let mut daemon = stopped_session(&rig, &worktree);
    let mut reporter = Reporter::default();
    let spooled_outcomes = || {
        rig.spooled()
            .iter()
            .filter(|record| record["event"]["type"] == "outcome")
            .count()
    };

    // The session may still be pushing: nothing is looked at yet.
    reporter.tick(&rig.settings(), wall + secs(5));
    assert_eq!(spooled_outcomes(), 0);
    reporter.tick(&rig.settings(), wall + secs(11));
    assert_eq!(spooled_outcomes(), 1);
    // Once, however often the reporter looks.
    reporter.tick(&rig.settings(), wall + secs(13));
    assert_eq!(spooled_outcomes(), 1);
    daemon.tick(start + secs(30));
    assert_eq!(outcomes(&mock).len(), 1);
    assert_eq!(outcomes(&mock)[0]["commits"], 0);

    // It works and stops twice more within the minute: one more look, a
    // minute after the last, and it sees the work as it then is.
    for file in ["a.txt", "b.txt"] {
        emit(&rig, "UserPromptSubmit", "s1", &worktree, json!({}));
        commit(&worktree, file);
        emit(&rig, "Stop", "s1", &worktree, json!({}));
        daemon.tick(start + secs(31));
    }
    reporter.tick(&rig.settings(), wall + secs(70));
    assert_eq!(spooled_outcomes(), 0);
    reporter.tick(&rig.settings(), wall + secs(72));
    assert_eq!(spooled_outcomes(), 1);
    reporter.tick(&rig.settings(), wall + secs(200));
    assert_eq!(spooled_outcomes(), 1);
    daemon.tick(start + secs(90));
    let sent = outcomes(&mock);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["commits"], 2);
    assert_eq!(sent[1]["unpushed_commits"], 2);
}

#[test]
fn a_session_that_ends_holding_unsaved_work_says_how_much() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    emit(&rig, "UserPromptSubmit", "s1", &worktree, json!({}));
    emit(&rig, "SessionEnd", "s1", &worktree, json!({}));
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    Reporter::default().tick(&rig.settings(), SystemTime::now() + secs(11));
    daemon.tick(start + secs(30));

    let sent = outcomes(&mock);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["uncommitted_repos"], 1);
    assert_eq!(sent[0]["unpushed_commits"], 1);
    assert_eq!(sent[0]["commits"], 1);
    // Its commit is on no remote, so GitHub has nothing to say about it and
    // the box says nothing about pull requests either way.
    assert!(sent[0].get("pull_requests").is_none());
    // The session stays ended, with what it held.
    let session = Store::new(&rig.home.state_dir())
        .load("client-a", "s1")
        .unwrap();
    assert_eq!(session.status, Status::Ended);
    assert_eq!(session.outcome.unwrap().unpushed_commits, Some(1));
}

#[test]
fn a_session_whose_folder_is_gone_reports_nothing_unsaved_and_nothing_else() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    emit(&rig, "UserPromptSubmit", "s1", &worktree, json!({}));
    emit(&rig, "SessionEnd", "s1", &worktree, json!({}));
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    let removed = run(rig.nucleate().args(["remove-session", "fix-login"]), "");
    assert!(removed.status.success(), "{}", stderr(&removed));
    Reporter::default().tick(&rig.settings(), SystemTime::now() + secs(11));
    daemon.tick(start + secs(30));

    let sent = outcomes(&mock);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["uncommitted_repos"], 0);
    assert_eq!(sent[0]["unpushed_commits"], 0);
    assert!(sent[0].get("commits").is_none());
    assert!(sent[0].get("pull_requests").is_none());
}

#[test]
fn a_session_outside_a_nucleus_reports_no_outcome() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    // A plain checkout: nucleate made no folder for it and keeps no record
    // of where it started.
    let checkout = rig.home.dir.join("projects").join("api");
    fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q", "-b", "main"]);
    emit(&rig, "UserPromptSubmit", "s1", &checkout, json!({}));
    emit(&rig, "Stop", "s1", &checkout, json!({}));
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    Reporter::default().tick(&rig.settings(), SystemTime::now() + secs(11));
    daemon.tick(start + secs(30));
    assert_eq!(events(&mock).len(), 2);
    assert!(outcomes(&mock).is_empty());
}

#[test]
fn when_github_gives_no_answer_the_outcome_says_nothing_about_pull_requests() {
    let rig = Rig::new();
    rig.as_github("your-org/web");
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let stub = github(|_, _| Reply::json(500, json!({ "message": "Server Error" })));
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    rig.push(&worktree);
    let start = Instant::now();
    let mut daemon = stopped_session(&rig, &worktree);
    Reporter::default()
        .asking_github_at(&stub.url)
        .tick(&rig.settings(), SystemTime::now() + secs(11));
    daemon.tick(start + secs(30));

    let sent = outcomes(&mock);
    assert_eq!(sent.len(), 1);
    assert_eq!(stub.requests().len(), 1);
    // Not known is not none: the field is left out, and what Watch had stands.
    assert!(sent[0].get("pull_requests").is_none());
    assert_eq!(sent[0]["commits"], 1);
    assert_eq!(sent[0]["unpushed_commits"], 0);
}

#[test]
fn a_commit_made_after_the_push_does_not_say_the_pull_request_is_gone() {
    let rig = Rig::new();
    rig.as_github("your-org/web");
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let stub = github(|slug, sha| Reply::json(200, listed(slug, sha)));
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    rig.push(&worktree);
    // The pull request is open on what was pushed. One more commit stays on the box.
    commit(&worktree, "b.txt");

    let start = Instant::now();
    let mut daemon = stopped_session(&rig, &worktree);
    Reporter::default()
        .asking_github_at(&stub.url)
        .tick(&rig.settings(), SystemTime::now() + secs(11));
    daemon.tick(start + secs(30));

    let sent = outcomes(&mock);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["commits"], 2);
    assert_eq!(sent[0]["unpushed_commits"], 1);
    // Not an empty list, which would tell Watch there are none: no word at
    // all, so Watch keeps the address it last heard.
    assert!(sent[0].get("pull_requests").is_none(), "{}", sent[0]);
    assert!(stub.requests().is_empty());
}

#[test]
fn a_session_that_has_committed_nothing_asks_github_nothing() {
    let rig = Rig::new();
    rig.as_github("your-org/web");
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let stub = github(|slug, sha| Reply::json(200, listed(slug, sha)));
    let worktree = rig.session("fix-login");
    let start = Instant::now();
    let mut daemon = stopped_session(&rig, &worktree);
    Reporter::default()
        .asking_github_at(&stub.url)
        .tick(&rig.settings(), SystemTime::now() + secs(11));
    daemon.tick(start + secs(30));
    assert!(stub.requests().is_empty());
    let sent = outcomes(&mock);
    assert_eq!(sent[0]["commits"], 0);
    assert_eq!(sent[0]["pull_requests"], json!([]));
}

// ---- the supervisor's signals ---------------------------------------------

const SYSTEMCTL: &str = r#"case "$*" in
  "--user is-active --quiet agent-server@client-a.service") exit 0 ;;
  "--user cat agent-server@client-a.service") exit 0 ;;
  "--user show --property=NRestarts --value agent-server@client-a.service") echo "$RESTARTS" ;;
  *) exit 1 ;;
esac"#;

const UPSC: &str = r#"[ "$1" = "ups@localhost" ] && [ "$2" = "ups.status" ] || exit 1
[ -n "$UPS_STATUS" ] || exit 1
echo "$UPS_STATUS""#;

/// Run `nucleated check` on a box whose systemd and NUT answer as given, and
/// return the report and the heartbeat it sent.
fn check_with(restarts: &str, ups_status: &str) -> (Value, Value) {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    // No `check` command: the agent server is the systemd unit.
    rig.configure_with("", "[ups]\nname = \"ups@localhost\"\n");
    let path = path_with(&rig.home, &[("systemctl", SYSTEMCTL), ("upsc", UPSC)]);
    let out = run(
        rig.nucleated()
            .arg("check")
            .env("PATH", path)
            .env("RESTARTS", restarts)
            .env("UPS_STATUS", ups_status),
        "",
    );
    (
        stdout_json(&out),
        heartbeats(&mock).pop().expect("a heartbeat"),
    )
}

#[test]
fn the_heartbeat_says_how_often_the_agent_server_restarted_and_whether_the_box_is_on_battery() {
    let (report, beat) = check_with("4", "OB DISCHRG");
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(
        beat["environments"],
        json!([{ "env_id": "client-a", "server_alive": true, "server_restarts": 4 }])
    );
    assert_eq!(beat["machine"]["on_battery"], true);
    let ups = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "ups")
        .unwrap();
    assert_eq!(ups["detail"], "ups@localhost: on battery");

    let (_, beat) = check_with("0", "OL CHRG");
    assert_eq!(beat["environments"][0]["server_restarts"], 0);
    assert_eq!(beat["machine"]["on_battery"], false);
}

#[test]
fn a_probe_that_gets_no_answer_leaves_its_field_out() {
    // systemd prints something that is not a count, and NUT does not answer.
    let (report, beat) = check_with("not a number", "");
    assert_eq!(
        beat["environments"],
        json!([{ "env_id": "client-a", "server_alive": true }])
    );
    assert!(beat["machine"].get("on_battery").is_none());
    // The heartbeat still went, and `check` names what did not answer.
    let failed: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["ok"] == false)
        .map(|check| check["name"].as_str().unwrap())
        .collect();
    assert_eq!(failed, ["ups"]);
}

#[test]
fn a_box_with_no_ups_says_nothing_about_a_battery() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    // An `upsc` that would say "on battery", and no UPS in the config.
    let path = path_with(&rig.home, &[("upsc", "echo OB")]);
    let out = run(rig.nucleated().arg("check").env("PATH", path), "");
    assert_eq!(stdout_json(&out)["ok"], true);
    assert!(heartbeats(&mock)[0]["machine"].get("on_battery").is_none());
}

// ---- the maintenance pause ------------------------------------------------

#[test]
fn pausing_tells_watch_at_once_and_resuming_ends_it() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    assert!(heartbeats(&mock)[0].get("paused_until").is_none());

    let files_before = common::files_under(&rig.home.state_dir()).len();
    let out = run(rig.nucleated().args(["pause", "2h"]), "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = stdout_json(&out);
    assert_eq!(said["paused"], true);
    let until = SystemTime::now() + secs(7_200);
    assert!(near(&said["until"], until));
    // One small file, and nothing else on the box is touched.
    assert_eq!(
        common::files_under(&rig.home.state_dir()).len(),
        files_before + 1
    );

    // The next heartbeat is not due for a minute. It goes now.
    daemon.tick(start + secs(1));
    let beats = heartbeats(&mock);
    assert_eq!(beats.len(), 2);
    assert_eq!(beats[1]["paused_until"], said["until"]);
    // The box goes on reporting as it did.
    assert_eq!(beats[1]["environments"], beats[0]["environments"]);
    daemon.tick(start + secs(2));
    assert_eq!(heartbeats(&mock).len(), 2);
    let text = String::from_utf8(run(rig.nucleated().arg("status"), "").stdout).unwrap();
    assert!(text.contains("Paused for maintenance until "), "{text}");

    let out = run(rig.nucleated().arg("resume"), "");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout_json(&out),
        json!({ "paused": false, "was_paused": true })
    );
    daemon.tick(start + secs(3));
    let beats = heartbeats(&mock);
    assert_eq!(beats.len(), 3);
    assert!(beats[2].get("paused_until").is_none());
    // Resuming with no pause is harmless.
    assert_eq!(
        stdout_json(&run(rig.nucleated().arg("resume"), "")),
        json!({ "paused": false, "was_paused": false })
    );
}

#[test]
fn a_pause_is_more_than_nothing_at_most_a_day_and_ends_by_itself() {
    let rig = Rig::new();
    let (mock, _) = watch();
    rig.sign_in(&mock.url);
    for refused in ["0s", "25h", "2d", "soon", "2"] {
        let out = run(rig.nucleated().args(["pause", refused]), "");
        assert_eq!(out.status.code(), Some(2), "pause {refused}");
        assert!(out.stdout.is_empty());
        assert!(!pause::file(&rig.home.state_dir()).exists());
    }
    assert_eq!(
        run(rig.nucleated().args(["pause", "24h"]), "")
            .status
            .code(),
        Some(0)
    );
    assert!(one_heartbeat(&rig, &mock).get("paused_until").is_some());

    // A pause whose time has passed is over, with nobody ending it.
    pause::set(
        &rig.home.state_dir(),
        secs(600),
        SystemTime::now() - secs(601),
    )
    .unwrap();
    assert!(one_heartbeat(&rig, &mock).get("paused_until").is_none());
}

// ---- the box speaking for itself ------------------------------------------

/// The owner's own address: a stand-in for ntfy that can be made to fail.
fn ntfy() -> (Mock, Arc<Knobs>) {
    let knobs = Arc::new(Knobs::default());
    let seen = knobs.clone();
    let mock = Mock::start(
        move |_: &Request| match seen.status.load(Ordering::SeqCst) {
            0 => Reply::json(200, json!({ "id": "x" })),
            status => Reply::json(status, json!({ "error": "no" })),
        },
    );
    (mock, knobs)
}

struct Outage {
    rig: Rig,
    watch: Mock,
    watch_knobs: Arc<Knobs>,
    ntfy: Mock,
    ntfy_knobs: Arc<Knobs>,
}

/// A box that reports to a Watch that has stopped answering, with a
/// notifier pointed at the owner's address.
fn outage(notify: &str) -> Outage {
    let rig = Rig::new();
    let (watch, watch_knobs) = watch();
    let (ntfy, ntfy_knobs) = ntfy();
    rig.sign_in(&watch.url);
    rig.configure(&notify.replace("{ntfy}", &ntfy.url));
    watch_knobs.status.store(503, Ordering::SeqCst);
    Outage {
        rig,
        watch,
        watch_knobs,
        ntfy,
        ntfy_knobs,
    }
}

const NOTIFY: &str = "[notify]\nurl = \"{ntfy}/your-private-topic\"\n";

fn kinds(mock: &Mock) -> Vec<String> {
    mock.requests()
        .iter()
        .map(|post| post.header("x-nucarium-alert-kind").unwrap().to_string())
        .collect()
}

/// `13:40 UTC`, from a time in Unix milliseconds.
fn clock(ms: u64) -> String {
    format!(
        "{} UTC",
        &rfc3339(UNIX_EPOCH + Duration::from_millis(ms))[11..16]
    )
}

fn waiting(rig: &Rig, session: &str) {
    emit(
        rig,
        "Notification",
        session,
        &rig.root,
        json!({ "notification_type": "permission_prompt" }),
    );
}

#[test]
fn the_box_says_so_once_when_watch_has_been_unreachable_and_once_when_it_is_back() {
    let outage = outage(NOTIFY);
    let (rig, ntfy) = (&outage.rig, &outage.ntfy);
    let start = Instant::now();
    let began = SystemTime::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    daemon.tick(start + secs(300));
    daemon.tick(start + secs(599));
    assert!(ntfy.requests().is_empty());

    // Ten minutes with no heartbeat accepted.
    daemon.tick(start + secs(600));
    let posts = ntfy.requests();
    assert_eq!(posts.len(), 1);
    let post = &posts[0];
    assert_eq!(post.method, "POST");
    assert_eq!(post.path, "/your-private-topic");
    assert_eq!(
        post.header("title"),
        Some("agentbox cannot reach Nucarium Watch")
    );
    assert_eq!(post.header("priority"), Some("high"));
    assert_eq!(
        post.header("x-nucarium-alert-kind"),
        Some("box_watch_unreachable")
    );
    assert!(post
        .header("content-type")
        .unwrap()
        .starts_with("text/plain"));
    // It is the box's own post: no alert of Watch's, and nowhere to tap to.
    assert_eq!(post.header("x-nucarium-alert-id"), None);
    assert_eq!(post.header("click"), None);
    assert_eq!(post.header("authorization"), None);
    let since = [began, began + secs(60)].map(|at| clock(nucleate::envelope::unix_ms(at)));
    assert!(
        since.iter().any(|since| post.body
            == format!(
                "No heartbeat has been accepted since {since}. This box tells you here when a \
                 session waits on you."
            )),
        "{}",
        post.body
    );

    // Once, however long it lasts.
    for later in [601, 900, 3_600, 7_200] {
        daemon.tick(start + secs(later));
    }
    assert_eq!(ntfy.requests().len(), 1);

    // Watch answers again.
    outage.watch_knobs.status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(9_000));
    let posts = ntfy.requests();
    assert_eq!(
        kinds(ntfy),
        ["box_watch_unreachable", "box_watch_reachable"]
    );
    assert_eq!(
        posts[1].header("title"),
        Some("agentbox can reach Nucarium Watch again")
    );
    assert_eq!(posts[1].header("priority"), Some("default"));
    assert_eq!(
        posts[1].body,
        "Heartbeats are accepted again after 3 hours."
    );
    for later in [9_001, 9_100, 12_000] {
        daemon.tick(start + secs(later));
    }
    assert_eq!(ntfy.requests().len(), 2);
    // Nothing about any of it went to Watch, and nothing is left to repeat.
    assert!(outage
        .watch
        .requests()
        .iter()
        .all(|request| [contract::HEARTBEATS, contract::EVENTS].contains(&request.path.as_str())));
    assert!(!rig.home.state_dir().join("notify.json").exists());
}

#[test]
fn while_watch_is_unreachable_each_session_that_starts_waiting_is_told_once() {
    let outage = outage("[notify]\nurl = \"{ntfy}/your-private-topic\"\nafter = \"2m\"\n");
    let (rig, ntfy) = (&outage.rig, &outage.ntfy);
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    // One session starts waiting before the box has said anything.
    waiting(rig, "early");
    daemon.tick(start + secs(60));
    assert!(ntfy.requests().is_empty());

    // The outage is said first, then the session nobody was told about.
    daemon.tick(start + secs(120));
    daemon.tick(start + secs(121));
    assert_eq!(
        kinds(ntfy),
        ["box_watch_unreachable", "box_session_waiting"]
    );
    let post = &ntfy.requests()[1];
    assert_eq!(
        post.header("title"),
        Some("An agent on client-a is waiting for you")
    );
    assert_eq!(post.header("priority"), Some("high"));
    let since = Store::new(&rig.home.state_dir())
        .load("client-a", "early")
        .unwrap()
        .needs_since_ms
        .unwrap();
    assert_eq!(
        post.body,
        format!(
            "A session on client-a (agentbox) has waited since {}. Watch cannot be reached, so \
             this comes from the box.",
            clock(since)
        )
    );

    // Still waiting, and asked again by the agent: nothing more.
    waiting(rig, "early");
    for later in [122, 180, 600] {
        daemon.tick(start + secs(later));
    }
    assert_eq!(ntfy.requests().len(), 2);

    // Another session starts waiting: told at once.
    waiting(rig, "late");
    daemon.tick(start + secs(601));
    assert_eq!(ntfy.requests().len(), 3);
    daemon.tick(start + secs(602));
    assert_eq!(ntfy.requests().len(), 3);

    // The first is answered, works, and waits again: that is a new wait.
    emit(rig, "PostToolUse", "early", &rig.root, json!({}));
    daemon.tick(start + secs(603));
    assert_eq!(ntfy.requests().len(), 3);
    waiting(rig, "early");
    daemon.tick(start + secs(604));
    assert_eq!(ntfy.requests().len(), 4);

    // A session that only works is none of the box's business to announce.
    emit(rig, "UserPromptSubmit", "busy", &rig.root, json!({}));
    daemon.tick(start + secs(605));
    assert_eq!(ntfy.requests().len(), 4);

    // Watch is back: it hears of every wait itself, and the box stops.
    outage.watch_knobs.status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(900));
    waiting(rig, "after");
    daemon.tick(start + secs(901));
    assert_eq!(
        kinds(ntfy),
        [
            "box_watch_unreachable",
            "box_session_waiting",
            "box_session_waiting",
            "box_session_waiting",
            "box_watch_reachable",
        ]
    );
    assert!(events(&outage.watch)
        .iter()
        .any(|event| event["session_id"] == "after"));
}

#[test]
fn a_restart_in_the_middle_of_an_outage_repeats_nothing() {
    let outage = outage("[notify]\nurl = \"{ntfy}/your-private-topic\"\nafter = \"2m\"\n");
    let (rig, ntfy) = (&outage.rig, &outage.ntfy);
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    waiting(rig, "s1");
    daemon.tick(start + secs(120));
    daemon.tick(start + secs(121));
    assert_eq!(ntfy.requests().len(), 2);
    drop(daemon);

    // A new daemon, with Watch still not answering.
    let mut daemon = Daemon::new(rig.settings(), timings());
    for later in [130, 131, 400, 401] {
        daemon.tick(start + secs(later));
    }
    assert_eq!(ntfy.requests().len(), 2);
    // It still knows there is an outage to end.
    outage.watch_knobs.status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(500));
    assert_eq!(kinds(ntfy).last().unwrap(), "box_watch_reachable");
    assert_eq!(ntfy.requests().len(), 3);
}

#[test]
fn a_refused_token_or_a_request_to_slow_down_is_not_an_outage() {
    for answer in [401, 429] {
        let outage = outage("[notify]\nurl = \"{ntfy}/your-private-topic\"\nafter = \"2m\"\n");
        outage.watch_knobs.status.store(answer, Ordering::SeqCst);
        let start = Instant::now();
        let mut daemon = Daemon::new(outage.rig.settings(), timings());
        for later in [0, 60, 120, 121, 600, 3_600] {
            daemon.tick(start + secs(later));
        }
        // Watch answered each time. It is there.
        assert!(outage.ntfy.requests().is_empty(), "after {answer}");
    }
}

#[test]
fn a_post_that_fails_is_tried_again_and_dropped_if_watch_returns_first() {
    let outage = outage("[notify]\nurl = \"{ntfy}/your-private-topic\"\nafter = \"2m\"\n");
    let (rig, ntfy) = (&outage.rig, &outage.ntfy);
    outage.ntfy_knobs.status.store(500, Ordering::SeqCst);
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    daemon.tick(start + secs(120));
    assert_eq!(ntfy.requests().len(), 1);
    // Tried again after a second, then two, as a failed batch of events is.
    daemon.tick(start + secs(120) + Duration::from_millis(900));
    assert_eq!(ntfy.requests().len(), 1);
    daemon.tick(start + secs(121));
    assert_eq!(ntfy.requests().len(), 2);
    daemon.tick(start + secs(122));
    assert_eq!(ntfy.requests().len(), 2);
    daemon.tick(start + secs(123));
    assert_eq!(ntfy.requests().len(), 3);

    // Watch returns before the owner was ever told it was gone. Saying
    // either thing now would be news of nothing.
    outage.watch_knobs.status.store(0, Ordering::SeqCst);
    outage.ntfy_knobs.status.store(0, Ordering::SeqCst);
    for later in [200, 201, 300, 600] {
        daemon.tick(start + secs(later));
    }
    assert_eq!(ntfy.requests().len(), 3);

    // A post that failed and is then delivered is delivered once.
    outage.watch_knobs.status.store(503, Ordering::SeqCst);
    outage.ntfy_knobs.status.store(500, Ordering::SeqCst);
    daemon.tick(start + secs(700));
    daemon.tick(start + secs(900));
    assert_eq!(ntfy.requests().len(), 4);
    outage.ntfy_knobs.status.store(0, Ordering::SeqCst);
    daemon.tick(start + secs(901));
    daemon.tick(start + secs(902));
    daemon.tick(start + secs(1_000));
    assert_eq!(ntfy.requests().len(), 5);
    assert_eq!(kinds(ntfy)[4], "box_watch_unreachable");
}

#[test]
fn the_notifier_is_off_unless_configured_and_silent_on_a_box_that_does_not_report() {
    // No address: an outage is nobody's to announce.
    let silent = outage("");
    let start = Instant::now();
    let mut daemon = Daemon::new(silent.rig.settings(), timings());
    waiting(&silent.rig, "s1");
    for later in [0, 600, 601, 3_600] {
        daemon.tick(start + secs(later));
    }
    assert!(silent.ntfy.requests().is_empty());
    assert!(!silent.rig.home.state_dir().join("notify.json").exists());

    // An address, and a box told to report nowhere: it has no Watch to lose.
    let nowhere = outage("target = \"none\"\n[notify]\nurl = \"{ntfy}/your-private-topic\"\n");
    let mut daemon = Daemon::new(nowhere.rig.settings(), timings());
    waiting(&nowhere.rig, "s1");
    for later in [0, 600, 601, 3_600] {
        daemon.tick(start + secs(later));
    }
    assert!(nowhere.ntfy.requests().is_empty());
    assert!(nowhere.watch.requests().is_empty());
}

#[test]
fn a_post_carries_the_owners_token_for_that_address_and_never_the_boxes() {
    let outage = outage(
        "[notify]\nurl = \"{ntfy}/your-private-topic\"\nafter = \"2m\"\ntoken_file = \"{token}\"\n",
    );
    let (rig, ntfy) = (&outage.rig, &outage.ntfy);
    let token_file = rig.home.dir.join("ntfy-token");
    let config = fs::read_to_string(rig.home.config_file()).unwrap();
    rig.home
        .write_config(&config.replace("{token}", token_file.to_str().unwrap()));
    nucleate::paths::write_private(&token_file, b"tk_owners_own_token\n").unwrap();
    let start = Instant::now();
    let mut daemon = Daemon::new(rig.settings(), timings());
    daemon.tick(start);
    daemon.tick(start + secs(120));
    let posts = ntfy.requests();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].header("authorization"),
        Some("Bearer tk_owners_own_token")
    );
    // The machine token is Watch's. It goes to Watch and nowhere else.
    assert!(!ntfy.everything().contains(TOKEN));
    assert!(!outage.watch.everything().contains("tk_owners_own_token"));

    // A token file others can read is not used, and no post goes without it.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o644)).unwrap();
    waiting(rig, "s1");
    daemon.tick(start + secs(200));
    daemon.tick(start + secs(400));
    assert_eq!(ntfy.requests().len(), 1);
    let report = stdout_json(&run(rig.nucleated().arg("check"), ""));
    let notifier = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "notifier")
        .unwrap();
    assert_eq!(notifier["ok"], false);
}
