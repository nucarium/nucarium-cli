//! A driver's look, held against a container that does what it likes.
//!
//! The look is the box's own `nucleated look`, run in the environment's
//! container through one `podman exec`. Here `podman` is a stand-in that
//! runs it on this machine, or, told to, does something else in its place:
//! prints for ever, prints thirty thousand rows, hangs, prints nonsense. The
//! host must take none of that as an answer, be done inside its limit, keep
//! every other environment's look on time, and leave nothing running.

mod common;

use common::gate::{self, mark, MARK};
use common::{contract, run, stderr, Rig, CLIENT_A_CONTAINER_UNIT};
use nucleate::driver::{Condition, Memory, END_GRACE, END_LIMIT, LOOK_LIMIT};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const WORKING: &str = "11111111-1111-4111-8111-111111111111";
const WAITING: &str = "22222222-2222-4222-8222-222222222222";

/// A box with Orca in `client-a`'s container, as `orca`, and another Orca in
/// a container of its own, as `orca-b`.
struct Boxed {
    rig: Rig,
    workspaces: PathBuf,
    cli: PathBuf,
    cli_b: PathBuf,
    answer: PathBuf,
}

fn stand_in(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let fresh = path.with_extension("new");
    fs::write(&fresh, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&fresh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&fresh, path).unwrap();
}

fn boxed() -> Boxed {
    let rig = Rig::new();
    let home = rig.home.dir.clone();
    let boxed = Boxed {
        workspaces: home.join("orca").join("workspaces"),
        cli: home.join("orca-server").join("bin").join("orca-ide"),
        cli_b: home.join("orca-b-server").join("bin").join("orca-ide"),
        answer: home.join("orca-answer.json"),
        rig,
    };
    let workspaces_b = home.join("orca-b").join("workspaces");
    fs::create_dir_all(&boxed.workspaces).unwrap();
    fs::create_dir_all(&workspaces_b).unwrap();
    boxed.rig.configure_in_container(&format!(
        "[[environment]]\nname = \"orca\"\nroot = \"{}\"\ndriver = \"orca\"\ncheck = \"true\"\n\
         {CLIENT_A_CONTAINER_UNIT}driver_cli = \"{}\"\ndriver_poll = \"5s\"\n\
         [[environment]]\nname = \"orca-b\"\nroot = \"{}\"\ndriver = \"orca\"\ncheck = \"true\"\n\
         unit = \"agent-env-orca-b.service\"\ndriver_cli = \"{}\"\ndriver_poll = \"5s\"\n",
        boxed.workspaces.display(),
        boxed.cli.display(),
        workspaces_b.display(),
        boxed.cli_b.display(),
    ));
    stand_in(&boxed.cli, &format!("cat '{}'", boxed.answer.display()));
    stand_in(
        &boxed.cli_b,
        "echo '{\"ok\":true,\"result\":{\"worktrees\":[]}}'",
    );
    boxed.answers(&orca_listing(&[(WORKING, "working"), (WAITING, "waiting")]));
    boxed
}

impl Boxed {
    fn answers(&self, answer: &Value) {
        fs::write(&self.answer, answer.to_string()).unwrap();
    }

    fn look(&self) -> Memory {
        common::look(&self.rig, "orca", SystemTime::now())
    }

    /// A look at `orca`, timed.
    fn timed_look(&self) -> (Memory, Duration) {
        let started = Instant::now();
        let memory = self.look();
        (memory, started.elapsed())
    }
}

/// What Orca prints for agents in one folder workspace, by id and state.
fn orca_listing(agents: &[(&str, &str)]) -> Value {
    let agents: Vec<Value> = agents
        .iter()
        .map(|(id, state)| json!({ "orcaSessionId": id, "state": state, "agentType": "gemini" }))
        .collect();
    json!({ "ok": true, "result": { "worktrees": [
        { "workspaceKind": "folder", "path": "/nowhere", "agents": agents },
    ] } })
}

/// A look's answer as the box's own `nucleated look` prints one.
fn answer(sessions: Value) -> Value {
    json!({
        "version": 1, "driver": "orca", "condition": "ok", "complete": true,
        "sessions": sessions,
    })
}

/// A process's state, as one letter, where the box keeps it in `/proc`.
fn state(pid: &str) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .next()?
        .chars()
        .next()
}

/// Whether a process is still running. A process that was killed and is
/// waiting for its parent to collect it (a zombie) runs nothing, so it does
/// not count: on Linux its state in `/proc` says so.
fn alive(pid: &str) -> bool {
    if let Some(state) = state(pid) {
        return !matches!(state, 'Z' | 'X');
    }
    std::process::Command::new("sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .status()
        .unwrap()
        .success()
}

/// How long a process that was just killed may take to be gone.
const KILLED: Duration = Duration::from_secs(3);

/// How long the host may take, once it has given up on a look, to end what
/// is left of it: ask the engine which process is the container's first,
/// end the look from the box, give `podman exec` its grace to return by
/// itself, then stop it.
const ENDING: Duration = END_LIMIT.saturating_add(END_GRACE).saturating_add(KILLED);

/// Wait, at most `within`, for a process that is being ended to be gone.
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

/// Wait, at most [`KILLED`], until `ready` holds.
#[cfg(target_os = "linux")]
fn soon(ready: impl Fn() -> bool) -> bool {
    let started = Instant::now();
    while !ready() {
        if started.elapsed() > KILLED {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

fn pids_in(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_look_is_the_boxs_own_nucleated_run_once_inside_the_environments_container() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let memory = boxed.look();
    assert_eq!(memory.condition, Condition::Ok);
    assert_eq!(
        memory.sessions.keys().cloned().collect::<Vec<_>>(),
        [WORKING, WAITING]
    );
    assert!(!rig.spooled().is_empty());

    let calls = rig.podman_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let program = env!("CARGO_BIN_EXE_nucleated");
    let fixed = format!("exec agent-env-client-a {program} look --env orca --within 4500 --now ");
    assert!(calls[0].starts_with(&fixed), "{}", calls[0]);
    // Then the look's token, and what the last look left: words and numbers.
    let rest: Vec<&str> = calls[0][fixed.len()..].splitn(5, ' ').collect();
    assert!(rest[0].bytes().all(|c| c.is_ascii_digit()));
    assert_eq!(rest[1], "--token");
    assert!(rest[2].len() == 16 && rest[2].bytes().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(rest[3], "--previous");
    let previous: Value = serde_json::from_str(rest[4]).unwrap();
    assert_eq!(previous["driver"], "orca");
    assert_eq!(previous["sessions"], json!({}));

    // The other environment is looked at in its own container.
    let other = common::look(rig, "orca-b", SystemTime::now());
    assert_eq!(other.condition, Condition::Ok);
    assert!(rig.podman_calls()[1].starts_with(&format!(
        "exec agent-env-orca-b {program} look --env orca-b "
    )));
}

#[test]
fn whatever_a_container_prints_in_place_of_an_answer_costs_one_short_look_and_says_nothing() {
    let boxed = boxed();
    let rig = &boxed.rig;
    assert_eq!(boxed.look().condition, Condition::Ok);
    let remembered = boxed.look().sessions;
    let spooled = rig.spooled().len();

    let home = &rig.home.dir;
    // Thirty thousand rows, each one a session that fits.
    let rows: Vec<Value> = (0..30_000)
        .map(|n| json!({ "id": format!("s{n}"), "phase": "working" }))
        .collect();
    let many = home.join("many-rows.json");
    fs::write(&many, answer(json!(rows)).to_string()).unwrap();
    let pid_file = home.join("hung.pid");
    let hostile: Vec<(&str, String, Condition)> = vec![
        (
            "an answer that never ends",
            "exec yes '{\"version\":1,'".into(),
            Condition::BadReply,
        ),
        (
            "thirty thousand rows",
            format!("exec cat '{}'", many.display()),
            Condition::BadReply,
        ),
        (
            "a hung program",
            format!(
                "trap '' TERM; echo $$ >> '{}'; exec sleep 60",
                pid_file.display()
            ),
            Condition::Timeout,
        ),
        (
            "half an answer, then a hang",
            format!(
                "echo $$ >> '{}'; printf '{{\"version\":1,'; exec sleep 60",
                pid_file.display()
            ),
            Condition::Timeout,
        ),
        (
            "words",
            "echo the user typed this".into(),
            Condition::BadReply,
        ),
        ("nothing at all", "exit 0".into(), Condition::BadReply),
        ("a failure", "exit 125".into(), Condition::Absent),
        (
            "a newer answer",
            "echo '{\"version\":2,\"driver\":\"orca\",\"condition\":\"ok\"}'".into(),
            Condition::Unsupported,
        ),
        (
            "another driver's answer",
            format!(
                "echo '{}'",
                json!({ "version": 1, "driver": "t3code", "condition": "ok" })
            ),
            Condition::BadReply,
        ),
    ];
    let looks = hostile.len();
    for (what, script, expected) in hostile {
        rig.container_does("agent-env-client-a", &script);
        let (memory, took) = boxed.timed_look();
        assert_eq!(memory.condition, expected, "{what}");
        assert!(
            took < LOOK_LIMIT + Duration::from_millis(1500),
            "{what} took {took:?}"
        );
        // Nothing is ended, started or said for a look that found nothing.
        assert_eq!(memory.sessions, remembered, "{what}");
        assert_eq!(rig.spooled().len(), spooled, "{what}");
    }
    // For each look it gave up on, late or for printing too much, the host
    // asked the engine which process is the container's first, to end the
    // look from the box, and ran nothing more in the container.
    let asked = || -> Vec<String> {
        rig.podman_calls()
            .into_iter()
            .filter(|call| call.starts_with("container inspect "))
            .collect()
    };
    let waited = Instant::now();
    while asked().len() < 3 && waited.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let calls = rig.podman_calls();
    let asked = asked();
    assert_eq!(asked.len(), 3, "{calls:?}");
    for call in &asked {
        assert_eq!(
            call,
            "container inspect --format {{.State.Pid}} agent-env-client-a"
        );
    }
    let execs = calls
        .iter()
        .filter(|call| call.starts_with("exec "))
        .count();
    assert_eq!(execs, 2 + looks, "{calls:?}");
    // Nothing a hung look started is still running once the host has ended
    // it. In these the container ran its own program in place of the look,
    // so there is no look to end from the box: the `podman exec` itself
    // hangs, and it is stopped when its grace is up.
    let pids = pids_in(&pid_file);
    assert_eq!(pids.len(), 2, "{pids:?}");
    for pid in pids {
        assert!(gone(&pid, ENDING), "{pid} is still running");
    }

    // And once the container answers properly again, so does the look.
    fs::remove_file(home.join("containers").join("agent-env-client-a.does")).unwrap();
    assert_eq!(boxed.look().condition, Condition::Ok);
}

#[test]
fn a_driver_that_hangs_inside_the_container_is_ended_there_with_everything_it_started() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let pids = rig.home.dir.join("orca.pids");
    // Orca's program that never answers, deaf to being asked to stop, with a
    // child of its own.
    stand_in(
        &boxed.cli,
        &format!(
            "trap '' TERM\necho $$ >> '{pids}'\nsleep 60 &\necho $! >> '{pids}'\nwait",
            pids = pids.display()
        ),
    );
    let (memory, took) = boxed.timed_look();
    assert_eq!(memory.condition, Condition::Timeout);
    assert!(took < LOOK_LIMIT + Duration::from_millis(1500), "{took:?}");
    let started = pids_in(&pids);
    assert_eq!(started.len(), 2);
    for pid in &started {
        assert!(gone(pid, ENDING), "{pid} is still running");
    }

    // The look keeps its own time, whatever its driver does: when the time
    // it was given is up it stops what it started, says so, and leaves.
    fs::remove_file(&pids).unwrap();
    rig.looker();
    let mut command = rig.nucleated();
    command
        .env(
            "NUCLEATED_CONFIG",
            rig.home.config_file().with_file_name("inside.toml"),
        )
        .args(["look", "--env", "orca", "--within", "300"]);
    let began = Instant::now();
    let out = run(&mut command, "");
    let took = began.elapsed();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(said["condition"], "timeout");
    assert_eq!(said["version"], 1);
    assert!(took < Duration::from_secs(3), "{took:?}");
    let started = pids_in(&pids);
    assert_eq!(started.len(), 2);
    for pid in &started {
        assert!(gone(pid, KILLED), "{pid} is still running");
    }
}

/// A container that holds its look still, where the daemon's own `podman
/// exec` cannot reach it: run, as the engine's monitor runs it, in a session
/// of its own, then stopped, with its driver hung beneath it.
#[cfg(target_os = "linux")]
#[test]
fn a_look_the_container_holds_still_is_ended_from_the_box_with_everything_it_started() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let home = &rig.home.dir;
    let (client, looks, drivers) = (
        home.join("client.pid"),
        home.join("look.pids"),
        home.join("driver.pids"),
    );
    stand_in(
        &boxed.cli,
        &format!(
            "echo $$ >> '{drivers}'\nsleep 60 &\necho $! >> '{drivers}'\nwait",
            drivers = drivers.display()
        ),
    );
    rig.container_does(
        "agent-env-client-a",
        &format!(
            "echo $$ > '{client}'\nsetsid \"$@\" 2> /dev/null &\nlook=$!\necho $look >> '{looks}'\n\
             sleep 1\nkill -STOP $look\nwait",
            client = client.display(),
            looks = looks.display(),
        ),
    );
    // Another look on the box, at another environment, that is none of this
    // one's business: held still in the middle of asking its driver, which
    // hangs.
    let other_driver = home.join("other-driver.pid");
    stand_in(
        &boxed.cli_b,
        &format!("echo $$ > '{}'\nexec sleep 60", other_driver.display()),
    );
    rig.looker();
    let mut other = rig.nucleated();
    other
        .env(
            "NUCLEATED_CONFIG",
            rig.home.config_file().with_file_name("inside.toml"),
        )
        .args(["look", "--env", "orca-b", "--token", "00112233445566aa"])
        .stdout(std::process::Stdio::null());
    let mut other = other.spawn().unwrap();
    let other_pid = other.id().to_string();
    assert!(
        soon(|| pids_in(&other_driver).len() == 1),
        "the other look did not start its driver"
    );
    let other_driver = pids_in(&other_driver).remove(0);
    assert!(std::process::Command::new("kill")
        .args(["-STOP", &other_pid])
        .status()
        .unwrap()
        .success());
    assert!(
        soon(|| state(&other_pid) == Some('T')),
        "the other look was not held still: {:?}",
        state(&other_pid)
    );

    let (memory, took) = boxed.timed_look();
    assert_eq!(memory.condition, Condition::Timeout);
    assert!(took < LOOK_LIMIT + Duration::from_millis(1500), "{took:?}");
    let look = pids_in(&looks);
    let driver = pids_in(&drivers);
    assert_eq!((look.len(), driver.len()), (1, 2), "{look:?} {driver:?}");
    for pid in pids_in(&client).iter().chain(&look).chain(&driver) {
        assert!(gone(pid, ENDING), "{pid} is still running");
    }
    // The other look is still stopped, as this test left it, and its driver
    // still runs: ending this look touched neither.
    assert_eq!(state(&other_pid), Some('T'), "another look was ended");
    assert!(alive(&other_driver), "another look's driver was ended");
    let _ = other.kill();
    let _ = other.wait();
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &other_driver])
        .status();
    // Nothing more was run in the container to end it.
    let calls = rig.podman_calls();
    let execs = calls
        .iter()
        .filter(|call| call.starts_with("exec "))
        .count();
    assert_eq!(execs, 1, "{calls:?}");
}

#[test]
fn planted_pipes_and_links_in_a_drivers_folder_cost_one_short_look() {
    let boxed = boxed();
    let workspaces = &boxed.workspaces;
    // One worktree whose `.git` is a pipe nobody writes to, and one whose
    // `.git` names a git folder whose HEAD links to a device that never ends.
    let piped = workspaces.join("piped");
    fs::create_dir_all(&piped).unwrap();
    assert!(std::process::Command::new("mkfifo")
        .arg(piped.join(".git"))
        .status()
        .unwrap()
        .success());
    let endless = workspaces.join("endless");
    let git_dir = boxed.rig.home.dir.join("endless-git");
    fs::create_dir_all(&endless).unwrap();
    fs::create_dir_all(&git_dir).unwrap();
    fs::write(
        endless.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    std::os::unix::fs::symlink("/dev/zero", git_dir.join("HEAD")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", git_dir.join("config")).unwrap();
    boxed.answers(&json!({ "ok": true, "result": { "worktrees": [
        { "workspaceKind": "git", "path": piped, "agents": [
            { "orcaSessionId": WORKING, "state": "working", "agentType": "gemini" },
        ] },
        { "workspaceKind": "git", "path": endless, "agents": [
            { "orcaSessionId": WAITING, "state": "waiting", "agentType": "gemini" },
        ] },
    ] } }));
    let (memory, took) = boxed.timed_look();
    assert_eq!(memory.condition, Condition::Ok);
    assert!(took < LOOK_LIMIT, "{took:?}");
    assert_eq!(memory.sessions.len(), 2);
    // Each is reported, and nothing is said of where it works.
    for record in boxed.rig.spooled() {
        assert!(record["event"].get("repos").is_none(), "{record}");
        assert!(record["event"].get("branch").is_none(), "{record}");
    }
}

#[test]
fn one_environments_hung_look_holds_up_no_other() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let pid_file = rig.home.dir.join("hung.pids");
    // `client-a`'s container hangs every look; `orca-b`'s answers at once.
    rig.container_does(
        "agent-env-client-a",
        &format!("echo $$ >> '{}'; exec sleep 60", pid_file.display()),
    );
    let podman = rig.looker().podman;
    let path = common::path_with(&rig.home, &[]);
    assert!(podman.starts_with(rig.home.dir.join("stand-ins")));
    let looks_at_b = || {
        rig.podman_calls()
            .iter()
            .filter(|call| call.contains("look --env orca-b "))
            .count()
    };
    let started = Instant::now();
    let second_look_at_b = std::cell::Cell::new(None);
    // Until `orca-b` has been looked at twice, and the daemon has given up
    // on `orca`'s first look and it is over.
    let hung_and_ended = || {
        let pids = pids_in(&pid_file);
        !pids.is_empty() && pids.iter().all(|pid| !alive(pid))
    };
    gate::daemon_until(rig, Some(&path), "two looks at orca-b", &|| {
        if second_look_at_b.get().is_none() && looks_at_b() >= 2 {
            second_look_at_b.set(Some(started.elapsed()));
        }
        second_look_at_b.get().is_some() && hung_and_ended()
    });
    let took = second_look_at_b.get().unwrap();
    // The first look is five seconds after start, the next one poll later.
    // Had the hung look at `orca` held up `orca-b`'s, each would have waited
    // five seconds more.
    assert!(took < Duration::from_secs(13), "took {took:?}");
    let memory = nucleate::driver::read_memory(&rig.home.state_dir(), "orca-b").unwrap();
    assert_eq!(memory.condition, Condition::Ok);
    let memory = nucleate::driver::read_memory(&rig.home.state_dir(), "orca").unwrap();
    assert_eq!(memory.condition, Condition::Timeout);
}

#[test]
fn nothing_a_look_hands_over_but_ids_words_and_numbers_reaches_the_wire_the_disk_or_the_status() {
    let boxed = boxed();
    let rig = &boxed.rig;
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let good_session = json!({
        "id": WORKING, "phase": "failed", "error_kind": "failure",
        "limit_resets_at_ms": 1_795_089_600_000_u64, "activity": 3, "turn": 2,
        "claim_key": "wt-0123456789abcdef", "agent_session_id": WAITING,
        "repos": ["acme/web"], "branch": "orca/task-1", "workspace": "task-1",
        "folder": "0123456789abcdef", "within": ["fedcba9876543210"],
    });
    let good = || {
        let mut good = answer(json!([good_session.clone()]));
        good["link_ref"] = json!("orca-1");
        good["facts"] = json!({ "listed_once": 1 });
        good["uncounted_worktrees"] = json!(0);
        good
    };
    // The sentinel, in turn, in every field a look can hand over, and in
    // one it may not.
    let mut marked: Vec<(String, Value)> = Vec::new();
    for field in [
        "driver",
        "condition",
        "link_ref",
        "uncounted_worktrees",
        "title",
    ] {
        let mut answer = good();
        answer[field] = json!(mark(field));
        marked.push((field.to_string(), answer));
    }
    let mut answer = good();
    answer["facts"] = json!({ mark("facts"): 1 });
    marked.push(("facts".into(), answer));
    for field in [
        "id",
        "phase",
        "error_kind",
        "limit_resets_at_ms",
        "activity",
        "turn",
        "claim_key",
        "agent_session_id",
        "branch",
        "workspace",
        "folder",
        "prompt",
    ] {
        let mut answer = good();
        answer["sessions"][0][field] = json!(mark(field));
        marked.push((format!("sessions.{field}"), answer));
    }
    for field in ["repos", "within"] {
        let mut answer = good();
        answer["sessions"][0][field] = json!([mark(field)]);
        marked.push((format!("sessions.{field}"), answer));
    }
    let file = rig.home.dir.join("answer.json");
    rig.container_does(
        "agent-env-client-a",
        &format!("exec cat '{}'", file.display()),
    );
    for (field, answer) in &marked {
        assert!(answer.to_string().contains(MARK), "{field}");
        fs::write(&file, answer.to_string()).unwrap();
        assert_eq!(boxed.look().condition, Condition::BadReply, "{field}");
        assert!(rig.spooled().is_empty(), "{field}");
    }
    // The answer as it should be is taken, and reaches Watch as ids, fixed
    // words and repo names.
    fs::write(&file, good().to_string()).unwrap();
    let memory = boxed.look();
    assert_eq!(memory.condition, Condition::Ok);
    assert_eq!(memory.link_ref.as_deref(), Some("orca-1"));
    assert!(!rig.spooled().is_empty());
    gate::tick(rig);
    assert!(!mock.to(contract::EVENTS).is_empty());
    gate::assert_clean(rig, &mock);
}
