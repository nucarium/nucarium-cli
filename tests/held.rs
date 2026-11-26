//! What the reporter reads on the host for an environment in a container,
//! where the container writes: the session folders and manifests behind an
//! outcome, the worktrees a nucleus's repos list, and the worktrees a driver
//! keeps. Links, files too big to be manifests, and paths out of the
//! container's folders are not followed, and git is not run there.

mod common;

use common::{git, Rig, CLIENT_A_CONTAINER_UNIT};
use nucleate::envelope::{unix_ms, Draft, Envelope, EventType};
use nucleate::reporter::Reporter;
use nucleate::sessions::Store;
use nucleate::spool::{Record, Spool};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// `client-a` in its container.
fn contained() -> Rig {
    let rig = Rig::new();
    rig.configure_in_container("");
    rig
}

/// A session in `client-a` that worked in the session folder `workspace`
/// and has stopped, as the daemon has it once it applied the hook's records.
fn stopped_in(rig: &Rig, workspace: &str) -> nucleate::sessions::Session {
    let spool = Spool::new(&rig.container_spool());
    for (n, kind) in [EventType::UserPromptSubmit, EventType::Stop]
        .into_iter()
        .enumerate()
    {
        let at = SystemTime::now() + Duration::from_millis(n as u64);
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
    common::gate::tick(rig);
    let session = Store::new(&rig.home.state_dir())
        .load("client-a", "s1")
        .unwrap();
    assert_eq!(session.workspace.as_deref(), Some(workspace));
    session
}

fn outcome(rig: &Rig, session: &nucleate::sessions::Session) -> Option<Draft> {
    let settings = rig.settings();
    let environment = settings.environment("client-a").unwrap();
    Reporter::default().outcome(&settings, environment, session)
}

/// Move a session folder out of the nucleus, as another environment's.
fn elsewhere(rig: &Rig, name: &str) -> PathBuf {
    let other = rig.home.dir.join("workspace-other").join(name);
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::rename(rig.root.join(".worktrees").join(name), &other).unwrap();
    other
}

#[test]
fn an_outcome_reads_a_session_folder_of_the_nucleus_itself() {
    let rig = contained();
    let worktree = rig.session("fix-login");
    fs::write(worktree.join("notes.txt"), "not committed\n").unwrap();
    let session = stopped_in(&rig, "fix-login");
    let told = outcome(&rig, &session).unwrap();
    assert_eq!(told.branch.as_deref(), Some("workspace/fix-login"));
    assert_eq!(told.uncommitted_repos, Some(1));
}

#[test]
fn a_link_in_place_of_a_session_folder_is_not_followed() {
    let rig = contained();
    let other = rig.session("theirs");
    fs::write(other.join("notes.txt"), "not committed\n").unwrap();
    let theirs = elsewhere(&rig, "theirs");
    std::os::unix::fs::symlink(&theirs, rig.root.join(".worktrees").join("x")).unwrap();
    let session = stopped_in(&rig, "x");
    assert!(outcome(&rig, &session).is_none());
}

#[test]
fn a_link_in_place_of_the_sessions_folder_is_not_followed() {
    let rig = contained();
    rig.session("theirs");
    let sessions = rig.root.join(".worktrees");
    let moved = rig.home.dir.join("workspace-other").join(".worktrees");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&sessions, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &sessions).unwrap();
    let session = stopped_in(&rig, "theirs");
    assert!(outcome(&rig, &session).is_none());
}

#[test]
fn a_manifest_that_is_a_link_or_too_big_is_not_read() {
    let rig = contained();
    let folder = rig.root.join(".worktrees").join("x");
    fs::create_dir_all(&folder).unwrap();
    let manifest = r#"{"name":"x","branch":"workspace/x","repos":{}}"#;

    // Past what a manifest can be, it is not read, however it parses.
    let padded = format!("{manifest}{}", " ".repeat(2 * 1024 * 1024));
    fs::write(folder.join(".workspace.json"), padded).unwrap();
    let session = stopped_in(&rig, "x");
    assert!(outcome(&rig, &session).is_none());

    // A link to one somewhere else is not read either.
    let theirs = rig.home.dir.join("their-manifest.json");
    fs::write(&theirs, manifest).unwrap();
    fs::remove_file(folder.join(".workspace.json")).unwrap();
    std::os::unix::fs::symlink(&theirs, folder.join(".workspace.json")).unwrap();
    assert!(outcome(&rig, &session).is_none());

    // The same manifest, in its place, is.
    fs::remove_file(folder.join(".workspace.json")).unwrap();
    fs::write(folder.join(".workspace.json"), manifest).unwrap();
    let told = outcome(&rig, &session).unwrap();
    assert_eq!(told.branch.as_deref(), Some("workspace/x"));
}

#[test]
fn a_manifest_that_lists_a_worktree_outside_its_folder_gives_no_outcome() {
    let rig = contained();
    let other = rig.session("theirs");
    fs::write(other.join("notes.txt"), "not committed\n").unwrap();
    let theirs = elsewhere(&rig, "theirs");
    // A folder of the nucleus whose manifest is the other one's.
    let folder = rig.root.join(".worktrees").join("x");
    fs::create_dir_all(&folder).unwrap();
    fs::copy(
        theirs.join(".workspace.json"),
        folder.join(".workspace.json"),
    )
    .unwrap();
    let session = stopped_in(&rig, "x");
    assert!(outcome(&rig, &session).is_none());
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

fn uncommitted(rig: &Rig) -> Option<u64> {
    let counts = Reporter::default().count(&rig.settings(), SystemTime::now());
    counts
        .nucleus("client-a")
        .unwrap()
        .unsaved
        .map(|unsaved| unsaved.uncommitted_repos)
}

#[test]
fn a_container_nucleus_counts_no_worktree_its_repos_say_is_outside_it() {
    let rig = contained();
    let inside = rig.session("fix-login");
    fs::write(inside.join("notes.txt"), "not committed\n").unwrap();
    assert_eq!(uncommitted(&rig), Some(1));
    // The container can write where its repos say their worktrees are.
    worktree_at(&rig, &rig.home.dir.join("workspace-other").join("wt"));
    assert_eq!(uncommitted(&rig), None);
}

#[test]
fn a_nucleus_on_the_host_still_counts_its_worktrees_wherever_they_are() {
    let rig = Rig::new();
    let inside = rig.session("fix-login");
    fs::write(inside.join("notes.txt"), "not committed\n").unwrap();
    worktree_at(&rig, &rig.home.dir.join("elsewhere").join("wt"));
    assert_eq!(uncommitted(&rig), Some(2));
}

#[test]
fn a_container_drivers_worktree_whose_git_is_outside_the_container_is_not_counted() {
    let rig = Rig::new();
    let workspaces = rig.home.dir.join("orca").join("workspaces");
    fs::create_dir_all(&workspaces).unwrap();
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"orca\"\nroot = \"{}\"\ndriver = \"orca\"\ncheck = \"true\"\n\
         {CLIENT_A_CONTAINER_UNIT}driver_cli = \"/nonexistent\"\ndriver_home = \"{}\"\n",
        workspaces.display(),
        rig.home.dir.join("orca-data").display(),
    ));
    let driver = |rig: &Rig| {
        let counts = Reporter::default().count(&rig.settings(), SystemTime::now());
        counts
            .nucleus("orca")
            .unwrap()
            .unsaved
            .map(|unsaved| unsaved.uncommitted_repos)
    };
    // A project in the login's nucleus, which the container has, and one of
    // its worktrees in Orca's folder: counted.
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
    assert_eq!(driver(&rig), Some(1));

    // A worktree there whose git is somewhere the container does not have.
    let theirs = rig.home.dir.join("workspace-other").join("web");
    git(
        &rig.home.dir,
        &[
            "clone",
            "-q",
            rig.upstream.to_str().unwrap(),
            theirs.to_str().unwrap(),
        ],
    );
    let planted = workspaces.join("planted");
    git(
        &theirs,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "planted",
            planted.to_str().unwrap(),
        ],
    );
    assert_eq!(driver(&rig), None);
}
