//! `nucleate init` and `nucleate clone`: a nucleus in two commands.

mod common;

use common::{git, remote_of, run, stderr, stdout_json, Home, Rig};
use std::fs;
use std::path::PathBuf;

#[test]
fn init_creates_a_nucleus_and_names_it_in_the_config() {
    let home = Home::new();
    let root = home.dir.join("work");
    fs::create_dir_all(&root).unwrap();
    let out = run(
        home.nucleate()
            .args(["init", "--root", root.to_str().unwrap()]),
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let printed = stdout_json(&out);
    assert_eq!(printed["root"], root.to_str().unwrap());
    assert_eq!(printed["created"], true);
    let config = home.dir.join(".config/nucleate/config.toml");
    assert_eq!(printed["config"], config.to_str().unwrap());
    for folder in [".repos", ".worktrees", ".seeds"] {
        assert!(root.join(folder).is_dir(), "{folder}");
    }
    // The config now names the root, so nucleate works from anywhere.
    let listed = run(home.nucleate().arg("available"), "");
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(stdout_json(&listed), serde_json::json!([]));
}

#[test]
fn init_again_changes_nothing() {
    let home = Home::new();
    let first = run(home.nucleate().arg("init"), "");
    assert_eq!(stdout_json(&first)["created"], true);
    let config = home.dir.join(".config/nucleate/config.toml");
    let before = fs::read_to_string(&config).unwrap();
    let second = run(home.nucleate().arg("init"), "");
    assert!(second.status.success());
    assert_eq!(stdout_json(&second)["created"], false);
    assert_eq!(stdout_json(&second)["root"], stdout_json(&first)["root"]);
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
}

#[test]
fn init_defaults_to_the_current_folder() {
    let home = Home::new();
    let out = run(home.nucleate().arg("init"), "");
    assert_eq!(stdout_json(&out)["root"], home.dir.to_str().unwrap());
    assert!(home.dir.join(".repos").is_dir());
}

#[test]
fn init_keeps_the_settings_already_in_the_config() {
    let home = Home::new();
    let config = home.dir.join(".config/nucleate/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "branch_prefix = \"agent\"\n").unwrap();
    let out = run(home.nucleate().arg("init"), "");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("branch_prefix = \"agent\""));
    assert!(text.contains("root = "));
    // Still one valid file: the prefix it set is the one sessions use.
    run(home.nucleate().args(["create-session", "s1"]), "");
    let manifest = fs::read_to_string(home.dir.join(".worktrees/s1/.workspace.json")).unwrap();
    assert!(manifest.contains("agent/s1"));
}

#[test]
fn a_second_nucleus_does_not_take_over_the_config() {
    let home = Home::new();
    let (first, second) = (home.dir.join("one"), home.dir.join("two"));
    run(
        home.nucleate()
            .args(["init", "--root", first.to_str().unwrap()]),
        "",
    );
    let out = run(
        home.nucleate()
            .args(["init", "--root", second.to_str().unwrap()]),
        "",
    );
    assert_eq!(stdout_json(&out)["created"], true);
    assert!(second.join(".repos").is_dir());
    let config = fs::read_to_string(home.dir.join(".config/nucleate/config.toml")).unwrap();
    assert!(config.contains("one"));
    assert!(!config.contains("two"));
}

#[test]
fn clone_makes_a_repo_available_for_sessions() {
    let rig = Rig::new();
    let bare = rig.bare();
    assert!(bare.is_dir());
    // Remote-tracking refs, so a fetch never rewrites a session's branch.
    assert_eq!(
        git(&bare, &["config", "--get", "remote.origin.fetch"]),
        "+refs/heads/*:refs/remotes/origin/*"
    );
    assert_eq!(
        git(
            &bare,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]
        ),
        "origin/main"
    );
    assert_eq!(git(&bare, &["rev-parse", "--is-bare-repository"]), "true");

    let listed = stdout_json(&run(rig.nucleate().arg("available"), ""));
    assert_eq!(listed[0]["repo"], "web");

    // A session can be built on it straight away, from the remote's head.
    let worktree = rig.session("fix-login");
    assert_eq!(
        fs::read_to_string(worktree.join("README.md")).unwrap(),
        "web\n"
    );
    assert_eq!(
        git(&worktree, &["branch", "--show-current"]),
        "workspace/fix-login"
    );

    // And a later push upstream reaches it on refresh.
    fs::write(rig.upstream.join("NEW.md"), "new\n").unwrap();
    git(&rig.upstream, &["add", "."]);
    git(&rig.upstream, &["commit", "-q", "-m", "more"]);
    let refreshed = run(rig.nucleate().arg("refresh"), "");
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));
    assert_eq!(
        git(&bare, &["rev-parse", "refs/remotes/origin/main"]),
        git(&rig.upstream, &["rev-parse", "HEAD"])
    );
}

#[test]
fn clone_prints_the_entry_available_would() {
    let rig = Rig::new();
    let other = rig.home.dir.join("upstream/api");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "trunk"]);
    fs::write(other.join("a"), "a\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "init"]);
    let out = run(
        rig.nucleate()
            .args(["clone", &remote_of(&other), "--name", "backend"]),
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let printed = stdout_json(&out);
    assert_eq!(printed["repo"], "backend");
    assert_eq!(printed["label"], "backend");
    assert_eq!(
        printed["bare"],
        rig.root.join(".repos/_/backend.git").to_str().unwrap()
    );
    assert_eq!(printed["already_present"], false);
    let mut entry = printed.clone();
    entry.as_object_mut().unwrap().remove("already_present");
    let listed = stdout_json(&run(rig.nucleate().arg("available"), ""));
    assert!(listed.as_array().unwrap().contains(&entry));
    // The remote's own default branch is the one sessions start from.
    let session = run(rig.nucleate().args(["create-session", "s1", "backend"]), "");
    assert!(session.status.success(), "{}", stderr(&session));
    assert!(rig.root.join(".worktrees/s1/backend/a").is_file());
}

#[test]
fn clone_names_the_repo_from_the_url() {
    let rig = Rig::new();
    let named = |url: &str| {
        let out = run(rig.nucleate().args(["clone", url]), "");
        stderr(&out)
    };
    // Each fails at the fetch, after the name is settled; the name is in the path it tried.
    assert!(named("https://example.invalid/your-org/api.git").contains("api.git"));
    assert!(named("git@example.invalid:your-org/console").contains("console.git"));
    // A failed clone leaves nothing behind.
    assert!(!rig.root.join(".repos/_/api.git").exists());
    assert!(!rig.root.join(".repos/_/console.git").exists());
}

#[test]
fn clone_finds_a_repo_already_there_and_refuses_one_it_cannot_name() {
    let rig = Rig::new();
    // The same repo again: found, and nothing changes.
    let again = run(rig.nucleate().args(["clone", &rig.remote()]), "");
    assert!(again.status.success(), "{}", stderr(&again));
    let printed = stdout_json(&again);
    assert_eq!(printed["already_present"], true);
    assert_eq!(printed["bare"], rig.bare().to_str().unwrap());
    // Another repo of the same name with no owner to tell them apart.
    let elsewhere = rig.home.dir.join("elsewhere/web");
    fs::create_dir_all(&elsewhere).unwrap();
    git(&elsewhere, &["init", "-q", "-b", "main"]);
    let other = run(rig.nucleate().args(["clone", &remote_of(&elsewhere)]), "");
    assert_eq!(other.status.code(), Some(1));
    assert!(stderr(&other).contains("already available"));
    let unnamed = run(
        rig.nucleate().args(["clone", "https://example.invalid/"]),
        "",
    );
    assert_eq!(unnamed.status.code(), Some(1));
    assert!(stderr(&unnamed).contains("--name"));
}

#[test]
fn clone_takes_no_folder_or_program_on_this_machine_for_a_remote() {
    let rig = Rig::new();
    let folder = rig.home.dir.join("upstream/api");
    fs::create_dir_all(&folder).unwrap();
    git(&folder, &["init", "-q", "-b", "main"]);
    git(&folder, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let marker = rig.home.dir.join("a-program-ran");
    for url in [
        folder.to_str().unwrap().to_string(),
        format!("file://{}", folder.display()),
        format!("ext::sh -c 'touch {}'", marker.display()),
        "http://example.invalid/your-org/api.git".to_string(),
        "git://example.invalid/your-org/api.git".to_string(),
    ] {
        let out = run(rig.nucleate().args(["clone", &url, "--name", "api"]), "");
        assert_eq!(out.status.code(), Some(1), "{url}");
        assert!(stderr(&out).contains("https:// or ssh"), "{}", stderr(&out));
    }
    assert!(!rig.root.join(".repos/_/api.git").exists());
    assert!(!marker.exists(), "a program a remote names was run");
}

#[test]
fn a_worktree_asked_for_inside_a_session_is_made_and_removed_by_the_hooks() {
    // A repo made by `nucleate clone` holds no local branch until a session
    // makes one.
    let rig = Rig::new();
    let checkout = rig.session("s1");
    let payload = serde_json::json!({ "name": "helper", "cwd": checkout }).to_string();
    let out = run(rig.nucleate().arg("hook-create"), &payload);
    assert!(out.status.success(), "{}", stderr(&out));
    let sibling = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    assert_eq!(
        sibling,
        rig.root.join(".worktrees/s1/.subworktrees/helper-web")
    );
    assert!(sibling.join("README.md").is_file());

    let payload = serde_json::json!({ "worktree_path": sibling }).to_string();
    let out = run(rig.nucleate().arg("hook-remove"), &payload);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!sibling.exists());
    assert!(checkout.join("README.md").is_file());
}
