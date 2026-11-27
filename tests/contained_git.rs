//! Git for an environment in a container runs inside its container, never on
//! the host (D15).
//!
//! On the host, `git` here is a stand-in that writes down that it was run
//! and fails. `podman` is a stand-in that runs the box's own `nucleated
//! repos` on this machine, as it would run in the container, with the real
//! git and a local remote. What the daemon learns of the container's
//! repositories it learns from that answer: the counts of unsaved work, the
//! session folders it reports, what a session left behind, and what cleanup
//! would remove. And when the answer is not one, it knows nothing, and says
//! nothing in its place.

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
fn cleanup_decides_from_inside_and_github_and_removes_nothing_from_the_host() {
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

    // Run for real, nothing is removed yet, and it says so.
    let out = boxed.job("cleanup", &[], Some(&github));
    let report = &stdout_json(&out)[0];
    assert_eq!(verdict(report, "merged")["action"], "failed");
    assert!(rig.root.join(".worktrees").join("merged").is_dir());
    assert_eq!(boxed.host_git(), Vec::<String>::new());
}

#[test]
fn no_job_runs_git_on_the_host_for_a_container_and_none_removes_or_pushes_anything() {
    let boxed = boxed();
    let rig = &boxed.rig;
    three_folders(rig);
    let dirty = rig.root.join(".worktrees").join("dirty");
    fs::write(dirty.join("web").join("more.txt"), "more\n").unwrap();
    for job in ["refresh", "cleanup", "snapshot"] {
        let out = boxed.job(job, &[], None);
        let report = &stdout_json(&out)[0];
        if job == "cleanup" {
            assert!(out.status.success(), "{}", stderr(&out));
            for entry in report["sessions"].as_array().unwrap() {
                assert_ne!(entry["action"], "removed", "{report}");
            }
        }
        // Nothing the report says is a path.
        assert!(
            !report.to_string().contains(rig.root.to_str().unwrap()),
            "{report}"
        );
    }
    assert_eq!(boxed.host_git(), Vec::<String>::new());
    for name in ["dirty", "ahead", "pushed"] {
        assert!(rig.root.join(".worktrees").join(name).is_dir(), "{name}");
    }
    assert_eq!(git(&rig.upstream, &["for-each-ref", "refs/wip"]), "");
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
    let spooled = rig.spooled().len();
    for (what, does) in hostile {
        rig.container_does("agent-env-client-a", &does);
        let hangs = does.contains("sleep 60");
        let seen = heard(&boxed, &session, limit);
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
    assert_eq!(boxed.host_git(), Vec::<String>::new());

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
        boxed.reporter().tick(&rig.settings(), SystemTime::now());
        let counts = fs::read_to_string(rig.home.state_dir().join("unsaved.json")).unwrap();
        assert!(!counts.contains(MARK), "{field}: {counts}");
        let nucleus = &serde_json::from_str::<Value>(&counts).unwrap()["nuclei"][0];
        assert!(nucleus["unsaved"].is_null(), "{field}: {nucleus}");
        assert!(nucleus.get("worktrees").is_none(), "{field}: {nucleus}");
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
    assert_eq!(boxed.host_git(), Vec::<String>::new());
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
