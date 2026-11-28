//! The daemon runs git on the host, in repositories agent sessions can
//! write. Whatever a session writes into a bare repo's config or a
//! worktree's git pointers, the daemon runs no program that names: it
//! refuses the repo, logs it, shows it in `nucleated status` and leaves the
//! repo out of what it counts, and every job carries on.
//!
//! Each crafted case points at a program that leaves a marker file when it
//! runs, and sets the repo up so that the daemon's own work would run it if
//! git were let read the repo as it stands: a `git status` for the
//! file-system monitor, a fetch for the ssh command, an `add` for a filter, a
//! push for a hook.

mod common;

use common::{git, run, stderr, stdout_json, Rig};
use nucleate::reporter::Reporter;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn marker_program(rig: &Rig) -> (PathBuf, PathBuf) {
    common::marker_program(&rig.home)
}

fn bare(rig: &Rig) -> PathBuf {
    rig.bare()
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

/// A session whose worktree holds work the snapshot job would push.
fn session_with_work(rig: &Rig) -> PathBuf {
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "work\n").unwrap();
    age(&worktree.join("notes.txt"));
    worktree
}

/// What the daemon does with the nucleus: fetch, snapshot and count, and
/// then what the box shows. The jobs' reports, their logs, and the status.
struct Seen {
    refresh: Value,
    snapshot: Value,
    logs: String,
    status: Value,
    text: String,
}

fn job(rig: &Rig, name: &str) -> (Value, String) {
    let out = run(rig.nucleated().args(["housekeeping", "run", name]), "");
    assert!(out.status.success(), "{}", stderr(&out));
    (stdout_json(&out)[0].clone(), stderr(&out))
}

fn everything(rig: &Rig) -> Seen {
    let (refresh, mut logs) = job(rig, "refresh");
    let (snapshot, more) = job(rig, "snapshot");
    logs.push_str(&more);
    Reporter::default().tick(&rig.settings(), SystemTime::now());
    let out = run(rig.nucleated().args(["status", "--json"]), "");
    assert!(out.status.success(), "{}", stderr(&out));
    let status = stdout_json(&out);
    let text = String::from_utf8(run(rig.nucleated().arg("status"), "").stdout).unwrap();
    Seen {
        refresh,
        snapshot,
        logs,
        status,
        text,
    }
}

/// The repo was refused for `reason` everywhere the daemon looked, nothing
/// that names a program was run, and the jobs and the status still worked.
fn assert_refused(rig: &Rig, marker: &Path, reason: &str, planted: &str) {
    let seen = everything(rig);
    assert!(!marker.exists(), "a program the repo names was run");

    assert_eq!(seen.refresh["repos"][0]["repo"], "web", "{}", seen.refresh);
    assert_eq!(seen.refresh["repos"][0]["action"], "refused");
    assert_eq!(seen.refresh["repos"][0]["reason"], reason);
    assert_eq!(
        seen.snapshot["worktrees"][0]["action"], "refused",
        "{}",
        seen.snapshot
    );
    assert_eq!(seen.snapshot["worktrees"][0]["reason"], reason);
    assert!(git(&rig.upstream, &["for-each-ref", "refs/wip"]).is_empty());

    let environment = &seen.status["environments"][0];
    assert_eq!(
        environment["git_refused"],
        json!([{ "repo": "web", "reason": reason }])
    );
    // What a heartbeat would say: no count, rather than one that leaves the
    // repo out.
    assert!(environment["unsaved"].is_null(), "{environment}");
    assert!(
        seen.text.contains(&format!("web ({reason})")),
        "{}",
        seen.text
    );

    // Logged once per run, by name, and with nothing the repo's config says.
    let lines: Vec<&str> = seen
        .logs
        .lines()
        .filter(|line| line.contains("refused") && line.contains("web"))
        .collect();
    assert_eq!(lines.len(), 2, "one line per job:\n{}", seen.logs);
    assert!(!seen.logs.contains(planted), "{}", seen.logs);
}

#[test]
fn a_file_system_monitor_in_the_config_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    session_with_work(&rig);
    let value = program.to_str().unwrap();
    git(&bare(&rig), &["config", "core.fsmonitor", value]);
    assert_refused(&rig, &marker, "config", value);
}

#[test]
fn a_hooks_folder_in_the_config_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    session_with_work(&rig);
    let hooks = rig.home.dir.join("planted-hooks");
    fs::create_dir_all(&hooks).unwrap();
    for hook in ["pre-push", "reference-transaction", "post-index-change"] {
        fs::copy(&program, hooks.join(hook)).unwrap();
    }
    let value = hooks.to_str().unwrap();
    git(&bare(&rig), &["config", "core.hooksPath", value]);
    // The key itself is allowed, and overridden. What refuses this repo is
    // the other key that comes with it.
    git(
        &bare(&rig),
        &["config", "core.fsmonitor", program.to_str().unwrap()],
    );
    assert_refused(&rig, &marker, "config", value);
}

#[test]
fn an_ssh_command_in_the_config_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    session_with_work(&rig);
    let value = program.to_str().unwrap();
    git(&bare(&rig), &["config", "core.sshCommand", value]);
    git(
        &bare(&rig),
        &[
            "config",
            "remote.origin.url",
            "ssh://nobody.invalid/web.git",
        ],
    );
    assert_refused(&rig, &marker, "config", value);
}

#[test]
fn a_remote_that_is_a_folder_with_hooks_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    session_with_work(&rig);
    // A repository whose hooks a push to it would run, where a session
    // could have made one, and the session's repo pointed at it.
    let planted = rig.home.dir.join("planted.git");
    let value = planted.to_str().unwrap();
    git(&rig.home.dir, &["init", "-q", "--bare", value]);
    for hook in ["pre-receive", "update", "post-receive"] {
        fs::copy(&program, planted.join("hooks").join(hook)).unwrap();
    }
    for url in [value.to_string(), format!("file://{value}")] {
        git(&bare(&rig), &["config", "remote.origin.url", &url]);
        assert_refused(&rig, &marker, "config", value);
    }
    assert!(git(&planted, &["for-each-ref"]).is_empty());
}

#[test]
fn in_report_only_mode_a_push_to_a_folder_runs_none_of_its_hooks() {
    let rig = Rig::new();
    rig.configure("[git]\nvetting = \"report\"\n");
    let (program, marker) = marker_program(&rig);
    session_with_work(&rig);
    let planted = rig.home.dir.join("planted.git");
    let value = planted.to_str().unwrap();
    git(&rig.home.dir, &["init", "-q", "--bare", value]);
    for hook in ["pre-receive", "update", "post-receive"] {
        fs::copy(&program, planted.join("hooks").join(hook)).unwrap();
    }
    git(&bare(&rig), &["config", "remote.origin.url", value]);

    let seen = everything(&rig);
    // Git ran, and would not fetch from the folder or push to it.
    assert!(!marker.exists(), "a hook of the folder pushed to was run");
    assert_eq!(seen.refresh["repos"][0]["ok"], false, "{}", seen.refresh);
    assert!(git(&planted, &["for-each-ref"]).is_empty());
    let environment = &seen.status["environments"][0];
    assert_eq!(
        environment["git_refused"],
        json!([{ "repo": "web", "reason": "config" }])
    );
}

#[test]
fn a_filter_in_the_config_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    let worktree = session_with_work(&rig);
    fs::write(worktree.join(".gitattributes"), "*.txt filter=planted\n").unwrap();
    age(&worktree.join(".gitattributes"));
    let value = program.to_str().unwrap();
    git(&bare(&rig), &["config", "filter.planted.clean", value]);
    assert_refused(&rig, &marker, "config", value);
}

#[test]
fn an_include_in_the_config_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    session_with_work(&rig);
    let included = rig.home.dir.join("planted.gitconfig");
    fs::write(
        &included,
        format!("[core]\n\tfsmonitor = {}\n", program.display()),
    )
    .unwrap();
    let value = included.to_str().unwrap();
    git(&bare(&rig), &["config", "include.path", value]);
    assert_refused(&rig, &marker, "config", value);
}

/// A repository outside the nucleus whose config runs the program.
fn outside_repo(rig: &Rig, program: &Path) -> PathBuf {
    let outside = rig.home.dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    git(&outside, &["init", "-q", "-b", "main"]);
    git(&outside, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &outside,
        &["config", "core.fsmonitor", program.to_str().unwrap()],
    );
    outside
}

#[test]
fn a_worktree_that_points_outside_the_nucleus_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    let worktree = session_with_work(&rig);
    let outside = outside_repo(&rig, &program);
    let value = format!("gitdir: {}\n", outside.join(".git").display());
    fs::write(worktree.join(".git"), &value).unwrap();
    assert_refused_worktree(&rig, &marker, "pointer", value.trim());
}

#[test]
fn a_worktree_whose_common_folder_is_outside_the_nucleus_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    session_with_work(&rig);
    let outside = outside_repo(&rig, &program);
    let commondir = bare(&rig).join("worktrees/web/commondir");
    assert!(commondir.is_file());
    let value = outside.join(".git").display().to_string();
    fs::write(&commondir, format!("{value}\n")).unwrap();
    assert_refused_worktree(&rig, &marker, "pointer", &value);
}

#[test]
fn a_worktree_whose_common_folder_pointer_is_a_link_is_refused_and_not_read() {
    let rig = Rig::new();
    let (_, marker) = marker_program(&rig);
    session_with_work(&rig);
    // A link in place of the pointer, to a file outside the nucleus that
    // says just what the pointer said: read through the link it would pass.
    let commondir = bare(&rig).join("worktrees/web/commondir");
    let outside = rig.home.dir.join("elsewhere-commondir");
    fs::copy(&commondir, &outside).unwrap();
    fs::remove_file(&commondir).unwrap();
    std::os::unix::fs::symlink(&outside, &commondir).unwrap();
    assert_refused_worktree(&rig, &marker, "pointer", outside.to_str().unwrap());
}

/// As [`assert_refused`], for a repo whose own config is clean and one of
/// whose worktrees points somewhere git must not be let go: the repo is
/// fetched, and the worktree is neither snapshotted nor counted.
fn assert_refused_worktree(rig: &Rig, marker: &Path, reason: &str, planted: &str) {
    let seen = everything(rig);
    assert!(!marker.exists(), "a program the repo names was run");
    assert_eq!(seen.refresh["repos"][0]["ok"], true, "{}", seen.refresh);
    assert_eq!(
        seen.snapshot["worktrees"][0]["action"], "refused",
        "{}",
        seen.snapshot
    );
    assert_eq!(seen.snapshot["worktrees"][0]["reason"], reason);
    let environment = &seen.status["environments"][0];
    assert_eq!(
        environment["git_refused"],
        json!([{ "repo": "web", "reason": reason }])
    );
    assert!(environment["unsaved"].is_null(), "{environment}");
    assert!(
        seen.text.contains(&format!("web ({reason})")),
        "{}",
        seen.text
    );
    assert!(!seen.logs.contains(planted), "{}", seen.logs);
}

#[test]
fn a_worktree_holding_a_submodule_with_a_config_of_its_own_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig);
    let worktree = session_with_work(&rig);
    // A repository checked out inside the worktree and committed to it as a
    // submodule. Its own config, which nobody checks, names a filter for its
    // files; `git add` in the worktree starts git again in there.
    let nested = worktree.join("vendored");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q", "-b", "main"]);
    fs::write(nested.join(".gitattributes"), "* filter=planted\n").unwrap();
    fs::write(nested.join("lib.txt"), "library\n").unwrap();
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-q", "-m", "vendored"]);
    git(&worktree, &["add", "vendored"]);
    git(&worktree, &["commit", "-q", "-m", "add a submodule"]);
    let value = program.to_str().unwrap();
    git(&nested, &["config", "filter.planted.clean", value]);
    // Changed since its index saw it, so git must read it through the filter.
    fs::write(nested.join("lib.txt"), "library, edited\n").unwrap();
    assert_refused_worktree(&rig, &marker, "submodule", value);
}

#[test]
fn a_worktree_whose_index_names_a_submodule_not_checked_out_is_refused() {
    let rig = Rig::new();
    let (_, marker) = marker_program(&rig);
    let worktree = session_with_work(&rig);
    // Nothing is there yet. Something can be by the time git looks.
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    git(
        &worktree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},vendored"),
        ],
    );
    assert_refused_worktree(&rig, &marker, "submodule", &head);
}

#[test]
fn a_repo_with_tracking_branches_and_the_keys_git_writes_passes() {
    let rig = Rig::new();
    let (_, marker) = marker_program(&rig);
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("a.txt"), "work\n").unwrap();
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-q", "-m", "work"]);
    rig.push(&worktree);
    // A second session's branch, made the way a sibling worktree is.
    rig.session("second");
    let bare = bare(&rig);
    git(
        &bare,
        &[
            "config",
            "branch.workspace/fix-login.vscode-merge-base",
            "origin/main",
        ],
    );
    git(&bare, &["config", "gc.auto", "0"]);
    git(&bare, &["config", "extensions.worktreeConfig", "true"]);
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    age(&worktree.join("notes.txt"));

    let seen = everything(&rig);
    assert!(!marker.exists());
    assert_eq!(
        seen.refresh["repos"],
        json!([{ "repo": "web", "ok": true }])
    );
    let actions: Vec<&Value> = seen.snapshot["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| &entry["action"])
        .collect();
    assert!(actions.contains(&&json!("snapshot")), "{}", seen.snapshot);
    assert!(!actions.contains(&&json!("refused")), "{}", seen.snapshot);
    let environment = &seen.status["environments"][0];
    assert_eq!(environment["git_refused"], json!([]));
    assert_eq!(
        environment["unsaved"]["uncommitted_repos"], 1,
        "{environment}"
    );
    assert_eq!(environment["unsaved"]["unpushed_commits"], 0);
    assert!(!seen.text.contains("refused"), "{}", seen.text);
    assert!(!seen.logs.contains("refused"), "{}", seen.logs);
}

#[test]
fn in_report_only_mode_a_refusal_is_shown_and_git_runs_with_nothing_the_repo_names() {
    let rig = Rig::new();
    rig.configure("[git]\nvetting = \"report\"\n");
    let (program, marker) = marker_program(&rig);
    let worktree = session_with_work(&rig);
    let hooks = rig.home.dir.join("planted-hooks");
    fs::create_dir_all(&hooks).unwrap();
    for hook in ["pre-push", "reference-transaction", "post-index-change"] {
        fs::copy(&program, hooks.join(hook)).unwrap();
    }
    git(
        &bare(&rig),
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    git(
        &bare(&rig),
        &["config", "core.fsmonitor", program.to_str().unwrap()],
    );
    git(
        &bare(&rig),
        &["config", "core.attributesFile", "/nonexistent"],
    );
    let _ = worktree;

    let seen = everything(&rig);
    // The two keys that name a program are switched off on every call.
    assert!(!marker.exists(), "a program the repo names was run");
    // Git ran: the repo was fetched, the work snapshotted and counted.
    assert_eq!(seen.refresh["repos"][0]["ok"], true, "{}", seen.refresh);
    assert_eq!(
        seen.snapshot["worktrees"][0]["action"], "snapshot",
        "{}",
        seen.snapshot
    );
    let environment = &seen.status["environments"][0];
    assert_eq!(
        environment["unsaved"]["uncommitted_repos"], 1,
        "{environment}"
    );
    // And the operator is shown what enforcing would refuse.
    assert_eq!(
        environment["git_refused"],
        json!([{ "repo": "web", "reason": "config" }])
    );
    assert!(seen.text.contains("web (config)"), "{}", seen.text);
    assert!(seen.logs.contains("web"), "{}", seen.logs);
}

#[test]
fn an_unknown_vetting_mode_is_an_error_that_names_the_key() {
    let rig = Rig::new();
    rig.configure("[git]\nvetting = \"off\"\n");
    let out = run(rig.nucleated().args(["housekeeping", "run", "refresh"]), "");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("git.vetting"), "{}", stderr(&out));
}
