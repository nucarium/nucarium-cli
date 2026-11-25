//! `nucleate` runs git on the host too: from Claude Code's worktree hooks,
//! from its MCP server, and from the commands a person runs. Whatever a
//! session writes into a bare repo's config or a worktree's git pointers,
//! `nucleate` runs no program that names: it refuses the repo with an error
//! that says so, and leaves the session as it was. A repo holding only what
//! git and `nucleate` write works as before.
//!
//! Each crafted case points at a program that leaves a marker file when it
//! runs, and sets the repo up so that the command would run it if git were
//! let read the repo as it stands.

mod common;

use common::{git, marker_program, run, stderr, stdout_json, Mock, Reply, Rig};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

fn bare(rig: &Rig) -> PathBuf {
    rig.bare()
}

fn nucleate(rig: &Rig, args: &[&str], stdin: &str) -> Output {
    run(rig.nucleate().args(args), stdin)
}

/// Run `nucleate` and expect it to succeed.
fn succeeds(rig: &Rig, args: &[&str], stdin: &str) -> Output {
    let out = nucleate(rig, args, stdin);
    assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    out
}

/// Run `nucleate`, expect it to fail because the `web` repo was refused, and
/// return what it said.
fn refused(rig: &Rig, args: &[&str], stdin: &str) -> String {
    let out = nucleate(rig, args, stdin);
    assert!(!out.status.success(), "{args:?} succeeded");
    let said = stderr(&out);
    assert!(said.contains("refused repo web"), "{args:?}: {said}");
    said
}

/// Claude Code's `WorktreeCreate` payload, from a session started in `cwd`.
fn create_payload(name: &str, cwd: &Path) -> String {
    json!({ "hook_event_name": "WorktreeCreate", "name": name, "cwd": cwd }).to_string()
}

fn remove_payload(path: &Path) -> String {
    json!({ "hook_event_name": "WorktreeRemove", "worktree_path": path }).to_string()
}

/// Commit a file to the upstream, so that the next fetch has a ref to move.
fn commit_upstream(rig: &Rig, file: &str, text: &str) {
    fs::write(rig.upstream.join(file), text).unwrap();
    git(&rig.upstream, &["add", "."]);
    git(&rig.upstream, &["commit", "-q", "-m", file]);
}

/// What `nucleate refresh` printed and its exit code.
fn refresh(rig: &Rig) -> (Value, Option<i32>) {
    let out = nucleate(rig, &["refresh"], "");
    (stdout_json(&out), out.status.code())
}

fn assert_refresh_refused(rig: &Rig) {
    let (results, code) = refresh(rig);
    assert_eq!(
        results,
        json!([{ "repo": "web", "ok": false, "action": "refused", "reason": "config" }])
    );
    assert_eq!(code, Some(1));
}

#[test]
fn a_hooks_folder_in_the_config_runs_no_hook_and_every_command_still_works() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    let hooks = rig.home.dir.join("planted-hooks");
    fs::create_dir_all(&hooks).unwrap();
    for hook in [
        "post-checkout",
        "reference-transaction",
        "post-index-change",
        "pre-auto-gc",
    ] {
        fs::copy(&program, hooks.join(hook)).unwrap();
    }
    // The key is one git and tools beside it write, and it is overridden on
    // every call, so the repo is not refused for it: no hook in it runs.
    git(
        &bare(&rig),
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    commit_upstream(&rig, "b.txt", "b\n");

    let (results, code) = refresh(&rig);
    assert_eq!(results, json!([{ "repo": "web", "ok": true }]));
    assert_eq!(code, Some(0));
    let created = succeeds(
        &rig,
        &["hook-create", "web"],
        &create_payload("fix-login", &rig.root),
    );
    let session = rig.root.join(".worktrees/fix-login");
    assert_eq!(
        String::from_utf8_lossy(&created.stdout).trim(),
        session.to_str().unwrap()
    );
    assert!(session.join("web/b.txt").is_file());
    succeeds(&rig, &["status", "--session", "fix-login"], "");
    succeeds(&rig, &["hook-remove"], &remove_payload(&session));

    assert!(!session.exists());
    assert!(!marker.exists(), "a hook the repo names was run");
}

#[test]
fn a_file_system_monitor_in_the_config_is_refused_and_the_session_is_kept() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    let worktree = rig.session("fix-login");
    let session = worktree.parent().unwrap().to_path_buf();
    git(
        &bare(&rig),
        &["config", "core.fsmonitor", program.to_str().unwrap()],
    );

    refused(&rig, &["hook-remove"], &remove_payload(&session));
    refused(&rig, &["status", "--session", "fix-login"], "");
    refused(&rig, &["remove", "web", "--session", "fix-login"], "");
    refused(&rig, &["remove-session", "fix-login"], "");

    assert!(worktree.join("README.md").is_file());
    assert!(!marker.exists(), "a program the repo names was run");
}

#[test]
fn an_ssh_command_in_the_config_is_refused_by_refresh_and_add() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    succeeds(&rig, &["create-session", "fix-login"], "");
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

    assert_refresh_refused(&rig);
    let said = refused(&rig, &["add", "web", "--session", "fix-login"], "");
    assert!(!said.contains(value), "{said}");

    assert!(!rig.root.join(".worktrees/fix-login/web").exists());
    assert!(!marker.exists(), "a program the repo names was run");
}

#[test]
fn a_filter_in_the_config_is_refused_before_a_session_is_made() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    // A file the filter would be run on as a new worktree is checked out.
    fs::write(
        rig.upstream.join(".gitattributes"),
        "*.txt filter=planted\n",
    )
    .unwrap();
    commit_upstream(&rig, "notes.txt", "notes\n");
    succeeds(&rig, &["refresh"], "");
    for key in ["filter.planted.smudge", "filter.planted.clean"] {
        git(&bare(&rig), &["config", key, program.to_str().unwrap()]);
    }

    let out = nucleate(
        &rig,
        &["hook-create", "web"],
        &create_payload("fix-login", &rig.root),
    );
    assert!(!out.status.success());
    // The session is told why, and is given no folder to work in.
    assert!(
        stderr(&out).contains("refused repo web"),
        "{}",
        stderr(&out)
    );
    assert!(out.stdout.is_empty());
    assert!(!rig.root.join(".worktrees/fix-login").exists());
    assert!(!marker.exists(), "a program the repo names was run");
}

#[test]
fn an_include_in_the_config_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    let included = rig.home.dir.join("planted.gitconfig");
    fs::write(
        &included,
        format!("[core]\n\tfsmonitor = {}\n", program.display()),
    )
    .unwrap();
    git(
        &bare(&rig),
        &["config", "include.path", included.to_str().unwrap()],
    );

    refused(&rig, &["create-session", "fix-login", "web"], "");
    assert_refresh_refused(&rig);

    assert!(!rig.root.join(".worktrees/fix-login").exists());
    assert!(!marker.exists(), "a program the repo names was run");
}

/// A stand-in git server that asks for credentials, as a private repo does.
fn asks_for_credentials() -> Mock {
    Mock::start(|_| Reply::json(401, json!({})).header("WWW-Authenticate", "Basic realm=\"git\""))
}

#[test]
fn a_credential_helper_in_the_config_is_refused_and_nothing_is_fetched() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    let server = asks_for_credentials();
    git(
        &bare(&rig),
        &[
            "config",
            "remote.origin.url",
            &format!("{}/web.git", server.url),
        ],
    );
    git(
        &bare(&rig),
        &["config", "credential.helper", program.to_str().unwrap()],
    );

    assert_refresh_refused(&rig);
    refused(&rig, &["create-session", "fix-login", "web"], "");

    assert!(server.requests().is_empty(), "the repo was fetched");
    assert!(!marker.exists(), "a program the repo names was run");
}

#[test]
fn a_url_rewrite_in_the_config_is_refused_and_nothing_is_fetched() {
    let rig = Rig::new();
    let server = asks_for_credentials();
    let rewrite = format!("url.{}/.insteadOf", server.url);
    git(
        &bare(&rig),
        &["config", &rewrite, rig.upstream.to_str().unwrap()],
    );

    assert_refresh_refused(&rig);
    refused(&rig, &["create-session", "fix-login", "web"], "");

    assert!(
        server.requests().is_empty(),
        "the rewritten address was fetched"
    );
}

#[test]
fn a_worktree_that_points_outside_the_nucleus_is_refused() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    let worktree = rig.session("fix-login");
    let session = worktree.parent().unwrap().to_path_buf();
    let outside = rig.home.dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    git(&outside, &["init", "-q", "-b", "main"]);
    git(&outside, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &outside,
        &["config", "core.fsmonitor", program.to_str().unwrap()],
    );
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", outside.join(".git").display()),
    )
    .unwrap();
    let inside = worktree.join("src");
    fs::create_dir_all(&inside).unwrap();

    // A worktree asked for from inside the session's checkout.
    let out = nucleate(&rig, &["hook-create"], &create_payload("second", &inside));
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("refused repo web"),
        "{}",
        stderr(&out)
    );
    assert!(out.stdout.is_empty());
    assert!(!session.join(".subworktrees").exists());
    refused(&rig, &["hook-remove"], &remove_payload(&session));
    refused(&rig, &["status", "--session", "fix-login"], "");

    assert!(worktree.join("README.md").is_file());
    assert!(!marker.exists(), "a program the repo names was run");
}

#[test]
fn the_mcp_server_refuses_to_add_a_repo_whose_config_names_a_program() {
    let rig = Rig::new();
    let (program, marker) = marker_program(&rig.home);
    succeeds(&rig, &["create-session", "fix-login"], "");
    let session = rig.root.join(".worktrees/fix-login");
    git(
        &bare(&rig),
        &["config", "core.fsmonitor", program.to_str().unwrap()],
    );
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_repo","arguments":{"repo":"web"}}}"#,
        "\n",
    );
    let out = run(rig.nucleate().current_dir(&session).arg("mcp"), input);
    assert!(out.status.success(), "{}", stderr(&out));
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("refused repo web"),
        "{reply}"
    );
    assert!(!session.join("web").exists());
    assert!(!marker.exists(), "a program the repo names was run");
}

#[test]
fn a_repo_nucleate_cloned_and_worked_in_passes_its_own_check_and_the_daemons() {
    // The repo's remote reads as one on GitHub, and the box's own git
    // settings send it to the upstream folder: what the user's own config
    // says is still followed, as their credential helper is.
    let rig = Rig::new();
    rig.as_github("your-org/web");
    commit_upstream(&rig, "b.txt", "b\n");
    let (results, code) = refresh(&rig);
    assert_eq!(results, json!([{ "repo": "web", "ok": true }]));
    assert_eq!(code, Some(0));

    let created = succeeds(
        &rig,
        &["hook-create", "web"],
        &create_payload("fix-login", &rig.root),
    );
    let session = PathBuf::from(String::from_utf8_lossy(&created.stdout).trim());
    let worktree = session.join("web");
    assert!(worktree.join("b.txt").is_file());
    fs::write(worktree.join("c.txt"), "c\n").unwrap();
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-q", "-m", "c"]);
    rig.push(&worktree);
    // What a worktree's own settings add is allowed too.
    git(
        &bare(&rig),
        &["config", "extensions.worktreeConfig", "true"],
    );

    let status = stdout_json(&succeeds(&rig, &["status", "--session", "fix-login"], ""));
    assert_eq!(status["repos"]["web"]["unsafe_reasons"], json!([]));
    assert_eq!(status["repos"]["web"]["dirty"], false);

    // The daemon's check passes the repo as nucleate left it.
    let daemon = run(rig.nucleated().args(["housekeeping", "run", "refresh"]), "");
    assert!(daemon.status.success(), "{}", stderr(&daemon));
    assert_eq!(
        stdout_json(&daemon)[0]["repos"],
        json!([{ "repo": "web", "ok": true }])
    );

    succeeds(&rig, &["hook-remove"], &remove_payload(&session));
    assert!(!session.exists());
    assert_eq!(
        stdout_json(&succeeds(&rig, &["available"], "")),
        json!([{ "repo": "web", "label": "your-org/web", "bare": bare(&rig) }])
    );
}
