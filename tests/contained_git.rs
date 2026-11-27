//! Git for an environment in a container runs inside its container, never on
//! the host (D15).
//!
//! On the host, `git` here is a stand-in that writes down that it was run
//! and fails. `podman` is a stand-in that runs the box's own `nucleated
//! repos` on this machine, as it would run in the container, with the real
//! git and a local remote. What the daemon learns of the container's
//! repositories it learns from that answer: the counts of unsaved work, the
//! session folders it reports, what a session left behind, and what cleanup
//! would remove. What it changes in them is changed in there too: a refresh,
//! a snapshot, Watch's Push and Clean up, and cleanup's removal. And when the
//! answer is not one, it knows nothing, says nothing in its place, and
//! removes nothing.

mod common;

use common::gate::{self, mark, MARK};
use common::{contract, git, run, stderr, stdout_json, Mock, Reply, Request, Rig};
use nucleate::driver::{END_GRACE, END_LIMIT};
use nucleate::envelope::{unix_ms, Draft, Envelope, EventType};
use nucleate::reporter::{Counts, Reporter};
use nucleate::sessions::Store;
use nucleate::spool::{Record, Spool};
use nucleate::workspace::{Unsaved, Workspace};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// A box with `client-a` in its container, and a `PATH` for the host whose
/// `git` writes down that it ran and fails.
struct Boxed {
    rig: Rig,
    /// The host's `PATH`: the stand-in `git`, then the stand-in `podman`,
    /// then the test's own.
    path: OsString,
    /// Where the stand-in `git` writes each command line it was given.
    host_git: PathBuf,
}

fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let fresh = path.with_extension("new");
    fs::write(&fresh, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&fresh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&fresh, path).unwrap();
}

fn boxed() -> Boxed {
    boxed_in(Rig::new())
}

fn boxed_in(rig: Rig) -> Boxed {
    rig.configure_in_container("");
    let real = std::env::var("PATH").unwrap_or_default();
    let bin = rig.home.dir.join("host-bin");
    let host_git = rig.home.dir.join("host-git.log");
    script(
        &bin.join("git"),
        &format!("echo \"$*\" >> '{}'\nexit 1", host_git.display()),
    );
    // The container has its own git: the real one.
    let podman = rig.looker().podman;
    script(
        &bin.join("podman"),
        &format!(
            "PATH='{real}'\nexport PATH\nexec '{}' \"$@\"",
            podman.display()
        ),
    );
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&real));
    let path = std::env::join_paths(paths).unwrap();
    Boxed {
        rig,
        path,
        host_git,
    }
}

impl Boxed {
    /// Every command line git was given on the host.
    fn host_git(&self) -> Vec<String> {
        fs::read_to_string(&self.host_git)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The reporter as the daemon has it: git for the container through the
    /// stand-in `podman`, inside it.
    fn reporter(&self) -> Reporter {
        Reporter::default().running_with(self.rig.looker())
    }

    fn counts(&self) -> Counts {
        self.reporter()
            .count(&self.rig.settings(), SystemTime::now())
    }

    /// A housekeeping job, run as the box runs one, with the host's `PATH`.
    fn job(&self, job: &str, more: &[&str], github: Option<&Mock>) -> std::process::Output {
        self.rig.looker();
        let mut command = self.rig.nucleated();
        command.env("PATH", &self.path);
        if let Some(github) = github {
            command.env("NUCLEATE_GITHUB_API_URL", &github.url);
        }
        run(command.args(["housekeeping", "run", job]).args(more), "")
    }
}

/// Whether the host asked inside `client-a`'s container to `op`.
fn asked_inside(rig: &Rig, op: &str) -> bool {
    rig.podman_calls().iter().any(|call| {
        call.starts_with("exec agent-env-client-a ")
            && call.contains(" repos --env ")
            && call.contains(&format!(" --do {op} "))
    })
}

/// Ask for `client-a`'s session `s1` to be reported on, as the daemon's
/// loop asks once it stops, long enough ago that it is due.
fn ask_for_outcome(rig: &Rig) {
    nucleate::reporter::ask(
        &rig.home.state_dir(),
        "client-a",
        "s1",
        SystemTime::now() - Duration::from_secs(60),
    );
}

/// How many censuses the host has asked of the container.
fn censuses(rig: &Rig) -> usize {
    rig.podman_calls()
        .iter()
        .filter(|call| call.contains(" --do census "))
        .count()
}

fn outcomes_spooled(rig: &Rig) -> usize {
    rig.spooled()
        .iter()
        .filter(|record| record["event"]["type"] == "outcome")
        .count()
}

fn commit(worktree: &Path, file: &str, subject: &str) {
    fs::write(worktree.join(file), "work\n").unwrap();
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-q", "-m", subject]);
}

/// A session in `client-a` that worked in the session folder `workspace`
/// and has stopped, as the daemon has it once it applied the hook's records.
fn stopped_in(rig: &Rig, workspace: &str) -> nucleate::sessions::Session {
    let spool = Spool::new(&rig.container_spool());
    for (n, kind) in [EventType::UserPromptSubmit, EventType::Stop]
        .into_iter()
        .enumerate()
    {
        let at = SystemTime::now() - Duration::from_secs(60) + Duration::from_millis(n as u64);
        let draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            ..Draft::default()
        };
        spool
            .write(&Record {
                event: Envelope::new(kind, draft, at).unwrap(),
                at_ms: unix_ms(at),
                workspace: Some(workspace.into()),
                process: None,
            })
            .unwrap();
    }
    gate::tick(rig);
    Store::new(&rig.home.state_dir())
        .load("client-a", "s1")
        .unwrap()
}

fn outcome(boxed: &Boxed, session: &nucleate::sessions::Session) -> Option<Draft> {
    let settings = boxed.rig.settings();
    let environment = settings.environment("client-a").unwrap();
    boxed.reporter().outcome(&settings, environment, session)
}

/// Three session folders: one with a file not committed, one with a commit
/// on no remote, one pushed and clean.
fn three_folders(rig: &Rig) {
    let dirty = rig.session("dirty");
    fs::write(dirty.join("notes.txt"), "not committed\n").unwrap();
    let ahead = rig.session("ahead");
    commit(&ahead, "a.txt", "work");
    let pushed = rig.session("pushed");
    commit(&pushed, "b.txt", "pushed work");
    rig.push(&pushed);
}

fn folder<'a>(folders: &'a [Value], name: &str) -> &'a Value {
    folders
        .iter()
        .find(|folder| folder["folder"] == name)
        .unwrap_or_else(|| panic!("{name} is not in {folders:?}"))
}

#[test]
fn the_daemon_counts_reports_and_tells_an_outcome_from_inside_and_runs_no_git_on_the_host() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    three_folders(rig);
    let session = stopped_in(rig, "dirty");
    assert_eq!(session.workspace.as_deref(), Some("dirty"));
    let counts = rig.home.state_dir().join("unsaved.json");
    let told = || {
        mock.to(contract::EVENTS)
            .iter()
            .flat_map(|request| {
                request.json()["events"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .find(|event| event["type"] == "outcome")
    };
    let out = gate::daemon_until(rig, Some(&boxed.path), "a count and an outcome", &|| {
        counts.is_file() && told().is_some()
    });
    assert_eq!(boxed.host_git(), Vec::<String>::new(), "{}", stderr(&out));

    let counts: Value = serde_json::from_str(&fs::read_to_string(&counts).unwrap()).unwrap();
    let nucleus = &counts["nuclei"][0];
    assert_eq!(nucleus["environments"], json!(["client-a"]));
    assert_eq!(
        nucleus["unsaved"],
        json!({ "uncommitted_repos": 1, "unpushed_commits": 1 })
    );
    let folders = nucleus["worktrees"].as_array().unwrap();
    assert_eq!(folders.len(), 3, "{folders:?}");
    let repo = |name: &str| folder(folders, name)["repos"][0].clone();
    assert_eq!(repo("dirty")["uncommitted"], true);
    assert_eq!(repo("dirty")["unpushed_commits"], 0);
    assert_eq!(repo("ahead")["unpushed_commits"], 1);
    assert_eq!(repo("ahead")["branch"], "workspace/ahead");
    assert_eq!(repo("pushed")["uncommitted"], false);
    assert_eq!(repo("pushed")["unpushed_commits"], 0);
    assert_eq!(repo("pushed")["repo"], "web");

    let event = told().unwrap();
    assert_eq!(event["branch"], "workspace/dirty");
    assert_eq!(event["repos"], json!(["web"]));
    assert_eq!(event["commits"], 0);
    assert_eq!(event["uncommitted_repos"], 1);
    assert_eq!(event["unpushed_commits"], 0);
}

#[test]
fn an_outcome_of_a_folder_that_is_gone_says_nothing_was_left() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.session("done");
    let session = stopped_in(rig, "done");
    fs::remove_dir_all(rig.root.join(".worktrees").join("done")).unwrap();
    let told = outcome(&boxed, &session).unwrap();
    assert_eq!(told.uncommitted_repos, Some(0));
    assert_eq!(told.unpushed_commits, Some(0));
}

/// A worktree of the nucleus's `web` repo at `path`, holding a file that is
/// not committed.
fn worktree_at(rig: &Rig, path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git(
        &rig.bare(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "elsewhere",
            path.to_str().unwrap(),
            "origin/main",
        ],
    );
    fs::write(path.join("notes.txt"), "not committed\n").unwrap();
}

#[test]
fn a_census_tells_of_a_worktree_outside_the_environments_folders_only_that_it_left_one_out() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let inside = rig.session("fix-login");
    fs::write(inside.join("notes.txt"), "not committed\n").unwrap();
    let census = || {
        rig.looker();
        let out = run(
            rig.nucleated()
                .env(
                    "NUCLEATED_CONFIG",
                    rig.home.config_file().with_file_name("inside.toml"),
                )
                .args(["repos", "--env", "client-a", "--do", "census"]),
            "",
        );
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        stdout_json(&out)
    };
    let all = census();
    assert_eq!(all["version"], 1);
    assert_eq!(all["condition"], "ok");
    assert_eq!(all["complete"], true);
    assert!(all.get("left_out").is_none(), "{all}");
    assert_eq!(all["folders"][0]["folder"], "fix-login");
    assert_eq!(
        all["folders"][0]["worktrees"][0]["facts"]["uncommitted"],
        true
    );

    worktree_at(rig, &rig.home.dir.join("workspace-other").join("wt"));
    let some = census();
    assert_eq!(some["left_out"], true, "{some}");
    assert!(some.get("loose").is_none(), "{some}");
    assert_eq!(some["folders"][0]["folder"], "fix-login");
    // And the host knows no count.
    assert_eq!(boxed.counts().nucleus("client-a").unwrap().unsaved, None);
}

/// GitHub as it answers for a branch whose commits were squash-merged as
/// pull request #7.
fn github(commits: Vec<String>) -> Mock {
    Mock::start(move |request: &Request| {
        let path = request.path.split('?').next().unwrap_or_default();
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["repos", "octo", "web", "commits", _, "pulls"] => Reply::json(
                200,
                json!([{
                    "number": 7,
                    "html_url": "https://github.com/octo/web/pull/7",
                    "state": "closed",
                    "merged_at": "2026-11-20T12:00:00Z",
                    "head": { "sha": commits.last().unwrap() },
                }]),
            ),
            ["repos", "octo", "web", "pulls", "7", "commits"] => Reply::json(
                200,
                Value::Array(commits.iter().map(|sha| json!({ "sha": sha })).collect()),
            ),
            _ => Reply::json(404, json!({ "message": "Not Found" })),
        }
    })
}

/// A session whose branch was pushed, squash-merged as pull request #7 and
/// deleted on the remote. Returns its commits.
fn merged_session(rig: &Rig, name: &str) -> Vec<String> {
    let worktree = rig.session(name);
    commit(&worktree, "change.txt", "change");
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    rig.push(&worktree);
    let branch = git(&worktree, &["branch", "--show-current"]);
    git(&rig.upstream, &["merge", "-q", "--squash", &branch]);
    git(&rig.upstream, &["commit", "-q", "-m", "change (#7)"]);
    git(&rig.upstream, &["branch", "-q", "-D", &branch]);
    rig.home
        .git(&rig.bare(), &["fetch", "-q", "--prune", "origin"]);
    vec![head]
}

#[cfg(target_os = "linux")]
fn verdict(report: &Value, name: &str) -> Value {
    report["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["session"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} is not in {report}"))
}

/// Whether a process is in a folder is read from `/proc`, so what cleanup
/// would remove can only be shown where there is one.
#[cfg(target_os = "linux")]
#[test]
fn cleanup_decides_from_inside_and_github() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.as_github("octo/web");
    let commits = merged_session(rig, "merged");
    three_folders(rig);
    let github = github(commits);
    rig.configure_in_container("[housekeeping]\ncleanup_min_age = \"0s\"\n");

    let out = boxed.job("cleanup", &["--dry-run"], Some(&github));
    assert!(out.status.success(), "{}", stderr(&out));
    let report = &stdout_json(&out)[0];
    assert_eq!(
        verdict(report, "dirty"),
        json!({ "session": "dirty", "action": "kept", "reason": "web: uncommitted changes" })
    );
    assert_eq!(
        verdict(report, "ahead")["reason"],
        "web: HEAD is not on any remote branch or merged pull request"
    );
    assert_eq!(verdict(report, "pushed")["action"], "would_remove");
    assert_eq!(verdict(report, "merged")["action"], "would_remove");
    // GitHub was asked from the host, about the commit the container named.
    assert!(!github.requests().is_empty());

    // Merged work goes at once; work that was only pushed waits out the age.
    rig.configure_in_container("");
    let out = boxed.job("cleanup", &["--dry-run"], Some(&github));
    let report = &stdout_json(&out)[0];
    assert_eq!(verdict(report, "merged")["action"], "would_remove");
    assert_eq!(
        verdict(report, "pushed")["reason"],
        "it changed too recently"
    );

    // Run for real, the container removes what the host decided could go.
    let out = boxed.job("cleanup", &[], Some(&github));
    let report = &stdout_json(&out)[0];
    assert_eq!(verdict(report, "merged")["action"], "removed");
    assert!(!rig.root.join(".worktrees").join("merged").exists());
    assert!(rig.root.join(".worktrees").join("pushed").is_dir());
    assert_eq!(boxed.host_git(), Vec::<String>::new());
}

// ---- what the host takes from the container --------------------------------

/// What the reporter learned of `client-a` when the container answered with
/// whatever `does` makes it print, at most `limit` after asking.
struct Heard {
    unsaved: Option<nucleate::reporter::Counted>,
    worktrees: Option<Vec<nucleate::worktrees::Worktree>>,
    outcome: Option<Draft>,
    took: Duration,
}

fn heard(boxed: &Boxed, session: &nucleate::sessions::Session, limit: Duration) -> Heard {
    let settings = boxed.rig.settings();
    let reporter = boxed.reporter().waiting_at_most(limit);
    let started = Instant::now();
    let counts = reporter.count(&settings, SystemTime::now());
    let environment = settings.environment("client-a").unwrap();
    let outcome = reporter.outcome(&settings, environment, session);
    let nucleus = counts.nucleus("client-a").unwrap().clone();
    Heard {
        unsaved: nucleus.unsaved,
        worktrees: nucleus.worktrees,
        outcome,
        took: started.elapsed(),
    }
}

/// A census's answer as `nucleated repos` prints one, about the session
/// folder `fix-login` and nothing else.
fn answer() -> Value {
    json!({
        "version": 1, "op": "census", "condition": "ok", "complete": true,
        "folders": [{
            "folder": "fix-login", "stamp": "0123456789abcdef",
            "branch": "workspace/fix-login", "labels": ["web"],
            "worktrees": [{
                "key": "web", "repo": "web", "label": "web", "branch": "workspace/fix-login",
                "facts": {
                    "uncommitted": true, "tracked": false, "on_remote": true,
                    "head": "0123456789abcdef0123456789abcdef01234567", "on_default": true,
                },
                "commits": 0,
            }],
        }],
    })
}

fn pids_in(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .status()
        .unwrap()
        .success()
}

fn gone(pid: &str, within: Duration) -> bool {
    let started = Instant::now();
    while alive(pid) {
        if started.elapsed() > within {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn whatever_a_container_prints_in_place_of_an_answer_is_known_as_nothing() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let session = stopped_in(rig, "fix-login");
    let limit = Duration::from_secs(3);

    // Answered properly, it is used.
    let good = heard(&boxed, &session, limit);
    assert_eq!(good.unsaved.unwrap().uncommitted_repos, 1);
    assert_eq!(good.worktrees.unwrap().len(), 1);
    assert_eq!(good.outcome.unwrap().uncommitted_repos, Some(1));

    let home = &rig.home.dir;
    let file = |name: &str, value: &Value| -> String {
        let path = home.join(name);
        fs::write(&path, value.to_string()).unwrap();
        format!("exec cat '{}'", path.display())
    };
    let changed = |change: &dyn Fn(&mut Value)| {
        let mut answer = answer();
        change(&mut answer);
        answer
    };
    let mut many = answer();
    let loose = json!({ "repo": "web", "label": "web" });
    many["loose"] = json!(vec![loose; 30_000]);
    let padded = home.join("padded.json");
    fs::write(
        &padded,
        format!("{}{}", answer(), " ".repeat(3 * 1024 * 1024)),
    )
    .unwrap();
    let pid_file = home.join("hung.pids");
    let hostile: Vec<(&str, String)> = vec![
        (
            "a newer version",
            file("v.json", &changed(&|a| a["version"] = json!(2))),
        ),
        (
            "a field of its own",
            file("f.json", &changed(&|a| a["path"] = json!("/home/agent"))),
        ),
        (
            "a repo up and out",
            file(
                "r.json",
                &changed(&|a| a["repos"] = json!([{ "repo": "../x" }])),
            ),
        ),
        (
            "a branch with spaces",
            file(
                "b.json",
                &changed(&|a| a["folders"][0]["worktrees"][0]["branch"] = json!("fix login")),
            ),
        ),
        (
            "a branch with a control character",
            file(
                "c.json",
                &changed(&|a| a["folders"][0]["worktrees"][0]["branch"] = json!("fix\u{1b}login")),
            ),
        ),
        (
            "a folder that is no name",
            file(
                "n.json",
                &changed(&|a| a["folders"][0]["folder"] = json!("../x")),
            ),
        ),
        (
            "a head that is no commit",
            file(
                "h.json",
                &changed(&|a| a["folders"][0]["worktrees"][0]["facts"]["head"] = json!("HEAD")),
            ),
        ),
        (
            "an odd slug",
            file(
                "s.json",
                &changed(&|a| a["folders"][0]["worktrees"][0]["facts"]["slug"] = json!("a/b/c")),
            ),
        ),
        (
            "a count over its cap",
            file(
                "o.json",
                &changed(&|a| a["folders"][0]["worktrees"][0]["facts"]["alone"] = json!(100_001)),
            ),
        ),
        ("thirty thousand worktrees", file("m.json", &many)),
        (
            "three mebibytes",
            format!("exec cat '{}'", padded.display()),
        ),
        ("nothing at all", "exit 0".into()),
        ("a failure", "exit 1".into()),
        ("words", "echo the user typed this".into()),
        (
            "a hung program",
            format!(
                "trap '' TERM; echo $$ >> '{}'; sleep 60 & echo $! >> '{}'; wait",
                pid_file.display(),
                pid_file.display()
            ),
        ),
    ];
    // Asked for an outcome, the reporter writes one only from an answer.
    ask_for_outcome(rig);
    boxed.reporter().tick(&rig.settings(), SystemTime::now());
    assert_eq!(outcomes_spooled(rig), 1);
    let spooled = rig.spooled().len();
    for (what, does) in hostile {
        rig.container_does("agent-env-client-a", &does);
        let hangs = does.contains("sleep 60");
        let asked_before = censuses(rig);
        let seen = heard(&boxed, &session, limit);
        // Both were asked of the container: the count, and the outcome.
        assert_eq!(censuses(rig), asked_before + 2, "{what}");
        if !hangs {
            ask_for_outcome(rig);
            boxed
                .reporter()
                .waiting_at_most(limit)
                .tick(&rig.settings(), SystemTime::now());
        }
        assert_eq!(seen.unsaved, None, "{what}");
        assert_eq!(seen.worktrees, None, "{what}");
        assert!(seen.outcome.is_none(), "{what}");
        // Asked twice, each within the limit and its ending.
        assert!(
            seen.took < 2 * (limit + END_LIMIT + Duration::from_secs(1)),
            "{what} took {:?}",
            seen.took
        );
        assert_eq!(rig.spooled().len(), spooled, "{what}");
        // Cleanup knows nothing either, and removes nothing. (One that
        // hangs is given its full minute there.)
        if !hangs {
            let out = boxed.job("cleanup", &[], None);
            assert!(!out.status.success(), "{what}");
            assert!(
                rig.root.join(".worktrees").join("fix-login").is_dir(),
                "{what}"
            );
        }
    }
    // Nothing a hung call started is still running once the host ended it.
    let pids = pids_in(&pid_file);
    assert!(!pids.is_empty());
    for pid in pids {
        assert!(
            gone(&pid, END_LIMIT + END_GRACE + Duration::from_secs(3)),
            "{pid} is still running"
        );
    }

    // An answer about another folder than the one asked about tells of no
    // outcome, though it does for the whole nucleus.
    let mut other = answer();
    other["folders"][0]["folder"] = json!("other");
    rig.container_does("agent-env-client-a", &file("other.json", &other));
    let seen = heard(&boxed, &session, limit);
    assert!(seen.outcome.is_none());
    assert_eq!(seen.unsaved.unwrap().uncommitted_repos, 1);

    // An answer that says it could not read something, or left something
    // out, or was cut short, tells of the folders it read, and of no count.
    for (what, field, value) in [
        ("unreadable", "unreadable", json!(true)),
        ("left out", "left_out", json!(true)),
        ("cut short", "complete", json!(false)),
    ] {
        let mut partial = answer();
        partial[field] = value;
        rig.container_does("agent-env-client-a", &file("partial.json", &partial));
        let seen = heard(&boxed, &session, limit);
        assert_eq!(seen.unsaved, None, "{what}");
        assert_eq!(seen.worktrees.unwrap().len(), 1, "{what}");
    }

    // And once the container answers properly again, so is it used.
    fs::remove_file(home.join("containers").join("agent-env-client-a.does")).unwrap();
    assert_eq!(
        heard(&boxed, &session, limit)
            .unsaved
            .unwrap()
            .uncommitted_repos,
        1
    );
}

// ---- the privacy gate -------------------------------------------------------
//
// If any test below fails, content can leave the box. Do not release.

#[test]
fn no_string_a_container_hands_over_but_names_words_and_numbers_is_taken() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let session = stopped_in(rig, "fix-login");
    let good = || {
        let mut answer = answer();
        answer["repos"] = json!([{ "repo": "web", "refused": "config" }]);
        answer["loose"] = json!([{
            "repo": "web", "label": "web", "branch": "b", "refused": "pointer",
            "facts": answer["folders"][0]["worktrees"][0]["facts"].clone(),
        }]);
        answer["folders"][0]["worktrees"][0]["facts"]["slug"] = json!("acme/web");
        answer
    };
    let mut marked: Vec<(String, Value)> = Vec::new();
    for field in ["op", "condition", "title"] {
        let mut answer = good();
        answer[field] = json!(mark(field));
        marked.push((field.to_string(), answer));
    }
    for field in ["repo", "refused", "path"] {
        let mut answer = good();
        answer["repos"][0][field] = json!(mark(field));
        marked.push((format!("repos.{field}"), answer));
    }
    for field in ["folder", "stamp", "branch", "name"] {
        let mut answer = good();
        answer["folders"][0][field] = json!(mark(field));
        marked.push((format!("folders.{field}"), answer));
    }
    let mut answer = good();
    answer["folders"][0]["labels"] = json!([mark("labels")]);
    marked.push(("folders.labels".into(), answer));
    for list in ["folders", "loose"] {
        for field in ["key", "repo", "label", "branch", "refused", "subject"] {
            let mut answer = good();
            let at = match list {
                "folders" => &mut answer["folders"][0]["worktrees"][0],
                _ => &mut answer["loose"][0],
            };
            at[field] = json!(mark(field));
            marked.push((format!("{list}.{field}"), answer));
        }
        for field in ["head", "slug", "message"] {
            let mut answer = good();
            let at = match list {
                "folders" => &mut answer["folders"][0]["worktrees"][0],
                _ => &mut answer["loose"][0],
            };
            at["facts"][field] = json!(mark(field));
            marked.push((format!("{list}.facts.{field}"), answer));
        }
    }
    let file = rig.home.dir.join("answer.json");
    rig.container_does(
        "agent-env-client-a",
        &format!("exec cat '{}'", file.display()),
    );
    // Checked as the host checks an answer about the whole nucleus.
    let spooled = rig.spooled().len();
    for (field, answer) in &marked {
        assert!(answer.to_string().contains(MARK), "{field}");
        fs::write(&file, answer.to_string()).unwrap();
        let _ = fs::remove_file(rig.home.state_dir().join("unsaved.json"));
        ask_for_outcome(rig);
        boxed.reporter().tick(&rig.settings(), SystemTime::now());
        let counts = fs::read_to_string(rig.home.state_dir().join("unsaved.json")).unwrap();
        assert!(!counts.contains(MARK), "{field}: {counts}");
        let nucleus = &serde_json::from_str::<Value>(&counts).unwrap()["nuclei"][0];
        assert!(nucleus["unsaved"].is_null(), "{field}: {nucleus}");
        assert!(nucleus.get("worktrees").is_none(), "{field}: {nucleus}");
    }
    // Checked as the host checks an answer about one session folder, for
    // what that session left: each string marked in turn, where one that
    // is not would give an outcome.
    let mut about_one: Vec<(String, Value)> = Vec::new();
    for field in ["op", "condition"] {
        let mut one = crate::answer();
        one[field] = json!(mark(field));
        about_one.push((field.to_string(), one));
    }
    for field in ["folder", "stamp", "branch", "name"] {
        let mut one = crate::answer();
        one["folders"][0][field] = json!(mark(field));
        about_one.push((format!("folders.{field}"), one));
    }
    let mut labelled = crate::answer();
    labelled["folders"][0]["labels"] = json!([mark("labels")]);
    about_one.push(("folders.labels".into(), labelled));
    for field in ["key", "repo", "label", "branch", "refused", "subject"] {
        let mut one = crate::answer();
        one["folders"][0]["worktrees"][0][field] = json!(mark(field));
        about_one.push((format!("worktrees.{field}"), one));
    }
    for field in ["head", "slug", "message"] {
        let mut one = crate::answer();
        one["folders"][0]["worktrees"][0]["facts"][field] = json!(mark(field));
        about_one.push((format!("worktrees.facts.{field}"), one));
    }
    fs::write(&file, crate::answer().to_string()).unwrap();
    assert!(outcome(&boxed, &session).is_some(), "the control");
    for (field, answer) in &about_one {
        assert!(answer.to_string().contains(MARK), "{field}");
        fs::write(&file, answer.to_string()).unwrap();
        assert!(outcome(&boxed, &session).is_none(), "{field}");
    }
    assert_eq!(rig.spooled().len(), spooled);
    // The answer as it should be is taken.
    fs::write(&file, good().to_string()).unwrap();
    let _ = fs::remove_file(rig.home.state_dir().join("unsaved.json"));
    boxed.reporter().tick(&rig.settings(), SystemTime::now());
    let counts: Value = serde_json::from_str(
        &fs::read_to_string(rig.home.state_dir().join("unsaved.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(counts["nuclei"][0]["worktrees"][0]["folder"], "fix-login");
    gate::tick(rig);
    assert!(!mock.to(contract::HEARTBEATS).is_empty());
    gate::assert_clean(rig, &mock);
}

#[test]
fn what_a_real_census_reads_stays_in_the_container() {
    let boxed = boxed_in(Rig::under(MARK));
    let rig = &boxed.rig;
    // The remote's address holds the sentinel: the upstream is below it.
    assert!(git(&rig.bare(), &["config", "remote.origin.url"]).contains(MARK));
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let content = format!("{MARK} in a file the user wrote\n");
    fs::write(worktree.join("notes.txt"), &content).unwrap();
    git(&worktree, &["add", "notes.txt"]);
    git(&worktree, &["commit", "-q", "-m", &mark("subject")]);
    fs::write(worktree.join("draft.txt"), &content).unwrap();
    fs::write(worktree.join(format!("{MARK}.txt")), &content).unwrap();
    // And a file at the folder's root, by a name of its own.
    fs::write(
        rig.root
            .join(".worktrees/fix-login")
            .join(format!("{MARK}-notes")),
        &content,
    )
    .unwrap();
    let session = stopped_in(rig, "fix-login");

    boxed.reporter().tick(&rig.settings(), SystemTime::now());
    let counts = boxed.counts();
    let nucleus = counts.nucleus("client-a").unwrap();
    assert_eq!(nucleus.unsaved.unwrap().uncommitted_repos, 1);
    assert_eq!(nucleus.unsaved.unwrap().unpushed_commits, 1);
    let folders = nucleus.worktrees.as_ref().unwrap();
    assert_eq!(folders[0].folder, "fix-login");
    // The file at its root is said of the folder, not by its name.
    assert!(folders[0].repos.iter().any(|repo| repo.repo == "fix-login"));
    let told = outcome(&boxed, &session).unwrap();
    assert_eq!(told.commits, Some(1));
    // What reached the host, as the host holds it, and as it would go on.
    let ask = rig.home.state_dir().join("outcomes").join("client-a");
    let _ = fs::remove_dir_all(&ask);
    nucleate::reporter::ask(
        &rig.home.state_dir(),
        "client-a",
        "s1",
        SystemTime::now() - Duration::from_secs(60),
    );
    boxed.reporter().tick(&rig.settings(), SystemTime::now());
    assert!(rig
        .spooled()
        .iter()
        .any(|record| record["event"]["type"] == "outcome"));
    gate::tick(rig);
    assert!(!mock.to(contract::EVENTS).is_empty());
    assert!(asked_inside(rig, "census"));
    gate::assert_clean(rig, &mock);
}

// ---- the rule, split in two -------------------------------------------------

/// Every worktree of the nucleus, judged by what git says of it and what
/// GitHub says, comes to what the one rule says of it as it stands.
#[test]
fn facts_and_a_verdict_come_to_what_the_one_rule_says() {
    let rig = Rig::new();
    rig.as_github("octo/web");
    let commits = merged_session(&rig, "merged");
    rig.session("clean");
    let dirty = rig.session("dirty");
    fs::write(dirty.join("notes.txt"), "not committed\n").unwrap();
    let ahead = rig.session("ahead");
    commit(&ahead, "a.txt", "one");
    commit(&ahead, "b.txt", "two");
    let tracked = rig.session("tracked");
    commit(&tracked, "c.txt", "pushed");
    rig.push(&tracked);
    commit(&tracked, "d.txt", "after");
    let pushed = rig.session("pushed");
    commit(&pushed, "e.txt", "pushed");
    rig.push(&pushed);
    let detached = rig.session("detached");
    git(&detached, &["checkout", "-q", "--detach"]);
    let on_default = rig.session("on-default");
    git(
        &on_default,
        &["branch", "-q", "--set-upstream-to", "origin/main"],
    );
    commit(&on_default, "f.txt", "on top of main");
    let github = github(commits);
    let config = nucleate::config::Config {
        root: rig.root.clone(),
        github_token_file: None,
        branch_prefix: "workspace".into(),
        github_api: github.url.clone(),
    };
    let workspace = Workspace::new(config).asking_github_at(&github.url);
    let mut judged = 0;
    for name in [
        "merged",
        "clean",
        "dirty",
        "ahead",
        "tracked",
        "pushed",
        "detached",
        "on-default",
    ] {
        let path = rig.root.join(".worktrees").join(name).join("web");
        let facts = workspace.facts(&path).unwrap();
        let verdict = Unsaved::verdict(&facts, || {
            workspace.merged_on_github(facts.slug.as_deref(), &facts.head)
        });
        assert_eq!(verdict, workspace.unsaved(&path).unwrap(), "{name}");
        judged += 1;
    }
    assert_eq!(judged, 8);
    // The cases are what they say: each kind of answer is among them.
    let unsaved = |name: &str| {
        workspace
            .unsaved(&rig.root.join(".worktrees").join(name).join("web"))
            .unwrap()
    };
    assert!(unsaved("merged").is_saved());
    assert!(unsaved("dirty").uncommitted);
    assert_eq!(unsaved("ahead").unpushed, 2);
    assert!(unsaved("tracked").tracked);
    assert_eq!(unsaved("tracked").unpushed, 1);
    assert!(unsaved("pushed").is_saved());
    assert_eq!(unsaved("on-default").unpushed, 1);
}

// ---- what is changed inside -------------------------------------------------

/// The `podman` the tests run in-process calls through: git for the
/// container inside it, each call given `patience` when it is given.
fn inside(boxed: &Boxed, patience: Option<Duration>) -> nucleate::repos::Inside {
    nucleate::repos::Inside {
        runner: boxed.rig.looker(),
        patience,
    }
}

/// A housekeeping job for `client-a`, run in-process as the daemon runs it.
fn job_inside(
    boxed: &Boxed,
    job: nucleate::housekeeping::Job,
    patience: Option<Duration>,
) -> anyhow::Result<Value> {
    let settings = boxed.rig.settings();
    let environment = settings.environment("client-a").unwrap();
    let inside = inside(boxed, patience);
    nucleate::housekeeping::run_job_with(&settings, environment, job, false, Some(&inside))
}

/// An action's id, new each time it is asked for.
fn action_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("wta_{:016}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn action(folder: &str, kind: &str, discard: bool) -> Value {
    json!({
        "id": action_id(), "env_id": "client-a", "folder": folder, "kind": kind,
        "discard": discard, "requested_at": nucleate::envelope::rfc3339(SystemTime::now()),
    })
}

fn result_file(rig: &Rig, action: &Value) -> PathBuf {
    rig.home
        .state_dir()
        .join("worktree-actions/results/client-a")
        .join(format!("{}.json", action["id"].as_str().unwrap()))
}

/// Run one Watch action in-process, as the daemon's thread for them does,
/// and hand back its result.
fn act_inside(boxed: &Boxed, action: &Value, patience: Option<Duration>) -> Value {
    let settings = boxed.rig.settings();
    let taken: nucleate::worktrees::Action = serde_json::from_value(action.clone()).unwrap();
    nucleate::worktrees::take(&settings, &[taken], SystemTime::now());
    let inside = inside(boxed, patience);
    nucleate::worktrees::run_queued_with(&settings, Some(&inside));
    serde_json::from_str(&fs::read_to_string(result_file(&boxed.rig, action)).unwrap()).unwrap()
}

/// A Watch that hands out `actions` in its answer to every heartbeat.
fn handing_out(actions: Vec<Value>) -> Mock {
    let mut mock = Mock::start(move |request: &Request| {
        if contract::violation_taking(request, &["worktrees"]).is_some() {
            return Reply::problem(400, "validation_failed");
        }
        let received = json!("2026-11-19T12:00:00.000Z");
        match request.path.as_str() {
            contract::HEARTBEATS => Reply::json(
                200,
                json!({
                    "received_at": received, "heartbeat_interval_s": 60,
                    "accepts": ["worktrees"], "worktree_actions": actions,
                }),
            ),
            _ => Reply::json(
                200,
                json!({ "received_at": received, "accepted": 1, "duplicates": 0, "rejected": [] }),
            ),
        }
    });
    mock.ahead = &["worktrees"];
    mock
}

/// Run one Watch action as the box does, by `nucleated run` with the host's
/// `PATH`, and hand back its result.
fn act_through_the_daemon(boxed: &Boxed, action: &Value) -> Value {
    let rig = &boxed.rig;
    let mock = handing_out(vec![action.clone()]);
    rig.sign_in(&mock.url);
    rig.looker();
    let file = result_file(rig, action);
    gate::daemon_until(rig, Some(&boxed.path), "the action's result", &|| {
        file.is_file()
    });
    serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap()
}

/// Set a file's modification time well into the past, so that it does not
/// look like something being written.
fn age(path: &Path) {
    assert!(std::process::Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(path)
        .status()
        .unwrap()
        .success());
}

/// The tree the host has recorded as snapshotted for `name`,
/// `<repo>-<branch>`.
fn recorded(rig: &Rig, name: &str) -> Option<String> {
    let file = rig
        .home
        .state_dir()
        .join("snapshots-contained/client-a.json");
    let text = fs::read_to_string(file).ok()?;
    let recorded: Value = serde_json::from_str(&text).unwrap();
    recorded[name]["tree"].as_str().map(str::to_string)
}

/// A session whose worktree holds a file not committed that a snapshot
/// would push.
fn with_work(rig: &Rig, name: &str) -> PathBuf {
    let worktree = rig.session(name);
    fs::write(worktree.join("notes.txt"), "work\n").unwrap();
    age(&worktree.join("notes.txt"));
    worktree
}

#[test]
fn a_refresh_moves_the_repos_and_fetches_them_inside_the_container() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.session("fix-login");
    let flat = rig.flatten();
    commit(&rig.upstream, "later.txt", "later");
    let head = git(&rig.upstream, &["rev-parse", "HEAD"]);

    let out = boxed.job("refresh", &[], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = &stdout_json(&out)[0];
    assert_eq!(
        report["repos"],
        json!([{ "repo": "web", "ok": true }]),
        "{report}"
    );
    assert_eq!(report["moved"], 1);
    assert!(!flat.exists());
    let moved = rig.root.join(".repos/_/web.git");
    assert_eq!(
        git(&moved, &["rev-parse", "refs/remotes/origin/main"]),
        head
    );
    // The session's worktree follows its repo.
    let worktree = rig.root.join(".worktrees/fix-login/web");
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    assert_eq!(boxed.host_git(), Vec::<String>::new());
}

#[test]
fn the_daemon_moves_a_container_nucleus_inside_it_when_it_starts() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.session("fix-login");
    let flat = rig.flatten();
    let moved = rig.root.join(".repos/_/web.git");
    rig.looker();
    let out = gate::daemon_until(rig, Some(&boxed.path), "the move", &|| {
        moved.is_dir() && !flat.exists()
    });
    assert_eq!(boxed.host_git(), Vec::<String>::new(), "{}", stderr(&out));
}

#[test]
fn a_snapshot_is_pushed_from_inside_and_what_the_host_recorded_is_not_pushed_again() {
    let boxed = boxed();
    let rig = &boxed.rig;
    with_work(rig, "fix-login");

    let out = boxed.job("snapshot", &["--dry-run"], None);
    let report = &stdout_json(&out)[0];
    assert_eq!(
        report["worktrees"][0]["action"], "would_snapshot",
        "{report}"
    );
    assert_eq!(git(&rig.upstream, &["for-each-ref", "refs/wip"]), "");

    let out = boxed.job("snapshot", &[], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = &stdout_json(&out)[0];
    assert_eq!(
        report["worktrees"],
        json!([{
            "repo": "web", "folder": "fix-login", "action": "snapshot",
            "branch": "workspace/fix-login", "ref": "refs/wip/workspace-fix-login",
        }])
    );
    let pushed = git(
        &rig.upstream,
        &["rev-parse", "refs/wip/workspace-fix-login^{tree}"],
    );
    assert_eq!(recorded(rig, "web-workspace-fix-login").unwrap(), pushed);

    let out = boxed.job("snapshot", &[], None);
    let report = &stdout_json(&out)[0];
    assert_eq!(report["worktrees"][0]["action"], "skipped");
    assert_eq!(report["worktrees"][0]["reason"], "already snapshotted");
    assert_eq!(boxed.host_git(), Vec::<String>::new());

    // The record is one file of its own, and a full one reads back whole:
    // the work recorded among four thousand others is not pushed again.
    let file = rig
        .home
        .state_dir()
        .join("snapshots-contained/client-a.json");
    assert!(!rig.home.state_dir().join("snapshots/client-a").exists());
    let long = "x".repeat(193);
    let mut full: serde_json::Map<String, Value> = (0..4_095)
        .map(|n| {
            let name = format!("{long}-{n:05}");
            (name, json!({ "tree": pushed.clone(), "at_ms": n + 1 }))
        })
        .collect();
    full.insert(
        "web-workspace-fix-login".into(),
        json!({ "tree": pushed.clone(), "at_ms": 1_000_000 }),
    );
    fs::write(&file, Value::Object(full).to_string()).unwrap();
    let out = boxed.job("snapshot", &[], None);
    assert_eq!(
        stdout_json(&out)[0]["worktrees"][0]["reason"],
        "already snapshotted"
    );

    // It does not grow without end: the oldest go first.
    let many: serde_json::Map<String, Value> = (0..5_000)
        .map(|n| {
            let name = format!("old-{n:05}");
            (name, json!({ "tree": pushed.clone(), "at_ms": 5_000 - n }))
        })
        .collect();
    fs::write(&file, Value::Object(many).to_string()).unwrap();
    fs::write(rig.root.join(".worktrees/fix-login/web/more.txt"), "more\n").unwrap();
    age(&rig.root.join(".worktrees/fix-login/web/more.txt"));
    let out = boxed.job("snapshot", &[], None);
    assert_eq!(stdout_json(&out)[0]["worktrees"][0]["action"], "snapshot");
    let kept: serde_json::Map<String, Value> =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(kept.len(), 4096);
    assert!(kept.contains_key("web-workspace-fix-login"));
    // The newest of the old ones are the ones kept: the lowest numbers.
    assert!(kept.contains_key("old-00000"));
    assert!(!kept.contains_key("old-04999"));
}

#[test]
fn a_tree_recorded_for_one_branch_does_not_stop_the_same_tree_on_another() {
    let boxed = boxed();
    let rig = &boxed.rig;
    with_work(rig, "one");
    let other = rig.session("two");
    let out = boxed.job("snapshot", &[], None);
    assert_eq!(stdout_json(&out)[0]["worktrees"][0]["action"], "snapshot");
    // The same work on another branch makes the same tree, which is that
    // branch's to snapshot all the same.
    fs::write(other.join("notes.txt"), "work\n").unwrap();
    age(&other.join("notes.txt"));
    let out = boxed.job("snapshot", &[], None);
    let report = &stdout_json(&out)[0];
    let two = report["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["folder"] == "two")
        .cloned()
        .unwrap();
    assert_eq!(two["action"], "snapshot", "{report}");
    assert_eq!(
        git(
            &rig.upstream,
            &["rev-parse", "refs/wip/workspace-one^{tree}"]
        ),
        git(
            &rig.upstream,
            &["rev-parse", "refs/wip/workspace-two^{tree}"]
        )
    );
}

#[test]
fn push_runs_inside_the_container_and_says_what_it_pushed() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt", "work");
    let head = git(&worktree, &["rev-parse", "HEAD"]);

    let result = act_through_the_daemon(&boxed, &action("fix-login", "push", false));
    assert_eq!(result["outcome"], "done", "{result}");
    assert_eq!(result["detail"], "Pushed 1 commit in web.");
    assert_eq!(
        git(
            &rig.upstream,
            &["rev-parse", "refs/heads/workspace/fix-login"]
        ),
        head
    );
    assert_eq!(boxed.host_git(), Vec::<String>::new());

    // Pushed, there is nothing more to push.
    let again = act_inside(&boxed, &action("fix-login", "push", false), None);
    assert_eq!(again["outcome"], "refused");
    assert_eq!(again["detail"], "There is nothing to push.");
    let gone = act_inside(&boxed, &action("nowhere", "push", false), None);
    assert_eq!(gone["detail"], "There is nothing to push.");
}

#[test]
fn a_folder_whose_name_starts_with_a_dash_is_asked_about_as_a_folder() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    // A folder the container made, which `nucleate` would not name so.
    let folder = rig.root.join(".worktrees").join("-x");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join(".workspace.json"),
        r#"{"name":"-x","branch":"workspace/x","repos":{}}"#,
    )
    .unwrap();
    let result = act_inside(&boxed, &action("-x", "push", false), None);
    assert_eq!(result["outcome"], "refused", "{result}");
    assert_eq!(result["detail"], "There is nothing to push.");
}

#[test]
fn a_push_github_turns_down_is_said_in_the_hosts_words() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt", "first");
    rig.push(&worktree);
    // The branch moves on on the remote, and here another way.
    git(&rig.upstream, &["checkout", "-q", "workspace/fix-login"]);
    commit(&rig.upstream, "theirs.txt", "theirs");
    git(&rig.upstream, &["checkout", "-q", "main"]);
    git(&worktree, &["reset", "-q", "--hard", "HEAD~1"]);
    commit(&worktree, "b.txt", "ours");
    rig.home.git(&worktree, &["fetch", "-q"]);
    git(&worktree, &["reset", "-q", "--soft", "HEAD"]);
    let result = act_inside(&boxed, &action("fix-login", "push", false), None);
    assert_eq!(result["outcome"], "failed", "{result}");
    assert_eq!(
        result["detail"],
        "web was not pushed: the branch on GitHub has commits this one does not."
    );
}

/// A proxy that takes no one anywhere: every `CONNECT` is refused, as an
/// environment's proxy refuses GitHub when its network is `none`.
fn refusing_proxy() -> String {
    proxy_answering("403 Forbidden")
}

/// A proxy that answers every `CONNECT` with `status`.
fn proxy_answering(status: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut asked = [0u8; 4096];
            let _ = stream.read(&mut asked);
            let said = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
            let _ = stream.write_all(said.as_bytes());
        }
    });
    format!("http://{address}")
}

#[test]
fn where_the_environments_network_does_not_reach_github_nothing_is_sent_and_it_says_so() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt", "work");
    // The repo is GitHub's, as a box's are, and the container's proxy will
    // not go there.
    git(
        &rig.bare(),
        &[
            "config",
            "remote.origin.url",
            "https://github.com/octo/web.git",
        ],
    );
    let proxy = refusing_proxy();
    rig.container_does(
        "agent-env-client-a",
        &format!(
            "HTTPS_PROXY='{proxy}' https_proxy='{proxy}' ALL_PROXY='{proxy}'\n\
             export HTTPS_PROXY https_proxy ALL_PROXY\nexec \"$@\""
        ),
    );
    let out = boxed.job("refresh", &[], None);
    let report = &stdout_json(&out)[0];
    assert_eq!(report["repos"][0]["ok"], false, "{report}");
    assert_eq!(
        report["repos"][0]["error"],
        "The environment's network does not reach GitHub, so nothing was sent."
    );
    let result = act_inside(&boxed, &action("fix-login", "push", false), None);
    assert_eq!(result["outcome"], "failed");
    assert_eq!(
        result["detail"],
        "The environment's network does not reach GitHub, so nothing was sent."
    );
    assert_eq!(boxed.host_git(), Vec::<String>::new());
}

/// The container's proxy in front of every https remote.
fn behind(rig: &Rig, proxy: &str) {
    rig.container_does(
        "agent-env-client-a",
        &format!(
            "HTTPS_PROXY='{proxy}' https_proxy='{proxy}' ALL_PROXY='{proxy}'\n\
             export HTTPS_PROXY https_proxy ALL_PROXY\nexec \"$@\""
        ),
    );
}

#[test]
fn a_proxy_that_fails_another_way_is_github_not_being_reached() {
    let boxed = boxed();
    let rig = &boxed.rig;
    git(
        &rig.bare(),
        &[
            "config",
            "remote.origin.url",
            "https://github.com/octo/web.git",
        ],
    );
    behind(rig, &proxy_answering("502 Bad Gateway"));
    let out = boxed.job("refresh", &[], None);
    let report = &stdout_json(&out)[0];
    assert_eq!(
        report["repos"][0]["error"], "GitHub could not be reached",
        "{report}"
    );
}

#[test]
fn where_one_repo_cannot_reach_github_the_others_are_still_pushed_and_said() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    // A second repo, whose remote is GitHub's, behind the proxy.
    let api = rig.home.dir.join("upstream").join("api");
    fs::create_dir_all(&api).unwrap();
    git(&api, &["init", "-q", "-b", "main"]);
    commit(&api, "README.md", "init");
    let cloned = run(rig.nucleate().args(["clone", api.to_str().unwrap()]), "");
    assert!(cloned.status.success(), "{}", stderr(&cloned));
    let made = run(
        rig.nucleate().args(["create-session", "two", "web", "api"]),
        "",
    );
    assert!(made.status.success(), "{}", stderr(&made));
    let two = rig.root.join(".worktrees").join("two");
    commit(&two.join("web"), "a.txt", "web work");
    commit(&two.join("api"), "b.txt", "api work");
    git(
        &rig.root.join(".repos/_/api.git"),
        &[
            "config",
            "remote.origin.url",
            "https://github.com/octo/api.git",
        ],
    );
    behind(rig, &refusing_proxy());
    let result = act_inside(&boxed, &action("two", "push", false), None);
    assert_eq!(result["outcome"], "failed", "{result}");
    assert_eq!(
        result["detail"],
        "Pushed 1 commit in web. api was not pushed: the environment's network does not reach GitHub."
    );
    assert!(!git(&rig.upstream, &["branch", "--list", "workspace/two"]).is_empty());
}

/// Whether a process is in a folder is read from `/proc`, so what is
/// removed can only be shown where there is one.
#[cfg(target_os = "linux")]
#[test]
fn clean_up_with_discard_backs_up_inside_and_removes_the_folder() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let asked = action("fix-login", "clean_up", true);
    let id = asked["id"].as_str().unwrap().to_string();
    let result = act_through_the_daemon(&boxed, &asked);
    let backup = format!("refs/wip/workspace-fix-login-{id}");
    assert_eq!(result["outcome"], "done", "{result}");
    assert_eq!(result["detail"], format!("Backed up to {backup} in web."));
    let tree = git(&rig.upstream, &["ls-tree", "--name-only", &backup]);
    assert!(tree.lines().any(|name| name == "notes.txt"), "{tree}");
    assert!(!rig.root.join(".worktrees/fix-login").exists());
    assert_eq!(boxed.host_git(), Vec::<String>::new());

    // Without discard, a folder that holds work keeps it.
    let other = rig.session("other");
    fs::write(other.join("notes.txt"), "not committed\n").unwrap();
    let kept = act_inside(&boxed, &action("other", "clean_up", false), None);
    assert_eq!(kept["outcome"], "refused");
    assert_eq!(kept["detail"], "It holds changes not committed in web.");
    assert!(other.is_dir());
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_removes_inside_what_the_host_decided_could_go() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.as_github("octo/web");
    let commits = merged_session(rig, "merged");
    three_folders(rig);
    let github = github(commits);
    rig.configure_in_container("[housekeeping]\ncleanup_min_age = \"0s\"\n");
    let out = boxed.job("cleanup", &[], Some(&github));
    assert!(out.status.success(), "{}", stderr(&out));
    let report = &stdout_json(&out)[0];
    assert_eq!(verdict(report, "merged")["action"], "removed", "{report}");
    assert_eq!(verdict(report, "pushed")["action"], "removed", "{report}");
    assert_eq!(verdict(report, "dirty")["action"], "kept");
    assert!(!rig.root.join(".worktrees/merged").exists());
    assert!(!rig.root.join(".worktrees/pushed").exists());
    assert!(rig.root.join(".worktrees/dirty").is_dir());
    assert_eq!(boxed.host_git(), Vec::<String>::new());
}

#[cfg(target_os = "linux")]
#[test]
fn a_folder_that_changed_since_the_host_looked_at_it_is_not_removed() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let pushed = rig.session("pushed");
    commit(&pushed, "b.txt", "pushed work");
    rig.push(&pushed);
    rig.configure_in_container(
        "worktree_actions = true\n[housekeeping]\ncleanup_min_age = \"0s\"\n",
    );
    // Something is written in it between the host's look and the removal.
    rig.container_does(
        "agent-env-client-a",
        &format!(
            "case \"$*\" in *'--do remove'*) echo late > '{}'/late-$$.txt ;; esac\nexec \"$@\"",
            pushed.display()
        ),
    );
    let out = boxed.job("cleanup", &[], None);
    let report = &stdout_json(&out)[0];
    assert_eq!(
        verdict(report, "pushed"),
        json!({ "session": "pushed", "action": "kept", "reason": "it changed since it was looked at" })
    );
    assert!(pushed.is_dir());
    let result = act_inside(&boxed, &action("pushed", "clean_up", true), None);
    assert_eq!(result["outcome"], "failed");
    assert_eq!(
        result["detail"],
        "It changed while it was looked at, so nothing was done."
    );
    assert!(pushed.is_dir());
}

/// From now on the container answers `op` by printing `answer`, and runs
/// everything else as it should.
fn answers(rig: &Rig, op: &str, answer: &Value) {
    let file = rig.home.dir.join(format!("{op}-answer.json"));
    fs::write(&file, answer.to_string()).unwrap();
    rig.container_does(
        "agent-env-client-a",
        &format!(
            "case \"$*\" in *'--do {op} '*|*'--do {op}') exec cat '{}' ;; esac\nexec \"$@\"",
            file.display()
        ),
    );
}

fn write_answer(op: &str, condition: &str, part: &str, entries: Value) -> Value {
    let mut answer =
        json!({ "version": 1, "op": op, "condition": condition, "complete": condition == "ok" });
    answer[part] = entries;
    answer
}

#[test]
fn a_refresh_or_snapshot_answer_out_of_line_or_cut_short_is_taken_for_no_more_than_it_says() {
    use nucleate::housekeeping::Job;
    let boxed = boxed();
    let rig = &boxed.rig;
    with_work(rig, "fix-login");
    let keys = rig
        .home
        .state_dir()
        .join("snapshots-contained/client-a.json");
    let fetched = |repo: &str| json!([{ "repo": repo, "ok": true }]);
    for (what, answer) in [
        (
            "a repo up and out",
            write_answer("refresh", "ok", "fetched", fetched("../x")),
        ),
        (
            "a word of its own",
            write_answer(
                "refresh",
                "ok",
                "fetched",
                json!([{ "repo": "web", "ok": false, "failure": "it broke" }]),
            ),
        ),
        (
            "another op's part",
            write_answer("refresh", "ok", "pushes", json!([{ "repo": "web" }])),
        ),
    ] {
        answers(rig, "refresh", &answer);
        assert!(job_inside(&boxed, Job::Refresh, None).is_err(), "{what}");
    }
    // Cut short: what it did not list was not done.
    answers(
        rig,
        "refresh",
        &write_answer("refresh", "timeout", "fetched", fetched("web")),
    );
    let error = job_inside(&boxed, Job::Refresh, None).unwrap_err();
    assert!(format!("{error:#}").contains("took too long"), "{error:#}");
    // As when it could not read, or did not look at, some of the nucleus.
    for flag in ["unreadable", "left_out"] {
        let mut answer = write_answer("refresh", "ok", "fetched", fetched("web"));
        answer[flag] = json!(true);
        answers(rig, "refresh", &answer);
        let error = job_inside(&boxed, Job::Refresh, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("could not read all"),
            "{flag}: {error:#}"
        );
    }

    let tree = "0123456789abcdef0123456789abcdef01234567";
    let snapshot = |reference: &str| {
        json!([{
            "repo": "web", "folder": "fix-login", "branch": "workspace/fix-login",
            "action": "snapshot", "tree": tree, "ref": reference,
        }])
    };
    for (what, answer) in [
        (
            "a ref outside refs/wip",
            write_answer("snapshot", "ok", "snapshots", snapshot("refs/heads/main")),
        ),
        (
            "another branch's ref",
            write_answer(
                "snapshot",
                "ok",
                "snapshots",
                snapshot("refs/wip/workspace-other"),
            ),
        ),
        (
            "would_snapshot in a run that was to push",
            write_answer(
                "snapshot",
                "ok",
                "snapshots",
                json!([{
                    "repo": "web", "branch": "workspace/fix-login", "action": "would_snapshot",
                    "tree": tree, "ref": "refs/wip/workspace-fix-login",
                }]),
            ),
        ),
        (
            "a tree that is no tree",
            write_answer(
                "snapshot",
                "ok",
                "snapshots",
                json!([{
                    "repo": "web", "branch": "workspace/fix-login", "action": "snapshot",
                    "tree": "HEAD", "ref": "refs/wip/workspace-fix-login",
                }]),
            ),
        ),
    ] {
        answers(rig, "snapshot", &answer);
        assert!(job_inside(&boxed, Job::Snapshot, None).is_err(), "{what}");
        assert!(!keys.exists(), "{what}");
    }
    // A dry run that says it pushed something is no answer, and nothing of
    // it is recorded.
    answers(
        rig,
        "snapshot",
        &write_answer(
            "snapshot",
            "ok",
            "snapshots",
            snapshot("refs/wip/workspace-fix-login"),
        ),
    );
    let settings = rig.settings();
    let environment = settings.environment("client-a").unwrap();
    let dry = nucleate::housekeeping::run_job_with(
        &settings,
        environment,
        Job::Snapshot,
        true,
        Some(&inside(&boxed, None)),
    );
    assert!(dry.is_err(), "{dry:?}");
    assert!(!keys.exists());
    // Cut short, what it says it pushed is recorded, and the run is not good.
    answers(
        rig,
        "snapshot",
        &write_answer(
            "snapshot",
            "timeout",
            "snapshots",
            snapshot("refs/wip/workspace-fix-login"),
        ),
    );
    assert!(job_inside(&boxed, Job::Snapshot, None).is_err());
    assert_eq!(
        recorded(rig, "web-workspace-fix-login").as_deref(),
        Some(tree)
    );
    // One that could not read, or did not look at, some of the nucleus is
    // recorded for what it did, and the run is not good.
    for flag in ["unreadable", "left_out"] {
        fs::remove_file(&keys).unwrap();
        let mut answer = write_answer(
            "snapshot",
            "ok",
            "snapshots",
            snapshot("refs/wip/workspace-fix-login"),
        );
        answer[flag] = json!(true);
        answers(rig, "snapshot", &answer);
        let error = job_inside(&boxed, Job::Snapshot, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("could not read all"),
            "{flag}: {error:#}"
        );
        assert!(recorded(rig, "web-workspace-fix-login").is_some(), "{flag}");
    }
    assert_eq!(git(&rig.upstream, &["for-each-ref", "refs/wip"]), "");
}

#[test]
fn a_push_answer_out_of_line_or_cut_short_is_taken_for_no_more_than_it_says() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    commit(&worktree, "a.txt", "work");
    for (what, entries) in [
        (
            "a repo with spaces",
            json!([{ "repo": "the user typed this", "pushed": 1 }]),
        ),
        (
            "a count over its cap",
            json!([{ "repo": "web", "pushed": 100_001 }]),
        ),
        (
            "a word of its own",
            json!([{ "repo": "web", "failure": "it broke" }]),
        ),
    ] {
        answers(rig, "push", &write_answer("push", "ok", "pushes", entries));
        let result = act_inside(&boxed, &action("fix-login", "push", false), None);
        assert_eq!(result["outcome"], "failed", "{what}");
        assert_eq!(
            result["detail"],
            "The environment gave no answer that could be used, so nothing was done.",
            "{what}"
        );
    }
    answers(
        rig,
        "push",
        &write_answer(
            "push",
            "timeout",
            "pushes",
            json!([{ "repo": "web", "pushed": 1 }]),
        ),
    );
    let result = act_inside(&boxed, &action("fix-login", "push", false), None);
    assert_eq!(result["outcome"], "failed");
    assert_eq!(
        result["detail"],
        "Pushed 1 commit in web. The rest was not pushed: it took too long."
    );
    assert!(git(&rig.upstream, &["branch", "--list", "workspace/fix-login"]).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn a_removal_answer_out_of_line_or_cut_short_removes_nothing_it_does_not_say() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let tree = "0123456789abcdef0123456789abcdef01234567";
    // Each backup fits in every way but its ref.
    let backed_up = |reference: &str| {
        json!([{ "folder": "fix-login", "outcome": "backed_up", "backed_up": [{
            "repo": "web", "branch": "workspace/fix-login", "tree": tree, "ref": reference,
        }] }])
    };
    for (what, discard, entries) in [
        (
            "done for a folder not asked",
            false,
            json!([{ "folder": "other", "outcome": "removed" }]),
        ),
        (
            "a backup outside refs/wip",
            true,
            backed_up("refs/heads/main"),
        ),
        (
            "a backup of another action",
            true,
            backed_up("refs/wip/workspace-fix-login-wta_0000000000000000"),
        ),
        (
            "a backup of another branch",
            true,
            backed_up("refs/wip/workspace-other-{id}"),
        ),
    ] {
        let asked = action("fix-login", "clean_up", discard);
        let id = asked["id"].as_str().unwrap();
        let entries: Value =
            serde_json::from_str(&entries.to_string().replace("{id}", id)).unwrap();
        answers(
            rig,
            "remove",
            &write_answer("remove", "ok", "removals", entries),
        );
        let result = act_inside(&boxed, &asked, None);
        assert_eq!(result["outcome"], "failed", "{what}: {result}");
        assert_eq!(
            result["detail"],
            "The environment gave no answer that could be used, so nothing was done.",
            "{what}"
        );
        assert!(worktree.is_dir(), "{what}");
    }
    // The same answer with this branch's ref for this action is taken.
    let asked = action("fix-login", "clean_up", true);
    let id = asked["id"].as_str().unwrap();
    let mut good = backed_up(&format!("refs/wip/workspace-fix-login-{id}"));
    good[0]["outcome"] = json!("kept");
    good[0]["why"] = json!("in_use");
    answers(
        rig,
        "remove",
        &write_answer("remove", "ok", "removals", good),
    );
    let result = act_inside(&boxed, &asked, None);
    assert_eq!(result["outcome"], "refused", "{result}");

    answers(
        rig,
        "remove",
        &json!({ "version": 1, "op": "remove", "condition": "timeout" }),
    );
    let result = act_inside(&boxed, &action("fix-login", "clean_up", true), None);
    assert_eq!(
        result["detail"],
        "It took too long, so nothing was removed."
    );
    assert!(worktree.is_dir());
    assert_eq!(git(&rig.upstream, &["for-each-ref", "refs/wip"]), "");
}

/// A Clean up with discard whose removal call answers with `does`, the
/// backups having been made for real.
#[cfg(target_os = "linux")]
fn removal_answers(rig: &Rig, does: &str) {
    rig.container_does(
        "agent-env-client-a",
        &format!("case \"$*\" in *--backup-only*) exec \"$@\" ;; *'--do remove'*) {does} ;; esac\nexec \"$@\""),
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_removal_begun_when_the_time_ran_out_is_said_as_unknown_with_its_backups() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let file = rig.home.dir.join("started.json");
    fs::write(
        &file,
        json!({ "version": 1, "op": "remove", "condition": "timeout",
            "removals": [{ "folder": "fix-login", "outcome": "started" }] })
        .to_string(),
    )
    .unwrap();
    removal_answers(rig, &format!("exec cat '{}'", file.display()));
    let asked = action("fix-login", "clean_up", true);
    let id = asked["id"].as_str().unwrap().to_string();
    let result = act_inside(&boxed, &asked, None);
    assert_eq!(result["outcome"], "failed", "{result}");
    assert_eq!(
        result["detail"],
        format!(
            "Backed up to refs/wip/workspace-fix-login-{id} in web. It took too long to say \
             whether it was removed."
        )
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_session_that_comes_back_while_the_backups_are_pushed_keeps_the_folder() {
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    // A session of this box in the folder, which it learns of only once
    // the backups are made: the container cannot see the box's store.
    stopped_in(rig, "fix-login");
    let kept = rig.home.state_dir().join("sessions/client-a/s1.json");
    let aside = rig.home.dir.join("session-aside.json");
    fs::rename(&kept, &aside).unwrap();
    rig.container_does(
        "agent-env-client-a",
        &format!(
            "case \"$*\" in *--backup-only*) \"$@\"; done=$?; cp '{}' '{}'; exit $done ;; esac\n\
             exec \"$@\"",
            aside.display(),
            kept.display()
        ),
    );
    let asked = action("fix-login", "clean_up", true);
    let id = asked["id"].as_str().unwrap().to_string();
    let result = act_inside(&boxed, &asked, None);
    assert_eq!(result["outcome"], "refused", "{result}");
    assert_eq!(
        result["detail"],
        format!(
            "Backed up to refs/wip/workspace-fix-login-{id} in web. A session is still using it."
        )
    );
    assert!(worktree.is_dir());
}

#[test]
fn a_write_that_hangs_is_ended_by_the_host_and_leaves_nothing_running() {
    use nucleate::housekeeping::Job;
    let boxed = boxed();
    let rig = &boxed.rig;
    let pid_file = rig.home.dir.join("hung.pids");
    rig.container_does(
        "agent-env-client-a",
        &format!(
            "trap '' TERM; echo $$ >> '{pids}'; sleep 60 & echo $! >> '{pids}'; wait",
            pids = pid_file.display()
        ),
    );
    let patience = Duration::from_secs(3);
    let started = Instant::now();
    let error = job_inside(&boxed, Job::Refresh, Some(patience)).unwrap_err();
    assert!(
        started.elapsed() < patience + END_LIMIT,
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        format!("{error:#}"),
        "The environment took too long to answer, so nothing was done."
    );
    let pids = pids_in(&pid_file);
    assert_eq!(pids.len(), 2);
    for pid in pids {
        assert!(
            gone(&pid, END_LIMIT + END_GRACE + Duration::from_secs(3)),
            "{pid} is still running"
        );
    }
}

#[test]
fn nothing_a_write_hands_over_but_names_words_and_numbers_is_taken() {
    use nucleate::housekeeping::Job;
    let boxed = boxed();
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let worktree = with_work(rig, "fix-login");
    commit(&worktree, "a.txt", "work");
    let tree = "0123456789abcdef0123456789abcdef01234567";
    let mut marked: Vec<(String, &str, Value)> = Vec::new();
    for field in ["repo", "failure", "refused", "error"] {
        let mut entry = json!({ "repo": "web", "ok": false, "failure": "git" });
        entry[field] = json!(mark(field));
        marked.push((
            format!("fetched.{field}"),
            "refresh",
            write_answer("refresh", "ok", "fetched", json!([entry])),
        ));
    }
    for field in [
        "repo", "folder", "branch", "action", "reason", "tree", "ref", "worktree",
    ] {
        let mut entry = json!({
            "repo": "web", "folder": "fix-login", "branch": "workspace/fix-login",
            "action": "snapshot", "tree": tree, "ref": "refs/wip/workspace-fix-login",
        });
        entry[field] = json!(mark(field));
        marked.push((
            format!("snapshots.{field}"),
            "snapshot",
            write_answer("snapshot", "ok", "snapshots", json!([entry])),
        ));
    }
    for field in ["repo", "failure", "detail"] {
        let mut entry = json!({ "repo": "web", "pushed": 1 });
        entry[field] = json!(mark(field));
        marked.push((
            format!("pushes.{field}"),
            "push",
            write_answer("push", "ok", "pushes", json!([entry])),
        ));
    }
    let mut answer = write_answer("push", "ok", "pushes", json!([]));
    answer["why"] = json!(mark("why"));
    marked.push(("why".into(), "push", answer));
    for (field, op, answer) in &marked {
        assert!(answer.to_string().contains(MARK), "{field}");
        answers(rig, op, answer);
        match *op {
            "refresh" => assert!(job_inside(&boxed, Job::Refresh, None).is_err(), "{field}"),
            "snapshot" => assert!(job_inside(&boxed, Job::Snapshot, None).is_err(), "{field}"),
            _ => {
                let result = act_inside(&boxed, &action("fix-login", "push", false), None);
                assert_eq!(result["outcome"], "failed", "{field}");
            }
        }
    }
    // A removal's answer, where one can be asked for: whether a process is
    // in the folder is read from `/proc`.
    if cfg!(target_os = "linux") {
        let removal = |id: &str| {
            json!({
                "folder": "fix-login", "outcome": "backed_up", "backed_up": [{
                    "repo": "web", "branch": "workspace/fix-login", "tree": tree,
                    "ref": format!("refs/wip/workspace-fix-login-{id}"),
                }],
            })
        };
        type Change = Box<dyn Fn(&mut Value)>;
        let mut fields: Vec<(&str, Change)> = Vec::new();
        for field in ["folder", "outcome", "why", "repo", "note"] {
            fields.push((
                field,
                Box::new(move |r: &mut Value| r[field] = json!(mark(field))),
            ));
        }
        for field in ["repo", "ref", "branch", "tree"] {
            fields.push((
                field,
                Box::new(move |r: &mut Value| r["backed_up"][0][field] = json!(mark(field))),
            ));
        }
        fields.push((
            "holding.repo",
            Box::new(|r: &mut Value| {
                r["outcome"] = json!("kept");
                r["why"] = json!("unsaved");
                r["holding"] = json!([{ "repo": mark("holding"), "uncommitted": true }]);
            }),
        ));
        for (field, change) in fields {
            let asked = action("fix-login", "clean_up", true);
            let mut entry = removal(asked["id"].as_str().unwrap());
            change(&mut entry);
            assert!(entry.to_string().contains(MARK), "{field}");
            answers(
                rig,
                "remove",
                &write_answer("remove", "ok", "removals", json!([entry])),
            );
            let result = act_inside(&boxed, &asked, None);
            assert_eq!(result["outcome"], "failed", "removals.{field}: {result}");
            assert!(!result.to_string().contains(MARK), "removals.{field}");
        }
    }
    gate::tick(rig);
    assert!(!mock.to(contract::HEARTBEATS).is_empty());
    gate::assert_clean(rig, &mock);
}

#[test]
fn what_a_real_write_reads_and_sends_stays_in_the_container() {
    use nucleate::housekeeping::Job;
    let boxed = boxed_in(Rig::under(MARK));
    let rig = &boxed.rig;
    rig.configure_in_container("worktree_actions = true\n");
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let worktree = with_work(rig, "fix-login");
    let content = format!("{MARK} in a file the user wrote\n");
    fs::write(worktree.join("notes.txt"), &content).unwrap();
    age(&worktree.join("notes.txt"));
    fs::write(worktree.join(format!("{MARK}.txt")), &content).unwrap();
    age(&worktree.join(format!("{MARK}.txt")));
    git(&worktree, &["add", "notes.txt"]);
    git(&worktree, &["commit", "-q", "-m", &mark("subject")]);

    let report = job_inside(&boxed, Job::Snapshot, None).unwrap();
    assert_eq!(report["worktrees"][0]["action"], "snapshot", "{report}");
    assert!(!report.to_string().contains(MARK), "{report}");
    let report = job_inside(&boxed, Job::Refresh, None).unwrap();
    assert_eq!(report["repos"][0]["ok"], true, "{report}");
    let result = act_inside(&boxed, &action("fix-login", "push", false), None);
    assert_eq!(
        result["detail"], "Pushed 1 commit in web. web has changes that are not committed.",
        "{result}"
    );
    assert!(!report.to_string().contains(MARK), "{report}");
    boxed.reporter().tick(&rig.settings(), SystemTime::now());
    gate::tick(rig);
    gate::tick(rig);
    assert!(!mock.to(contract::HEARTBEATS).is_empty());
    for op in ["census", "snapshot", "refresh", "push"] {
        assert!(asked_inside(rig, op), "{op}");
    }
    gate::assert_clean(rig, &mock);
}

#[test]
fn a_host_environment_sharing_a_containers_nucleus_has_its_git_run_in_that_container() {
    let boxed = boxed();
    let rig = &boxed.rig;
    // `client-a` on the host, first, and `box-b` in the container, over the
    // same nucleus.
    rig.configure(&format!(
        "[[environment]]\nname = \"box-b\"\nroot = \"{}\"\ncheck = \"true\"\n\
         unit = \"agent-env-client-a.service\"\n",
        rig.root.display()
    ));
    rig.session("fix-login");
    let flat = rig.flatten();
    let out = boxed.job("refresh", &["--env", "client-a"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)[0]["repos"][0]["ok"], true);
    assert!(!flat.exists());
    assert!(rig
        .podman_calls()
        .iter()
        .any(|call| call.contains(" repos --env box-b ") && call.contains("--do refresh")));

    // The count is the container's answer: here one that says a worktree
    // holds what the nucleus's does not.
    let mut said = answer();
    said["folders"][0]["worktrees"][0]["facts"]["uncommitted"] = json!(true);
    let file = rig.home.dir.join("count.json");
    fs::write(&file, said.to_string()).unwrap();
    rig.container_does(
        "agent-env-client-a",
        &format!("exec cat '{}'", file.display()),
    );
    let counts = rig.home.state_dir().join("unsaved.json");
    rig.looker();
    let out = gate::daemon_until(rig, Some(&boxed.path), "a count", &|| counts.is_file());
    let counts: Value = serde_json::from_str(&fs::read_to_string(&counts).unwrap()).unwrap();
    let nucleus = &counts["nuclei"][0];
    assert_eq!(
        nucleus["environments"],
        json!(["client-a", "box-b"]),
        "{nucleus}"
    );
    assert_eq!(nucleus["unsaved"]["uncommitted_repos"], 1, "{nucleus}");
    assert_eq!(boxed.host_git(), Vec::<String>::new(), "{}", stderr(&out));
}

#[test]
fn a_container_drivers_worktrees_are_counted_inside_it_with_no_git_on_the_host() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let workspaces = rig.home.dir.join("orca").join("workspaces");
    fs::create_dir_all(&workspaces).unwrap();
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"orca\"\nroot = \"{}\"\ndriver = \"orca\"\ncheck = \"true\"\n\
         unit = \"agent-env-client-a.service\"\ndriver_cli = \"/nonexistent\"\ndriver_home = \"{}\"\n",
        workspaces.display(),
        rig.home.dir.join("orca-data").display(),
    ));
    let project = rig.root.join("projects").join("web");
    git(
        &rig.home.dir,
        &[
            "clone",
            "-q",
            rig.upstream.to_str().unwrap(),
            project.to_str().unwrap(),
        ],
    );
    let task = workspaces.join("task");
    git(
        &project,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task",
            task.to_str().unwrap(),
        ],
    );
    fs::write(task.join("notes.txt"), "not committed\n").unwrap();
    let counts = rig.home.state_dir().join("unsaved.json");
    rig.looker();
    let out = gate::daemon_until(rig, Some(&boxed.path), "a count", &|| counts.is_file());
    let counts: Value = serde_json::from_str(&fs::read_to_string(&counts).unwrap()).unwrap();
    let orca = counts["nuclei"]
        .as_array()
        .unwrap()
        .iter()
        .find(|nucleus| nucleus["environments"] == json!(["orca"]))
        .cloned()
        .unwrap();
    assert_eq!(orca["unsaved"]["uncommitted_repos"], 1, "{orca}");
    assert!(rig
        .podman_calls()
        .iter()
        .any(|call| call.contains(" repos --env orca ") && call.ends_with(" --driver")));
    assert_eq!(boxed.host_git(), Vec::<String>::new(), "{}", stderr(&out));
}
