//! What holds for every agent and every driver, whichever adapter is asked:
//! whose hook a hook is, where it lands, and that housekeeping never touches
//! what a driver owns.

mod common;

use common::gate::{self, MARK};
use common::{git, run, stderr, stdout_json, Rig};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const SESSION: &str = "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a";

fn hook(rig: &Rig, args: &[&str], cwd: &Path) {
    let payload = json!({ "session_id": SESSION, "cwd": cwd });
    let out = run(rig.nucleated().arg("hook").args(args), &payload.to_string());
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

/// A box whose first environment runs another agent, in a nucleus of its
/// own, ahead of `client-a`.
fn two_agents() -> (Rig, PathBuf) {
    let rig = Rig::new();
    let other = rig.home.dir.join("other-agent");
    std::fs::create_dir_all(&other).unwrap();
    rig.home.write_config(&format!(
        "state_dir = \"{}\"\ncache_dir = \"{}\"\n\
         [[environment]]\nname = \"other\"\nroot = \"{}\"\nagent = \"codex\"\ncheck = \"true\"\n\
         [[environment]]\nname = \"client-a\"\nroot = \"{}\"\ncheck = \"true\"\n",
        rig.home.state_dir().display(),
        rig.home.cache_dir().display(),
        other.display(),
        rig.root.display(),
    ));
    (rig, other)
}

#[test]
fn a_hook_that_names_no_agent_is_the_default_agents_wherever_its_environment_is_listed() {
    let (rig, other) = two_agents();
    // Claude Code's hooks name no agent. Another agent's environment listed
    // first does not get to read them.
    hook(&rig, &["Stop"], &rig.root);
    let spooled = rig.spooled();
    assert_eq!(spooled.len(), 1);
    assert_eq!(spooled[0]["event"]["env_id"], "client-a");
    assert_eq!(spooled[0]["event"]["type"], "stop");
    // Nor do they land in that environment by being run from its folder.
    hook(&rig, &["Stop"], &other);
    let spooled = rig.spooled();
    assert_eq!(spooled.len(), 2);
    assert_eq!(spooled[1]["event"]["env_id"], "client-a");
    // Named outright, the agent is the one named.
    hook(&rig, &["Stop", "--agent", "claude-code"], &rig.root);
    assert_eq!(rig.spooled().len(), 3);
}

#[test]
fn a_hook_for_an_agent_there_is_no_adapter_for_records_nothing() {
    let (rig, _) = two_agents();
    hook(&rig, &["Stop", "--agent", "another"], &rig.root);
    hook(&rig, &["Stop", "--agent", ""], &rig.root);
    assert!(rig.spooled().is_empty());
}

#[test]
fn the_hooks_block_is_printed_for_the_agent_asked_for() {
    let (rig, _) = two_agents();
    let block = |args: &[&str]| run(rig.nucleated().args(["hooks", "print"]).args(args), "");
    // With nothing named, the default agent's, as it has always been.
    let plain = stdout_json(&block(&[]));
    assert_eq!(
        plain["hooks"]["Stop"][0]["hooks"][0]["command"],
        "nucleated hook Stop"
    );
    assert_eq!(stdout_json(&block(&["--agent", "claude-code"])), plain);
    let named = stdout_json(&block(&["--env", "client-a"]));
    assert_eq!(
        named["hooks"]["Stop"][0]["hooks"][0]["command"],
        "nucleated hook Stop --env client-a"
    );
    // An environment's own agent is asked for its block.
    assert!(block(&["--env", "other"]).status.success());
    let refused = block(&["--agent", "another"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("claude-code"),
        "{}",
        stderr(&refused)
    );
}

/// A box with a nucleus, and beside it the folder of a driver that owns its
/// worktrees, holding one worktree the driver made from the nucleus's own
/// bare repository, with work in it that exists nowhere else.
fn beside_a_driver() -> (Rig, PathBuf, PathBuf) {
    let rig = Rig::new();
    let workspaces = rig.home.dir.join("orca").join("workspaces");
    std::fs::create_dir_all(&workspaces).unwrap();
    let bare = rig.bare();
    let worktree = workspaces.join("task-1");
    git(
        &rig.home.dir,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            "-b",
            "orca/task-1",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    rig.configure(&format!(
        "[[environment]]\nname = \"orca\"\nroot = \"{}\"\ndriver = \"orca\"\ncheck = \"true\"\n",
        workspaces.display()
    ));
    (rig, bare, worktree)
}

/// Everything about a repository and a worktree that a job could change.
fn fingerprint(rig: &Rig, bare: &Path, worktree: &Path) -> Vec<String> {
    let bare_dir = bare.to_str().unwrap();
    let in_bare = |args: &[&str]| {
        let mut all = vec!["--git-dir", bare_dir];
        all.extend(args);
        git(&rig.home.dir, &all)
    };
    let mut files: Vec<String> = common::files_under(worktree)
        .into_iter()
        .map(|(path, bytes)| format!("{} {}", path.display(), bytes.len()))
        .collect();
    files.sort();
    let mut all = vec![
        in_bare(&["for-each-ref"]),
        in_bare(&["worktree", "list", "--porcelain"]),
        in_bare(&["count-objects"]),
        git(&rig.upstream, &["for-each-ref"]),
    ];
    if worktree.is_dir() {
        all.push(git(worktree, &["status", "--porcelain"]));
    }
    all.extend(files);
    all
}

fn housekeeping(rig: &Rig, args: &[&str]) -> std::process::Output {
    run(rig.nucleated().args(["housekeeping", "run"]).args(args), "")
}

#[test]
fn housekeeping_never_removes_or_changes_anything_under_a_drivers_own_folders() {
    let (rig, bare, worktree) = beside_a_driver();
    let before = fingerprint(&rig, &bare, &worktree);

    // Every job, over every environment the schedule would run it for.
    for job in ["refresh", "cleanup", "snapshot"] {
        let out = housekeeping(&rig, &[job]);
        assert!(out.status.success(), "{job}: {}", stderr(&out));
        let reports = stdout_json(&out);
        let environments: Vec<&Value> = reports
            .as_array()
            .unwrap()
            .iter()
            .map(|report| &report["environment"])
            .collect();
        assert_eq!(
            environments,
            [&json!("client-a")],
            "{job} ran for the driver"
        );
        if job == "snapshot" {
            // The nucleus's own snapshot sees the driver's worktree, through
            // the repository they share, and passes it by.
            let passed: Vec<&Value> = reports[0]["worktrees"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["reason"] == "owned by a driver")
                .collect();
            assert_eq!(passed.len(), 1);
            assert_eq!(passed[0]["action"], "skipped");
        }
    }
    // Asked for by hand, for the driver's environment, each job is refused.
    for job in ["refresh", "cleanup", "snapshot"] {
        let out = housekeeping(&rig, &[job, "--env", "orca"]);
        assert!(!out.status.success(), "{job} ran for the driver");
        assert!(out.stdout_contains("leaves them alone"), "{job}");
    }
    assert_eq!(fingerprint(&rig, &bare, &worktree), before);

    // The driver's folder goes away, a disk not mounted say. Git's record of
    // the worktree is the driver's too, and is not pruned.
    let aside = worktree.with_file_name("task-1.aside");
    std::fs::rename(&worktree, &aside).unwrap();
    let listed = || {
        git(
            &rig.home.dir,
            &[
                "--git-dir",
                bare.to_str().unwrap(),
                "worktree",
                "list",
                "--porcelain",
            ],
        )
    };
    let recorded = listed();
    assert!(recorded.contains("task-1"));
    assert!(housekeeping(&rig, &["cleanup"]).status.success());
    assert_eq!(listed(), recorded);
    std::fs::rename(&aside, &worktree).unwrap();
    assert_eq!(fingerprint(&rig, &bare, &worktree), before);
}

/// A box with a nucleus, and a worktree somebody else made from the
/// nucleus's own bare repository in a folder no config names: as a driver
/// that makes a worktree per thread does when it is pointed at a session's
/// checkout. It holds work that exists nowhere else, written long enough ago
/// that a snapshot would not pass it by as still being written.
fn beside_a_worktree_no_config_names() -> (Rig, PathBuf, PathBuf) {
    let rig = Rig::new();
    let bare = rig.bare();
    let worktree = rig.home.dir.join("threads").join("thread-1");
    add_worktree(&rig, &bare, &worktree, "thread/one");
    let notes = worktree.join("notes.txt");
    std::fs::write(&notes, "not committed\n").unwrap();
    let a_while_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
    let file = std::fs::File::options().write(true).open(&notes).unwrap();
    file.set_modified(a_while_ago).unwrap();
    (rig, bare, worktree)
}

fn add_worktree(rig: &Rig, bare: &Path, worktree: &Path, branch: &str) {
    git(
        &rig.home.dir,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            worktree.to_str().unwrap(),
            "origin/main",
        ],
    );
}

fn worktrees_recorded(rig: &Rig, bare: &Path) -> String {
    git(
        &rig.home.dir,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "list",
            "--porcelain",
        ],
    )
}

#[test]
fn housekeeping_leaves_alone_a_worktree_it_did_not_make_wherever_its_folder_is() {
    let (rig, bare, worktree) = beside_a_worktree_no_config_names();
    // The nucleus has a session of its own with unsaved work too, so the
    // snapshot job has something that is its own to do.
    let session = rig.session("fix-login");
    std::fs::write(session.join("notes.txt"), "the session's\n").unwrap();
    let before = fingerprint(&rig, &bare, &worktree);

    for job in ["refresh", "cleanup", "snapshot"] {
        let out = housekeeping(&rig, &[job]);
        assert!(out.status.success(), "{job}: {}", stderr(&out));
        if job == "snapshot" {
            let reports = stdout_json(&out);
            let entries = reports[0]["worktrees"].as_array().unwrap();
            let passed: Vec<&Value> = entries
                .iter()
                .filter(|entry| {
                    entry["worktree"]
                        .as_str()
                        .is_some_and(|at| at.ends_with("thread-1"))
                })
                .collect();
            assert_eq!(passed.len(), 1, "{reports}");
            assert_eq!(passed[0]["action"], "skipped");
            assert_eq!(passed[0]["reason"], "not under this nucleus's .worktrees");
        }
    }
    // Nothing was written into it, nothing pushed from it, and the
    // repository holds no object or ref made for it.
    assert_eq!(fingerprint(&rig, &bare, &worktree), before);

    // Its folder goes away for a while. Git's record of it is not the
    // box's to drop.
    let aside = worktree.with_file_name("thread-1.aside");
    std::fs::rename(&worktree, &aside).unwrap();
    let recorded = worktrees_recorded(&rig, &bare);
    assert!(recorded.contains("thread-1"));
    assert!(housekeeping(&rig, &["cleanup"]).status.success());
    assert_eq!(worktrees_recorded(&rig, &bare), recorded);
    std::fs::rename(&aside, &worktree).unwrap();
    assert_eq!(fingerprint(&rig, &bare, &worktree), before);
}

#[test]
fn nucleate_itself_never_drops_or_removes_a_worktree_it_did_not_make() {
    let (rig, bare, worktree) = beside_a_worktree_no_config_names();
    // `nucleate` reads no daemon config, so no list of folders protects
    // anything here.
    let session = |name: &str| rig.session(name).parent().unwrap().to_path_buf();
    let (first, second) = (session("s1"), session("s2"));
    let aside = worktree.with_file_name("thread-1.aside");
    std::fs::rename(&worktree, &aside).unwrap();

    let removed = run(rig.nucleate().args(["remove-session", "s1"]), "");
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!first.exists());
    assert!(worktrees_recorded(&rig, &bare).contains("thread-1"));
    // Claude Code's `WorktreeRemove` hook, at the end of an agent session.
    let hook_remove = |path: &Path| {
        run(
            rig.nucleate().arg("hook-remove"),
            &json!({ "worktree_path": path }).to_string(),
        )
    };
    let removed = hook_remove(&second);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!second.exists());
    assert!(worktrees_recorded(&rig, &bare).contains("thread-1"));

    // The folder comes back, and still is what it was.
    std::fs::rename(&aside, &worktree).unwrap();
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "?? notes.txt");

    // Asked to remove a worktree that is not a session's, even one with
    // nothing unsaved in it, the hook refuses.
    let clean = rig.home.dir.join("threads").join("thread-2");
    add_worktree(&rig, &bare, &clean, "thread/two");
    let refused = hook_remove(&clean);
    assert!(!refused.status.success());
    assert!(clean.join("README.md").is_file());
    assert!(worktrees_recorded(&rig, &bare).contains("thread-2"));
}

trait Printed {
    fn stdout_contains(&self, text: &str) -> bool;
}

impl Printed for std::process::Output {
    fn stdout_contains(&self, text: &str) -> bool {
        String::from_utf8_lossy(&self.stdout).contains(text)
    }
}

#[test]
fn a_nucleus_and_a_drivers_folder_that_overlap_are_refused_before_anything_runs() {
    let (rig, _, _) = beside_a_driver();
    rig.configure(&format!(
        "[[environment]]\nname = \"orca\"\nroot = \"{}\"\ndriver = \"orca\"\ncheck = \"true\"\n",
        rig.root.join(".worktrees").display()
    ));
    let out = housekeeping(&rig, &["cleanup"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("orca"), "{}", stderr(&out));
    assert!(run(rig.nucleated().arg("status"), "").status.code() != Some(0));
}

#[test]
fn a_hook_run_inside_a_drivers_folder_lands_in_the_drivers_environment() {
    let (rig, _, worktree) = beside_a_driver();
    hook(&rig, &["Stop"], &worktree);
    let spooled = rig.spooled();
    assert_eq!(spooled.len(), 1);
    assert_eq!(spooled[0]["event"]["env_id"], "orca");
    assert_eq!(spooled[0]["event"]["branch"], "orca/task-1");
    // It is no nucleus session, so there is no session folder to keep.
    assert!(spooled[0].get("workspace").is_none());
}

#[test]
fn a_hook_inside_a_worktree_named_for_a_task_names_the_repository_and_never_the_task() {
    let (rig, bare, _) = beside_a_driver();
    // A driver that makes a worktree per task names the folder after the
    // task. The repository is not on GitHub, so its name is a folder's: the
    // folder it lives in, `web.git`, and never the worktree's own.
    let task = rig
        .home
        .dir
        .join("orca")
        .join("workspaces")
        .join(format!("{MARK}-fix-the-login-page"));
    git(
        &rig.home.dir,
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            "-b",
            "orca/task-2",
            task.to_str().unwrap(),
        ],
    );
    let inside = task.join(format!("{MARK}_src"));
    std::fs::create_dir_all(&inside).unwrap();
    hook(&rig, &["Stop"], &inside);
    let spooled = rig.spooled();
    assert_eq!(spooled.len(), 1);
    assert_eq!(spooled[0]["event"]["env_id"], "orca");
    assert_eq!(spooled[0]["event"]["repos"], json!(["web"]));
    assert_eq!(spooled[0]["event"]["branch"], "orca/task-2");
    assert_eq!(
        gate::leaks_on_disk(&rig.home.state_dir()),
        Vec::<String>::new()
    );
}

#[test]
fn a_driver_environment_is_shown_and_checked_as_what_it_is() {
    let (rig, _, _) = beside_a_driver();
    let status = stdout_json(&run(rig.nucleated().args(["status", "--json"]), ""));
    let orca = &status["environments"][1];
    assert_eq!(orca["name"], "orca");
    assert_eq!(orca["driver"]["name"], "orca");
    // Nothing has asked the driver yet, and the view says so.
    assert_eq!(orca["driver"]["condition"], Value::Null);
    assert_eq!(status["environments"][0]["driver"], Value::Null);
    let text = run(rig.nucleated().arg("status"), "");
    let text = String::from_utf8_lossy(&text.stdout).to_string();
    assert!(text.contains("driver orca: not asked yet"), "{text}");

    // Its folder is checked for being a folder, not for being a nucleus.
    let check = stdout_json(&run(rig.nucleated().arg("check"), ""));
    let named = |name: &str| {
        check["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .cloned()
    };
    assert_eq!(named("orca: folder").unwrap()["ok"], true);
    assert!(named("orca: nucleus").is_none());
    assert_eq!(named("client-a: nucleus").unwrap()["ok"], true);
}
