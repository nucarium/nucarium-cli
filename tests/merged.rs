//! Work that was merged on GitHub: a squash-merged session branch counts as
//! saved, whatever it tracks and however far along the pull request its tip
//! is, and housekeeping cleans its folder up at the next run.

mod common;

use common::{git, run, stderr, Mock, Reply, Request, Rig};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

/// A session whose branch made three commits, was pushed, opened as pull
/// request #7, squash-merged into `main` and deleted on the remote, and
/// fetched since. Returns its worktree and its three commits, oldest first.
fn merged_session(rig: &Rig, name: &str, track_default: bool) -> (PathBuf, Vec<String>) {
    rig.as_github("octo/web");
    let worktree = rig.session(name);
    let mut commits = Vec::new();
    for n in 1..=3 {
        fs::write(worktree.join(format!("change-{n}.txt")), format!("{n}\n")).unwrap();
        rig.home.git(&worktree, &["add", "."]);
        rig.home
            .git(&worktree, &["commit", "-q", "-m", &format!("change {n}")]);
        commits.push(rig.home.git(&worktree, &["rev-parse", "HEAD"]));
    }
    let branch = rig.home.git(&worktree, &["branch", "--show-current"]);
    if track_default {
        // As sessions made before branches stopped tracking the default
        // branch, and pushed without `-u`.
        rig.home.git(
            &worktree,
            &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
        );
        rig.home
            .git(&worktree, &["branch", "--set-upstream-to", "origin/main"]);
    } else {
        rig.push(&worktree);
    }
    git(&rig.upstream, &["merge", "-q", "--squash", &branch]);
    git(&rig.upstream, &["commit", "-q", "-m", "fix login (#7)"]);
    git(&rig.upstream, &["branch", "-q", "-D", &branch]);
    rig.home.git(
        &rig.root.join(".repos/web.git"),
        &["fetch", "-q", "--prune", "origin"],
    );
    (worktree, commits)
}

/// GitHub as it answers for that branch: every commit asked about is listed
/// with pull request #7, merged, whose head is the last of `commits` and
/// whose commits are `commits`.
fn github(commits: &[String]) -> Mock {
    let commits = commits.to_vec();
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

fn remove_session(rig: &Rig, github: &Mock, name: &str) -> std::process::Output {
    run(
        rig.nucleate()
            .env("NUCLEATE_GITHUB_API_URL", &github.url)
            .args(["remove-session", name]),
        "",
    )
}

fn session_dir(rig: &Rig, name: &str) -> PathBuf {
    rig.root.join(".worktrees").join(name)
}

#[test]
fn a_new_session_branch_tracks_nothing() {
    let rig = Rig::new();
    let worktree = rig.session("fresh");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["rev-parse", "--symbolic-full-name", "@{upstream}"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "it tracks {:?}", out.stdout);
}

#[test]
fn a_squash_merged_branch_whose_tip_is_behind_the_pull_requests_head_is_saved() {
    let rig = Rig::new();
    let (worktree, commits) = merged_session(&rig, "behind", false);
    rig.home
        .git(&worktree, &["reset", "-q", "--hard", "HEAD~2"]);
    assert_eq!(rig.home.git(&worktree, &["rev-parse", "HEAD"]), commits[0]);
    let github = github(&commits);
    let out = remove_session(&rig, &github, "behind");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!session_dir(&rig, "behind").exists());
    // GitHub was asked which commits the pull request holds.
    assert!(github
        .requests()
        .iter()
        .any(|request| request.path.starts_with("/repos/octo/web/pulls/7/commits")));
}

#[test]
fn a_squash_merged_branch_that_tracks_the_default_branch_is_saved() {
    let rig = Rig::new();
    let (_, commits) = merged_session(&rig, "tracking", true);
    let github = github(&commits);
    let out = remove_session(&rig, &github, "tracking");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!session_dir(&rig, "tracking").exists());
}

#[test]
fn a_commit_after_the_merged_ones_is_unsaved() {
    let rig = Rig::new();
    let (worktree, commits) = merged_session(&rig, "more", false);
    fs::write(worktree.join("later.txt"), "after the merge\n").unwrap();
    rig.home.git(&worktree, &["add", "."]);
    rig.home.git(&worktree, &["commit", "-q", "-m", "later"]);
    let github = github(&commits);
    let out = remove_session(&rig, &github, "more");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("HEAD is not on any remote branch or merged pull request"),
        "{}",
        stderr(&out)
    );
    assert!(worktree.join("later.txt").is_file());
}

#[test]
fn a_merged_pull_request_that_does_not_hold_the_commit_does_not_save_it() {
    let rig = Rig::new();
    let (worktree, mut commits) = merged_session(&rig, "elsewhere", false);
    rig.home
        .git(&worktree, &["reset", "-q", "--hard", "HEAD~2"]);
    // GitHub names pull request #7, but its commits are other ones.
    commits[0] = "0".repeat(40);
    let github = github(&commits);
    let out = remove_session(&rig, &github, "elsewhere");
    assert!(!out.status.success());
    assert!(session_dir(&rig, "elsewhere").is_dir());
}

// ---- housekeeping ----------------------------------------------------------

/// Whether a process is inside a session is read from /proc, so what cleanup
/// removes can only be shown where there is one.
#[cfg(target_os = "linux")]
mod housekeeping {
    use super::common::stdout_json;
    use super::*;

    fn cleanup(rig: &Rig, github: &Mock) -> Value {
        let out = run(
            rig.nucleated()
                .env("NUCLEATE_GITHUB_API_URL", &github.url)
                .args(["housekeeping", "run", "cleanup"]),
            "",
        );
        assert!(out.status.success(), "{}", stderr(&out));
        stdout_json(&out)[0].clone()
    }

    fn verdict(report: &Value, name: &str) -> Value {
        report["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["session"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} is not in {report}"))
    }

    /// The age limit is left at its default: work that was merged a moment ago
    /// goes all the same.
    #[test]
    fn cleanup_removes_merged_work_at_once_whether_its_tip_is_the_head_or_behind_it() {
        let rig = Rig::new();
        let (behind, commits) = merged_session(&rig, "behind", false);
        rig.home.git(&behind, &["reset", "-q", "--hard", "HEAD~2"]);
        // A second session on another branch, merged the same way, at the head.
        let other = rig.session("at-head");
        rig.home
            .git(&other, &["reset", "-q", "--hard", &commits[2]]);
        let fresh = rig.session("fresh");
        let github = github(&commits);

        let report = cleanup(&rig, &github);
        assert_eq!(verdict(&report, "behind")["action"], "removed", "{report}");
        assert_eq!(verdict(&report, "at-head")["action"], "removed", "{report}");
        // A session that committed nothing still waits out the age limit.
        assert_eq!(
            verdict(&report, "fresh"),
            json!({ "session": "fresh", "action": "kept", "reason": "it changed too recently" })
        );
        assert!(!session_dir(&rig, "behind").exists());
        assert!(!session_dir(&rig, "at-head").exists());
        assert!(fresh.is_dir());
    }

    #[test]
    fn cleanup_keeps_merged_work_with_a_later_commit_or_an_uncommitted_change() {
        let rig = Rig::new();
        rig.configure("[housekeeping]\ncleanup_min_age = \"0s\"\n");
        let (more, commits) = merged_session(&rig, "more", false);
        fs::write(more.join("later.txt"), "after the merge\n").unwrap();
        rig.home.git(&more, &["add", "."]);
        rig.home.git(&more, &["commit", "-q", "-m", "later"]);
        let dirty = rig.session("dirty");
        rig.home
            .git(&dirty, &["reset", "-q", "--hard", &commits[2]]);
        fs::write(dirty.join("unsaved.txt"), "only here\n").unwrap();
        let github = github(&commits);

        let report = cleanup(&rig, &github);
        assert_eq!(
            verdict(&report, "more"),
            json!({
                "session": "more", "action": "kept",
                "reason": "web: HEAD is not on any remote branch or merged pull request",
            })
        );
        assert_eq!(
            verdict(&report, "dirty"),
            json!({ "session": "dirty", "action": "kept", "reason": "web: uncommitted changes" })
        );
        assert!(more.join("later.txt").is_file());
        assert!(dirty.join("unsaved.txt").is_file());
    }

    #[test]
    fn cleanup_keeps_merged_work_while_its_agent_session_is_live() {
        let rig = Rig::new();
        let (worktree, commits) = merged_session(&rig, "live", false);
        let hook = rig.hook(
            "UserPromptSubmit",
            &json!({ "session_id": "s1", "cwd": worktree }),
        );
        assert!(hook.status.success());
        let mut daemon =
            nucleate::daemon::Daemon::new(rig.settings(), nucleate::daemon::Timings::default());
        daemon.tick(std::time::Instant::now());
        let github = github(&commits);

        let report = cleanup(&rig, &github);
        assert_eq!(
            verdict(&report, "live")["reason"],
            "its agent session has not ended"
        );
        assert!(worktree.is_dir());
    }
}
