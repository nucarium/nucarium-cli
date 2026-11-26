//! The session folders a box reports to a Watch that takes them, and the
//! actions Watch sends back: Push, and Clean up.
//!
//! The daemon's loop, the reporter and the thread that runs actions are each
//! driven here in turn, with the time given, and nothing waits on another.

mod common;

use common::{contract, git, Mock, Reply, Request, Rig};
use nucleate::daemon::{Daemon, Timings};
use nucleate::envelope::rfc3339;
use nucleate::reporter::Reporter;
use nucleate::worktrees;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const ID: &str = "wta_abcdefghijk23456";

/// A Watch that takes worktrees and hands out `actions` in every answer to a
/// heartbeat: until a result for one arrives, as the API does, or for ever
/// when `stubborn`.
fn watch(stubborn: bool) -> (Mock, Arc<Mutex<Vec<Value>>>) {
    let actions = Arc::new(Mutex::new(Vec::<Value>::new()));
    let held = actions.clone();
    let mut mock = Mock::start(move |request: &Request| {
        if contract::violation_taking(request, &["worktrees"]).is_some() {
            return Reply::problem(400, "validation_failed");
        }
        let received = json!("2026-11-19T12:00:00.000Z");
        match request.path.as_str() {
            contract::HEARTBEATS => {
                let finished: Vec<Value> = request.json()["environments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|env| {
                        env["worktree_action_results"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                    })
                    .map(|result| result["id"].clone())
                    .collect();
                let mut held = held.lock().unwrap();
                if !stubborn {
                    held.retain(|action| !finished.contains(&action["id"]));
                }
                Reply::json(
                    200,
                    json!({
                        "received_at": received, "heartbeat_interval_s": 60,
                        "accepts": ["worktrees"], "worktree_actions": *held,
                    }),
                )
            }
            _ => Reply::json(
                200,
                json!({ "received_at": received, "accepted": 1, "duplicates": 0, "rejected": [] }),
            ),
        }
    });
    mock.ahead = &["worktrees"];
    (mock, actions)
}

fn action(folder: &str, kind: &str, discard: bool) -> Value {
    json!({
        "id": ID, "env_id": "client-a", "folder": folder, "kind": kind,
        "discard": discard, "requested_at": rfc3339(SystemTime::now()),
    })
}

/// The daemon's loop, a minute further on at each beat, so that every beat
/// sends a heartbeat.
struct Beats {
    daemon: Daemon,
    clock: Instant,
}

impl Beats {
    fn new(rig: &Rig) -> Beats {
        let daemon = Daemon::new(
            rig.settings(),
            Timings {
                jitter: false,
                http_timeout: Duration::from_secs(2),
                ..Timings::default()
            },
        );
        Beats {
            daemon,
            clock: Instant::now(),
        }
    }

    fn beat(&mut self) {
        self.clock += Duration::from_secs(61);
        self.daemon.tick(self.clock);
    }
}

fn heartbeats(mock: &Mock) -> Vec<Value> {
    assert_eq!(contract::violations(mock), Vec::<String>::new());
    mock.to(contract::HEARTBEATS)
        .iter()
        .map(Request::json)
        .collect()
}

/// Every result a heartbeat carried, in order.
fn results(mock: &Mock) -> Vec<Value> {
    heartbeats(mock)
        .iter()
        .flat_map(|beat| {
            beat["environments"][0]["worktree_action_results"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

/// A box whose owner has turned worktree actions on.
fn rig() -> Rig {
    let rig = Rig::new();
    rig.configure("worktree_actions = true\n");
    rig
}

fn count(rig: &Rig) {
    Reporter::default().tick(&rig.settings(), SystemTime::now());
}

fn run_actions(rig: &Rig) -> usize {
    worktrees::run_queued(&rig.settings())
}

fn commit(worktree: &Path, file: &str) {
    fs::write(worktree.join(file), "work\n").unwrap();
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-q", "-m", "work"]);
}

/// Ask for one action, run it, and return the one result the next heartbeat
/// carries.
fn act(rig: &Rig, action: Value) -> Value {
    let (mock, actions) = watch(false);
    rig.sign_in(&mock.url);
    actions.lock().unwrap().push(action);
    let mut beats = Beats::new(rig);
    beats.beat();
    assert_eq!(run_actions(rig), 1);
    beats.beat();
    let carried = results(&mock);
    assert_eq!(carried.len(), 1, "{carried:?}");
    carried[0].clone()
}

#[test]
fn a_watch_that_takes_worktrees_is_sent_them_and_one_that_does_not_is_not() {
    // Folders are reported whether or not actions are on.
    let rig = Rig::new();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    count(&rig);

    let plain = common::gate::watch();
    rig.sign_in(&plain.url);
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    assert!(heartbeats(&plain)
        .iter()
        .all(|beat| beat["environments"][0].get("worktrees").is_none()));
    assert_eq!(
        beats.daemon.state(true).heartbeat.withheld,
        ["agent", "worktrees"]
    );

    let taking = common::gate::watch_taking(&["worktrees"]);
    rig.sign_in(&taking.url);
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    let said = heartbeats(&taking).pop().unwrap();
    let folders = &said["environments"][0]["worktrees"];
    let last_change = folders[0]["last_change_at"].clone();
    assert!(last_change.is_string(), "{folders}");
    // Whether a process works in it is read from /proc. Where there is none,
    // the folder might be in use, and is said to be.
    assert_eq!(
        *folders,
        json!([{
            "folder": "fix-login",
            "in_use": !cfg!(target_os = "linux"),
            "last_change_at": last_change,
            "repos": [{
                "repo": "web", "branch": "workspace/fix-login",
                "uncommitted": false, "unpushed_commits": 1,
            }],
        }])
    );
    assert_eq!(said["environments"][0]["worktree_actions_allowed"], false);
    common::gate::only_listed_fields(&taking);

    // A box whose owner turned actions on says so.
    rig.configure("worktree_actions = true\n");
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    let said = heartbeats(&taking).pop().unwrap();
    assert_eq!(said["environments"][0]["worktree_actions_allowed"], true);
}

#[test]
fn with_actions_off_a_push_and_a_clean_up_are_refused_and_nothing_changes() {
    let rig = Rig::new();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    fs::write(worktree.join("notes.txt"), "only here\n").unwrap();
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    for (id, kind, discard) in [
        ("wta_0000000000000001", "push", false),
        ("wta_0000000000000002", "clean_up", true),
    ] {
        let mut asked = action("fix-login", kind, discard);
        asked["id"] = json!(id);
        let result = act(&rig, asked);
        assert_eq!(result["outcome"], "refused", "{kind}");
        assert_eq!(result["detail"], "Actions are off on this box.", "{kind}");
    }
    assert!(worktree.join("notes.txt").is_file());
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
    assert!(git(&rig.upstream, &["branch", "--list", "workspace/*"]).is_empty());
    assert!(git(&rig.upstream, &["for-each-ref", "refs/wip"]).is_empty());
}

#[test]
fn push_pushes_the_branch_and_says_so_once() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    commit(&worktree, "b.txt");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let (mock, actions) = watch(false);
    rig.sign_in(&mock.url);
    actions
        .lock()
        .unwrap()
        .push(action("fix-login", "push", false));
    let mut beats = Beats::new(&rig);
    beats.beat();
    assert_eq!(run_actions(&rig), 1);
    assert_eq!(
        git(&rig.upstream, &["rev-parse", "workspace/fix-login"]),
        git(&worktree, &["rev-parse", "HEAD"])
    );
    beats.beat();
    beats.beat();
    let carried = results(&mock);
    assert_eq!(carried.len(), 1, "a result is sent until Watch has it");
    assert_eq!(carried[0]["id"], ID);
    assert_eq!(carried[0]["outcome"], "done");
    assert_eq!(
        carried[0]["detail"],
        "Pushed 2 commits in web. web has changes that are not committed."
    );
    // The next report says what the folder holds now.
    count(&rig);
    beats.beat();
    let said = heartbeats(&mock).pop().unwrap();
    assert_eq!(
        said["environments"][0]["worktrees"][0]["repos"][0]["unpushed_commits"],
        0
    );
}

#[test]
fn push_with_nothing_to_push_is_refused() {
    let rig = rig();
    rig.session("fix-login");
    let result = act(&rig, action("fix-login", "push", false));
    assert_eq!(result["outcome"], "refused");
    assert_eq!(result["detail"], "There is nothing to push.");
}

#[test]
fn the_same_action_given_twice_runs_once() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    let (mock, actions) = watch(true);
    rig.sign_in(&mock.url);
    actions
        .lock()
        .unwrap()
        .push(action("fix-login", "push", false));
    let mut beats = Beats::new(&rig);
    beats.beat();
    assert_eq!(run_actions(&rig), 1);
    // Watch keeps sending it, before and after the result has gone.
    beats.beat();
    assert_eq!(run_actions(&rig), 0);
    beats.beat();
    assert_eq!(run_actions(&rig), 0);
    let carried = results(&mock);
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert_eq!(carried[0]["outcome"], "done");
}

#[test]
fn an_action_older_than_watch_waits_for_is_not_run() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    let (mock, actions) = watch(false);
    rig.sign_in(&mock.url);
    let mut old = action("fix-login", "push", false);
    old["requested_at"] = json!(rfc3339(SystemTime::now() - Duration::from_secs(12 * 60)));
    actions.lock().unwrap().push(old);
    let mut beats = Beats::new(&rig);
    beats.beat();
    assert_eq!(run_actions(&rig), 0);
    beats.beat();
    assert!(results(&mock).is_empty());
    assert!(git(&rig.upstream, &["branch", "--list", "workspace/*"]).is_empty());
}

#[test]
fn clean_up_of_a_folder_in_use_is_refused() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    let hook = rig.hook(
        "UserPromptSubmit",
        &json!({ "session_id": "s1", "cwd": worktree }),
    );
    assert!(hook.status.success());
    let result = act(&rig, action("fix-login", "clean_up", true));
    assert_eq!(result["outcome"], "refused");
    assert_eq!(result["detail"], "A session is still using it.");
    assert!(worktree.is_dir());
}

#[test]
fn a_branch_named_like_an_option_is_never_pushed() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    git(&worktree, &["update-ref", "refs/heads/--mirror", "HEAD"]);
    git(&worktree, &["symbolic-ref", "HEAD", "refs/heads/--mirror"]);
    let before = git(&rig.upstream, &["for-each-ref", "--format=%(refname)"]);
    let result = act(&rig, action("fix-login", "push", false));
    assert_eq!(result["outcome"], "failed");
    assert_eq!(
        result["detail"],
        "web was not pushed: its branch has a name that cannot be pushed safely."
    );
    assert_eq!(
        git(&rig.upstream, &["for-each-ref", "--format=%(refname)"]),
        before
    );
}

#[test]
fn an_action_the_box_stopped_in_the_middle_of_is_not_run_again() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    let (mock, actions) = watch(false);
    rig.sign_in(&mock.url);
    actions
        .lock()
        .unwrap()
        .push(action("fix-login", "push", false));
    let mut beats = Beats::new(&rig);
    beats.beat();
    // Its start was written down, and then the box stopped.
    let started = rig.home.state_dir().join("worktree-actions/started.json");
    fs::write(&started, json!({ ID: 1 }).to_string()).unwrap();
    assert_eq!(run_actions(&rig), 0);
    beats.beat();
    let carried = results(&mock);
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert_eq!(carried[0]["outcome"], "failed");
    assert_eq!(
        carried[0]["detail"],
        "The box stopped while it ran, and it was not run again."
    );
    assert!(git(&rig.upstream, &["branch", "--list", "workspace/*"]).is_empty());
}

#[test]
fn nothing_runs_while_the_record_of_started_actions_cannot_be_read_and_watch_hears_why() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt");
    let (mock, actions) = watch(false);
    rig.sign_in(&mock.url);
    actions
        .lock()
        .unwrap()
        .push(action("fix-login", "push", false));
    let mut beats = Beats::new(&rig);
    beats.beat();
    let started = rig.home.state_dir().join("worktree-actions/started.json");
    fs::write(&started, "{ not json").unwrap();
    assert_eq!(run_actions(&rig), 0);
    assert!(git(&rig.upstream, &["branch", "--list", "workspace/*"]).is_empty());
    beats.beat();
    let carried = results(&mock);
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert_eq!(carried[0]["outcome"], "refused");
    assert_eq!(
        carried[0]["detail"],
        "The box cannot read its record of earlier actions."
    );
}

#[test]
fn results_are_sent_again_after_a_heartbeat_watch_did_not_take() {
    let rig = rig();
    rig.session("fix-login");
    let (mock, actions) = watch(false);
    rig.sign_in(&mock.url);
    actions
        .lock()
        .unwrap()
        .push(action("fix-login", "push", false));
    let mut beats = Beats::new(&rig);
    beats.beat();
    assert_eq!(run_actions(&rig), 1);
    // Watch takes worktrees, and then is busy and says to come back later.
    let busy = Mock::start(|request: &Request| match request.nth {
        0 => Reply::json(
            200,
            json!({
                "received_at": "2026-11-19T12:00:00.000Z", "heartbeat_interval_s": 60,
                "accepts": ["worktrees"], "worktree_actions": [],
            }),
        ),
        _ => Reply::problem(503, "unavailable").header("Retry-After", "1"),
    });
    rig.sign_in(&busy.url);
    let mut busy_beats = Beats::new(&rig);
    busy_beats.beat();
    busy_beats.beat();
    let refused = busy.to(contract::HEARTBEATS);
    let carried =
        refused.last().unwrap().json()["environments"][0]["worktree_action_results"].clone();
    assert_eq!(carried[0]["id"], ID, "the refused heartbeat carried it");
    // Back to the Watch that takes them: the result is still there to send.
    rig.sign_in(&mock.url);
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    let carried = results(&mock);
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert_eq!(carried[0]["id"], ID);
}

#[test]
fn a_subagents_worktree_is_reported_as_part_of_its_repo() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    let payload = json!({ "name": "helper", "cwd": worktree }).to_string();
    let out = common::run(rig.nucleate().arg("hook-create"), &payload);
    assert!(out.status.success(), "{}", common::stderr(&out));
    let sibling = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    fs::write(sibling.join("only-here.txt"), "only here\n").unwrap();
    count(&rig);
    let taking = common::gate::watch_taking(&["worktrees"]);
    rig.sign_in(&taking.url);
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    let said = heartbeats(&taking).pop().unwrap();
    let repos = &said["environments"][0]["worktrees"][0]["repos"];
    assert_eq!(repos.as_array().unwrap().len(), 1, "{repos}");
    assert_eq!(repos[0]["repo"], "web");
    assert_eq!(repos[0]["uncommitted"], true);
}

#[test]
fn a_branch_with_a_space_javascript_sees_is_sent_blank() {
    let rig = rig();
    let worktree = rig.session("fix-login");
    git(&worktree, &["checkout", "-q", "-b", "agent/a\u{FEFF}b"]);
    count(&rig);
    let taking = common::gate::watch_taking(&["worktrees"]);
    rig.sign_in(&taking.url);
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    // `heartbeats` holds every one to the contract as the API does.
    let said = heartbeats(&taking).pop().unwrap();
    assert_eq!(
        said["environments"][0]["worktrees"][0]["repos"][0]["branch"],
        ""
    );
}

#[test]
fn a_file_at_a_folders_root_shows_the_folder_as_not_saved() {
    let rig = rig();
    rig.session("fix-login");
    fs::write(
        rig.root.join(".worktrees/fix-login/NOTES.md"),
        "only here\n",
    )
    .unwrap();
    count(&rig);
    let taking = common::gate::watch_taking(&["worktrees"]);
    rig.sign_in(&taking.url);
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    let said = heartbeats(&taking).pop().unwrap();
    let repos = &said["environments"][0]["worktrees"][0]["repos"];
    assert_eq!(
        repos[1],
        json!({
            "repo": "fix-login", "branch": "",
            "uncommitted": true, "unpushed_commits": 0,
        }),
        "{repos}"
    );
}

#[test]
fn a_result_the_box_cannot_read_is_sent_as_lost_and_then_forgotten() {
    let rig = rig();
    let (mock, _) = watch(false);
    rig.sign_in(&mock.url);
    let results_dir = rig
        .home
        .state_dir()
        .join("worktree-actions/results/client-a");
    fs::create_dir_all(&results_dir).unwrap();
    fs::write(results_dir.join(format!("{ID}.json")), "{ half a resu").unwrap();
    fs::write(results_dir.join("stray.json"), "{}").unwrap();
    let mut beats = Beats::new(&rig);
    beats.beat();
    beats.beat();
    beats.beat();
    let carried = results(&mock);
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert_eq!(carried[0]["id"], ID);
    assert_eq!(carried[0]["outcome"], "failed");
    assert_eq!(carried[0]["detail"], "The box lost this action's result.");
    assert_eq!(fs::read_dir(&results_dir).unwrap().count(), 0);
}

/// Whether a process works in a folder is read from /proc, so a folder can
/// only be shown cleaned up where there is one.
#[cfg(target_os = "linux")]
mod clean_up {
    use super::*;

    fn folder(rig: &Rig, name: &str) -> PathBuf {
        rig.root.join(".worktrees").join(name)
    }

    #[test]
    fn a_saved_folder_is_removed() {
        let rig = rig();
        rig.session("fix-login");
        let result = act(&rig, action("fix-login", "clean_up", false));
        assert_eq!(result["outcome"], "done");
        assert_eq!(result["detail"], "Cleaned up.");
        assert!(!folder(&rig, "fix-login").exists());
        // Asked again, it is already done.
        let again = act(
            &rig,
            json!({
                "id": "wta_0000000000000002", "env_id": "client-a", "folder": "fix-login",
                "kind": "clean_up", "discard": false, "requested_at": rfc3339(SystemTime::now()),
            }),
        );
        assert_eq!(again["outcome"], "done");
    }

    #[test]
    fn unsaved_work_without_discard_is_refused_and_nothing_is_removed() {
        let rig = rig();
        let worktree = rig.session("fix-login");
        commit(&worktree, "a.txt");
        commit(&worktree, "b.txt");
        fs::write(worktree.join("notes.txt"), "only here\n").unwrap();
        let result = act(&rig, action("fix-login", "clean_up", false));
        assert_eq!(result["outcome"], "refused");
        assert_eq!(
            result["detail"],
            "It holds 2 commits not on GitHub in web and changes not committed in web."
        );
        assert!(worktree.join("notes.txt").is_file());
        assert!(git(&rig.upstream, &["for-each-ref", "refs/wip"]).is_empty());
    }

    #[test]
    fn discard_pushes_a_backup_first_and_then_removes() {
        let rig = rig();
        let worktree = rig.session("fix-login");
        commit(&worktree, "a.txt");
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        fs::write(worktree.join("notes.txt"), "only here\n").unwrap();
        let result = act(&rig, action("fix-login", "clean_up", true));
        assert_eq!(result["outcome"], "done", "{result}");
        // A ref of the action's own, which nothing else writes.
        let backup = format!("refs/wip/workspace-fix-login-{ID}");
        assert_eq!(result["detail"], format!("Backed up to {backup} in web."));
        assert_eq!(
            git(&rig.upstream, &["show", &format!("{backup}:notes.txt")]),
            "only here"
        );
        assert_eq!(
            git(&rig.upstream, &["rev-parse", &format!("{backup}^")]),
            head
        );
        // On a ref, not a branch.
        assert!(git(&rig.upstream, &["branch", "--list", "workspace/*"]).is_empty());
        assert!(!folder(&rig, "fix-login").exists());
    }

    #[test]
    fn a_backup_that_fails_removes_nothing() {
        let rig = rig();
        let worktree = rig.session("fix-login");
        commit(&worktree, "a.txt");
        let gone = rig.home.dir.join("no-such-remote");
        git(
            &rig.bare(),
            &["remote", "set-url", "origin", gone.to_str().unwrap()],
        );
        let result = act(&rig, action("fix-login", "clean_up", true));
        assert_eq!(result["outcome"], "failed");
        assert_eq!(
            result["detail"],
            "The backup of web could not be pushed, so nothing was removed."
        );
        assert!(worktree.join("a.txt").is_file());
    }

    #[test]
    fn a_folder_cleaned_up_leaves_the_next_report() {
        let rig = rig();
        rig.session("fix-login");
        rig.session("other");
        count(&rig);
        act(&rig, action("fix-login", "clean_up", false));
        count(&rig);
        let taking = common::gate::watch_taking(&["worktrees"]);
        rig.sign_in(&taking.url);
        let mut beats = Beats::new(&rig);
        beats.beat();
        beats.beat();
        let said = heartbeats(&taking).pop().unwrap();
        let names: Vec<&Value> = said["environments"][0]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|folder| &folder["folder"])
            .collect();
        assert_eq!(names, [&json!("other")]);
    }

    #[test]
    fn discard_backs_up_a_subagents_worktree_too() {
        let rig = rig();
        let worktree = rig.session("fix-login");
        let payload = json!({ "name": "helper", "cwd": worktree }).to_string();
        let out = common::run(rig.nucleate().arg("hook-create"), &payload);
        assert!(out.status.success(), "{}", common::stderr(&out));
        let sibling = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        fs::write(sibling.join("only-here.txt"), "only here\n").unwrap();
        let result = act(&rig, action("fix-login", "clean_up", true));
        assert_eq!(result["outcome"], "done", "{result}");
        let backup = format!("refs/wip/workspace-helper-{ID}");
        assert_eq!(result["detail"], format!("Backed up to {backup} in web."));
        assert_eq!(
            git(&rig.upstream, &["show", &format!("{backup}:only-here.txt")]),
            "only here"
        );
        assert!(!folder(&rig, "fix-login").exists());
        let listed = git(&rig.bare(), &["worktree", "list"]);
        assert!(!listed.contains("helper"), "{listed}");
    }

    #[test]
    fn a_repository_nested_in_a_worktree_is_not_discarded() {
        let rig = rig();
        let worktree = rig.session("fix-login");
        let nested = worktree.join("vendor-copy");
        fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q", "-b", "main"]);
        fs::write(nested.join("own.txt"), "its own\n").unwrap();
        git(&nested, &["add", "."]);
        git(&nested, &["commit", "-q", "-m", "own"]);
        let result = act(&rig, action("fix-login", "clean_up", true));
        assert_eq!(result["outcome"], "failed", "{result}");
        assert_eq!(
            result["detail"],
            "web holds another git repository that cannot be backed up, so nothing was removed."
        );
        assert!(nested.join("own.txt").is_file());
        assert!(git(&rig.upstream, &["for-each-ref", "refs/wip"]).is_empty());
    }

    #[test]
    fn a_file_at_a_folders_root_is_never_cleaned_up() {
        let rig = rig();
        rig.session("fix-login");
        let notes = folder(&rig, "fix-login").join("NOTES.md");
        fs::write(&notes, "only here\n").unwrap();
        let result = act(&rig, action("fix-login", "clean_up", false));
        assert_eq!(result["outcome"], "refused");
        assert_eq!(result["detail"], "It holds files outside its repos.");
        let result = act(
            &rig,
            json!({
                "id": "wta_0000000000000003", "env_id": "client-a", "folder": "fix-login",
                "kind": "clean_up", "discard": true, "requested_at": rfc3339(SystemTime::now()),
            }),
        );
        assert_eq!(result["outcome"], "failed");
        assert_eq!(
            result["detail"],
            "It holds files outside its repos, which cannot be backed up, so nothing was removed."
        );
        assert!(notes.is_file());
    }

    #[test]
    fn a_detail_that_would_be_too_long_for_watch_is_said_shorter() {
        let rig = rig();
        let worktree = rig.session("fix-login");
        let payload = json!({ "name": "helper", "cwd": worktree }).to_string();
        let out = common::run(rig.nucleate().arg("hook-create"), &payload);
        assert!(out.status.success(), "{}", common::stderr(&out));
        let sibling = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        let smiles = "\u{1F600}".repeat(55);
        for (place, branch) in [
            (&worktree, format!("workspace/fix-login{smiles}")),
            (&sibling, format!("workspace/helper{smiles}")),
        ] {
            git(place, &["branch", "-q", "-m", &branch]);
            fs::write(place.join("only-here.txt"), "only here\n").unwrap();
        }
        // `act` holds the heartbeat that carries the result to the contract.
        let result = act(&rig, action("fix-login", "clean_up", true));
        assert_eq!(result["outcome"], "done", "{result}");
        assert_eq!(result["detail"], "Backed up 2 repos to GitHub.");
        let backups = git(
            &rig.upstream,
            &["for-each-ref", "--format=%(refname)", "refs/wip"],
        );
        assert_eq!(backups.lines().count(), 2, "{backups}");
        assert!(!folder(&rig, "fix-login").exists());
    }
}
