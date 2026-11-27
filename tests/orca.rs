//! Orca: what the adapter does, and its part of the privacy gate.
//!
//! The gate's shared pieces are in `common::gate`. This adapter's fixtures
//! are under `tests/fixtures/orca/`: an answer in the shape Orca's source
//! gives `orca-ide worktree ps --json`, and the same shape with the sentinel
//! in every field that holds content. The stand-in `orca-ide` here prints
//! one of them and writes down how it was run, so every test can say that
//! the one fixed command was all that was ever run. That housekeeping leaves
//! a driver's worktrees alone is proved for every such driver in
//! `tests/seam.rs`, and once more here beside the count of what they hold.

mod common;

use common::{
    contract, files_under, gate, git, run, stderr, stdout_json, Rig, CLIENT_A_CONTAINER_UNIT,
};
use nucleate::driver::{self, Condition, Memory, Phase};
use nucleate::reporter::Reporter;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const PLAIN: &str = include_str!("fixtures/orca/worktree-ps.json");
const SENTINEL: &str = include_str!("fixtures/orca/worktree-ps-sentinel.json");

// The agents in the plain fixture, by what they are.
const CLAUDE: &str = "11111111-1111-4111-8111-111111111111";
const GEMINI: &str = "22222222-2222-4222-8222-222222222222";
const CODEX: &str = "33333333-3333-4333-8333-333333333333";
const OPENCODE: &str = "44444444-4444-4444-8444-444444444444";
const AIDER: &str = "55555555-5555-4555-8555-555555555555";
const AMP: &str = "66666666-6666-4666-8666-666666666666";
const PI: &str = "77777777-7777-4777-8777-777777777777";

/// The id of the Claude Code session whose own hook reports it.
const HOOKED: &str = "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a";

/// A box that runs Orca beside a nucleus: Orca's workspace directory, the
/// projects Orca was pointed at, and a stand-in for Orca's program.
struct Orca {
    rig: Rig,
    workspaces: PathBuf,
    projects: PathBuf,
    cli: PathBuf,
    data: PathBuf,
    answer: PathBuf,
    calls: PathBuf,
}

impl Orca {
    fn new() -> Orca {
        Orca::under("")
    }

    /// As `new`, with everything of Orca's below a folder of this name.
    fn under(parent: &str) -> Orca {
        let rig = Rig::new();
        let base = rig.home.dir.join(parent);
        let orca = Orca {
            workspaces: base.join("orca").join("workspaces"),
            // In a folder of the container Orca runs in: the login's.
            projects: rig.root.join(parent).join("projects"),
            cli: rig
                .home
                .dir
                .join("orca-server")
                .join("bin")
                .join("orca-ide"),
            data: rig.home.dir.join("orca-data"),
            answer: rig.home.dir.join("orca-answer.json"),
            calls: rig.home.dir.join("orca-calls.log"),
            rig,
        };
        fs::create_dir_all(&orca.workspaces).unwrap();
        fs::create_dir_all(&orca.projects).unwrap();
        orca.configure("");
        orca.stand_in("cat \"$ANSWER\"");
        orca
    }

    /// Orca runs in `client-a`'s container, beside the agent's login, as
    /// the recipe runs a driver.
    fn configure(&self, more: &str) {
        self.rig.configure_in_container(&format!(
            "[[environment]]\nname = \"orca\"\nroot = \"{}\"\ndriver = \"orca\"\ncheck = \"true\"\n\
             {CLIENT_A_CONTAINER_UNIT}driver_cli = \"{}\"\ndriver_home = \"{}\"\n{more}",
            self.workspaces.display(),
            self.cli.display(),
            self.data.display(),
        ));
    }

    /// Put a program where the config says Orca's is. It writes down its
    /// arguments and every `ORCA_` variable it was given, then does `does`.
    fn stand_in(&self, does: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(self.cli.parent().unwrap()).unwrap();
        let script = format!(
            "#!/bin/sh\nANSWER='{}'\n{{ echo \"argv: $*\"; env | grep '^ORCA_' | sort; }} >> '{}'\n{does}\n",
            self.answer.display(),
            self.calls.display(),
        );
        // Written beside and moved into place: a program being written
        // cannot be run.
        let fresh = self.cli.with_extension("new");
        fs::write(&fresh, script).unwrap();
        fs::set_permissions(&fresh, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&fresh, &self.cli).unwrap();
    }

    /// A fixture as Orca would print it on this box.
    fn fixture(&self, text: &str) -> Value {
        let filled = text
            .replace("@WORKSPACES@", self.workspaces.to_str().unwrap())
            .replace("@PROJECTS@", self.projects.to_str().unwrap());
        serde_json::from_str(&filled).unwrap()
    }

    /// What Orca answers from now on.
    fn answers(&self, answer: &Value) {
        fs::write(&self.answer, answer.to_string()).unwrap();
    }

    /// A project Orca was pointed at: an ordinary clone of the upstream.
    fn project(&self, name: &str, on_github: bool) -> PathBuf {
        let project = self.projects.join(name);
        git(
            &self.rig.home.dir,
            &[
                "clone",
                "-q",
                self.rig.upstream.to_str().unwrap(),
                project.to_str().unwrap(),
            ],
        );
        if on_github {
            // Its remote reads as one on GitHub. Nothing here fetches.
            let url = "https://github.com/acme/web.git";
            git(&project, &["config", "remote.origin.url", url]);
        }
        project
    }

    /// A worktree as Orca makes one: `git worktree add` on a new branch.
    /// `from` is the branch it starts from and then tracks.
    fn worktree(&self, project: &Path, at: &Path, branch: &str, from: Option<&str>) -> PathBuf {
        let mut args = vec!["worktree", "add", "-q", "-b", branch, at.to_str().unwrap()];
        args.extend(from);
        git(project, &args);
        at.to_path_buf()
    }

    fn look(&self) -> Memory {
        self.look_at(SystemTime::now())
    }

    fn look_at(&self, now: SystemTime) -> Memory {
        common::look(&self.rig, "orca", now)
    }

    /// How the stand-in has been run so far: one line per run and one per
    /// `ORCA_` variable it was given.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The stand-in was run this many times, each time with the one fixed
    /// command, Orca's data folder, and nothing else of Orca's.
    fn assert_only_asked(&self, times: usize) {
        let one = [
            "argv: worktree ps --json".to_string(),
            format!("ORCA_USER_DATA_PATH={}", self.data.display()),
        ];
        let expected: Vec<String> = (0..times).flat_map(|_| one.clone()).collect();
        assert_eq!(self.calls(), expected);
    }
}

/// The events in the spool from `skip` on, as (session, type).
fn spooled(rig: &Rig, skip: usize) -> Vec<(String, String)> {
    rig.spooled()
        .iter()
        .skip(skip)
        .map(|record| {
            let event = &record["event"];
            assert_eq!(event["env_id"], "orca");
            (
                event["session_id"].as_str().unwrap().to_string(),
                event["type"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn events(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(session, kind)| (session.to_string(), kind.to_string()))
        .collect()
}

/// The one spooled event of this session and type.
fn the_event(rig: &Rig, session: &str, kind: &str) -> Value {
    let found: Vec<Value> = rig
        .spooled()
        .into_iter()
        .map(|record| record["event"].clone())
        .filter(|event| event["session_id"] == session && event["type"] == kind)
        .collect();
    assert_eq!(found.len(), 1, "{session} {kind}");
    found[0].clone()
}

/// One agent's row in an answer, to change what Orca says of it.
fn row<'a>(answer: &'a mut Value, session: &str) -> &'a mut Value {
    answer["result"]["worktrees"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .flat_map(|worktree| worktree["agents"].as_array_mut().unwrap().iter_mut())
        .find(|agent| agent["orcaSessionId"] == session)
        .unwrap()
}

/// Take an agent out of an answer, as Orca does when it is closed.
fn close(answer: &mut Value, session: &str) {
    for worktree in answer["result"]["worktrees"].as_array_mut().unwrap() {
        worktree["agents"]
            .as_array_mut()
            .unwrap()
            .retain(|agent| agent["orcaSessionId"] != session);
    }
}

/// The plain fixture's worktrees, made for real: two of Orca's, one in each
/// of its layouts, from a project on GitHub, and a folder workspace.
fn plain(orca: &Orca) -> Value {
    let project = orca.project("web", true);
    orca.worktree(
        &project,
        &orca.workspaces.join("task-1"),
        "orca/task-1",
        None,
    );
    orca.worktree(
        &project,
        &orca.workspaces.join("web").join("task-2"),
        "orca/task-2",
        None,
    );
    fs::create_dir_all(orca.workspaces.join("notes")).unwrap();
    orca.fixture(PLAIN)
}

/// Claude Code's hook, inside `client-a`'s container, where the recipe has
/// it name its login's environment. It writes the container's own spool.
fn hook(rig: &Rig, event: &str, cwd: &Path) {
    let out = run(
        rig.in_container(rig.nucleated().args(["hook", event, "--env", "client-a"])),
        &json!({ "session_id": HOOKED, "cwd": cwd }).to_string(),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

#[test]
fn with_no_orca_there_the_environment_is_alive_and_nothing_else_is_said() {
    let orca = Orca::new();
    let rig = &orca.rig;
    fs::remove_file(&orca.cli).unwrap();
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);

    let memory = orca.look();
    assert_eq!(memory.condition, Condition::Absent);
    assert!(memory.sessions.is_empty());
    assert!(rig.spooled().is_empty());

    gate::tick(rig);
    let beat = gate::said(&mock);
    let seen = &beat["environments"][1];
    assert_eq!(seen["env_id"], "orca");
    assert_eq!(seen["driver"], "orca");
    // Whether the server is alive is its supervisor's to say, not the adapter's.
    assert_eq!(seen["server_alive"], true);
    assert!(seen.get("driver_ref").is_none());
    // Orca's worktrees are never snapshotted, so there is no time to give.
    assert!(seen.get("last_snapshot_at").is_none());
    assert!(mock.to(contract::EVENTS).is_empty());
    gate::assert_clean(rig, &mock);
}

#[test]
fn each_agent_orca_lists_is_reported_by_its_state_and_one_command_is_all_that_is_run() {
    let orca = Orca::new();
    let rig = &orca.rig;
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let mut answer = plain(&orca);
    orca.answers(&answer);

    let memory = orca.look();
    assert_eq!(memory.condition, Condition::Ok);
    assert_eq!(memory.link_ref, None);
    assert_eq!(
        spooled(rig, 0),
        events(&[
            (CLAUDE, "session_start"),
            (CLAUDE, "user_prompt_submit"),
            // Waiting and blocked both need the owner. Orca does not say
            // whether for a permission or an answer, so neither is claimed.
            (GEMINI, "session_start"),
            (GEMINI, "input_prompt"),
            (CODEX, "session_start"),
            (CODEX, "input_prompt"),
            (OPENCODE, "session_start"),
            (OPENCODE, "stop_failure"),
            // Its subagents keep the row working; the main agent's own turn
            // was cut short, and that is what is said.
            (AMP, "session_start"),
            (AMP, "stop_failure"),
        ])
    );
    // What was idle when Orca was first seen is history. An agent Orca
    // restored and has not confirmed, and one it has not named yet, are not
    // spoken for.
    assert_eq!(memory.sessions[AIDER].phase, Phase::Idle);
    assert!(!memory.sessions.contains_key(PI));
    assert_eq!(memory.sessions.len(), 6);

    // Where each works is read from git's own files in its worktree: the
    // repo's name and the branch, in either of Orca's two layouts and in
    // the project itself.
    let started = the_event(rig, CLAUDE, "session_start");
    assert_eq!(started["repos"], json!(["acme/web"]));
    assert_eq!(started["branch"], "orca/task-1");
    assert!(started.get("bridge_session_id").is_none());
    let nested = the_event(rig, CODEX, "input_prompt");
    assert_eq!(nested["repos"], json!(["acme/web"]));
    assert_eq!(nested["branch"], "orca/task-2");
    assert_eq!(the_event(rig, AMP, "session_start")["branch"], "main");
    // A failed turn carries Orca's one word for how it ended.
    assert_eq!(
        the_event(rig, OPENCODE, "stop_failure")["error_kind"],
        "failure"
    );
    assert_eq!(
        the_event(rig, AMP, "stop_failure")["error_kind"],
        "interruption"
    );
    assert!(started.get("error_kind").is_none());

    // Each change Orca reports is the event a hook would have recorded.
    row(&mut answer, CLAUDE)["mainAgent"] = json!({ "state": "done", "stateStartedAt": 2 });
    row(&mut answer, GEMINI)["state"] = json!("working");
    row(&mut answer, AIDER)["mainAgent"] = json!({ "state": "working", "stateStartedAt": 3 });
    row(&mut answer, AMP)["mainAgent"]["outcome"] = json!("cancellation");
    row(&mut answer, PI)["restoredUnconfirmed"] = json!(false);
    orca.answers(&answer);
    let before = rig.spooled().len();
    orca.look();
    assert_eq!(
        spooled(rig, before),
        events(&[
            (CLAUDE, "stop"),
            (GEMINI, "activity"),
            (AIDER, "user_prompt_submit"),
            (AMP, "stop"),
            (PI, "session_start"),
            (PI, "user_prompt_submit"),
        ])
    );
    // A folder workspace has no git, so nothing is said of where it is.
    let folder = the_event(rig, PI, "session_start");
    assert!(folder.get("repos").is_none(), "{folder}");
    assert!(folder.get("branch").is_none());

    // An agent Orca stops listing is over once a second whole list agrees.
    close(&mut answer, CODEX);
    orca.answers(&answer);
    let before = rig.spooled().len();
    orca.look();
    assert_eq!(spooled(rig, before), events(&[]));
    orca.look();
    assert_eq!(spooled(rig, before), events(&[(CODEX, "session_end")]));

    // The events reach Watch as any hook's do, and nothing else does.
    let written = spooled(rig, 0);
    gate::tick(rig);
    let sent: Vec<(String, String)> = mock
        .to(contract::EVENTS)
        .iter()
        .flat_map(|request| request.json()["events"].as_array().unwrap().clone())
        .map(|event| {
            assert_eq!(event["env_id"], "orca");
            (
                event["session_id"].as_str().unwrap().to_string(),
                event["type"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for pair in &sent {
        assert!(written.contains(pair), "{pair:?} was never written");
    }
    for told in events(&[
        (CLAUDE, "session_start"),
        (GEMINI, "input_prompt"),
        (OPENCODE, "stop_failure"),
        (CODEX, "session_end"),
    ]) {
        assert!(sent.contains(&told), "{told:?} was not sent");
    }
    let beat = gate::said(&mock);
    assert_eq!(beat["environments"][1]["driver"], "orca");
    assert!(beat["environments"][1].get("driver_ref").is_none());
    gate::assert_clean(rig, &mock);
    orca.assert_only_asked(4);
}

#[test]
fn a_working_agent_is_heard_from_while_orca_says_it_is_doing_things() {
    let orca = Orca::new();
    let rig = &orca.rig;
    let mut answer = plain(&orca);
    orca.answers(&answer);
    let start = SystemTime::now();
    orca.look_at(start);
    let before = rig.spooled().len();

    // Orca moves `updatedAt` on with each tool and prompt. Within a beat of
    // the last word that is not news.
    row(&mut answer, CLAUDE)["updatedAt"] = json!(1_795_089_700_000_u64);
    orca.answers(&answer);
    orca.look_at(start + Duration::from_secs(15));
    assert_eq!(spooled(rig, before), events(&[]));
    // A beat on, it is.
    row(&mut answer, CLAUDE)["updatedAt"] = json!(1_795_089_900_000_u64);
    orca.answers(&answer);
    orca.look_at(start + Duration::from_secs(301));
    assert_eq!(spooled(rig, before), events(&[(CLAUDE, "activity")]));
    // Another beat with nothing done, and nothing is made up.
    orca.look_at(start + Duration::from_secs(700));
    assert_eq!(spooled(rig, before).len(), 1);
    orca.assert_only_asked(4);
}

#[test]
fn a_list_orca_cut_short_ends_no_session() {
    let orca = Orca::new();
    let rig = &orca.rig;
    let mut answer = plain(&orca);
    orca.answers(&answer);
    orca.look();
    let before = rig.spooled().len();

    close(&mut answer, CLAUDE);
    answer["result"]["truncated"] = json!(true);
    orca.answers(&answer);
    for _ in 0..3 {
        let memory = orca.look();
        assert_eq!(memory.condition, Condition::Ok);
        assert!(memory.sessions.contains_key(CLAUDE));
    }
    // Nor does one that does not say whether it is whole.
    answer["result"]
        .as_object_mut()
        .unwrap()
        .remove("truncated");
    orca.answers(&answer);
    for _ in 0..3 {
        assert!(orca.look().sessions.contains_key(CLAUDE));
    }
    assert_eq!(spooled(rig, before), events(&[]));
    // What a cut list does hold is still heard.
    row(&mut answer, GEMINI)["state"] = json!("done");
    orca.answers(&answer);
    orca.look();
    assert_eq!(spooled(rig, before), events(&[(GEMINI, "stop")]));
}

#[test]
fn an_orca_that_fails_or_answers_something_else_changes_nothing_but_a_word() {
    let orca = Orca::new();
    let rig = &orca.rig;
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let mut answer = plain(&orca);
    orca.answers(&answer);
    let known = orca.look().sessions;
    let before = rig.spooled().len();
    let mut looks = 1;

    let refusal = json!({
        "id": "cli-1",
        "ok": false,
        "error": { "code": "runtime_unavailable", "message": "Orca is not running" },
        "_meta": { "runtimeId": null },
    });
    let mut odd = answer.clone();
    odd["result"]["worktrees"] = json!({ "task-1": [] });
    let mut rows_in_order = answer.clone();
    rows_in_order["result"]["worktrees"] =
        json!([[orca.workspaces.join("task-1"), "git", [["x", "working"]]]]);
    let cases: Vec<(&str, String, &str, Condition)> = vec![
        // Its program ran and said it failed, as it does with no Orca
        // running, whatever it printed first.
        (
            "failed",
            answer.to_string(),
            "cat \"$ANSWER\"; exit 3",
            Condition::Error,
        ),
        (
            "refused",
            refusal.to_string(),
            "cat \"$ANSWER\"; exit 1",
            Condition::Error,
        ),
        (
            "failed, with words",
            "Orca 1.4.214\nNo such command\n".into(),
            "cat \"$ANSWER\"; exit 2",
            Condition::Error,
        ),
        ("failed, silent", String::new(), "exit 1", Condition::Error),
        // It printed an error reply and said it succeeded.
        (
            "refused, exit 0",
            refusal.to_string(),
            "cat \"$ANSWER\"",
            Condition::Error,
        ),
        // It said it succeeded, and said nothing.
        (
            "silent",
            String::new(),
            "cat \"$ANSWER\"",
            Condition::Unreachable,
        ),
        // It answered, and not with its documented answer.
        (
            "words",
            "Orca 1.4.214\nNo such command\n".into(),
            "cat \"$ANSWER\"",
            Condition::BadReply,
        ),
        (
            "cut off",
            answer.to_string()[..2_000].to_string(),
            "cat \"$ANSWER\"",
            Condition::BadReply,
        ),
        (
            "no result",
            json!({ "ok": true }).to_string(),
            "cat \"$ANSWER\"",
            Condition::BadReply,
        ),
        (
            "another shape",
            odd.to_string(),
            "cat \"$ANSWER\"",
            Condition::BadReply,
        ),
        (
            "rows in order",
            rows_in_order.to_string(),
            "cat \"$ANSWER\"",
            Condition::BadReply,
        ),
        (
            "an array",
            json!([true, answer["result"]]).to_string(),
            "cat \"$ANSWER\"",
            Condition::BadReply,
        ),
    ];
    for (what, printed, does, word) in cases {
        fs::write(&orca.answer, printed).unwrap();
        orca.stand_in(does);
        let memory = orca.look();
        looks += 1;
        assert_eq!(memory.condition, word, "{what}");
        assert_eq!(memory.sessions, known, "{what} forgot a session");
        assert_eq!(spooled(rig, before), events(&[]), "{what} wrote an event");
    }

    // Through all of it the environment's server is as alive as its
    // supervisor says, and no other environment is touched.
    gate::tick(rig);
    let beat = mock.to(contract::HEARTBEATS)[0].json();
    assert_eq!(beat["environments"][0]["server_alive"], true);
    assert_eq!(beat["environments"][1]["server_alive"], true);
    let status = stdout_json(&run(rig.nucleated().args(["status", "--json"]), ""));
    assert_eq!(
        status["environments"][1]["driver"]["condition"],
        "bad_reply"
    );

    // Orca comes back, and each session is as it was left.
    row(&mut answer, CLAUDE)["mainAgent"]["state"] = json!("waiting");
    orca.answers(&answer);
    orca.stand_in("cat \"$ANSWER\"");
    let before = rig.spooled().len();
    assert_eq!(orca.look().condition, Condition::Ok);
    assert_eq!(spooled(rig, before), events(&[(CLAUDE, "input_prompt")]));
    orca.assert_only_asked(looks + 1);
    gate::assert_clean(rig, &mock);
}

#[test]
fn an_orca_that_hangs_is_stopped_within_the_limit_and_is_no_answer() {
    let orca = Orca::new();
    orca.answers(&plain(&orca));
    orca.stand_in("cat \"$ANSWER\"; sleep 60");
    let started = Instant::now();
    let memory = orca.look();
    assert_eq!(memory.condition, Condition::Timeout);
    assert!(memory.sessions.is_empty());
    assert!(orca.rig.spooled().is_empty());
    assert!(started.elapsed() < driver::LOOK_LIMIT + Duration::from_secs(3));
    orca.assert_only_asked(1);
}

#[test]
fn an_agent_whose_own_hook_reports_it_is_left_to_the_hook() {
    let orca = Orca::new();
    let rig = &orca.rig;
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let mut answer = plain(&orca);
    // A second Claude Code, in the other worktree, with no hook of its own.
    let other = "88888888-8888-4888-8888-888888888888";
    let mut second = row(&mut answer, CLAUDE).clone();
    second["orcaSessionId"] = json!(other);
    answer["result"]["worktrees"][1]["agents"]
        .as_array_mut()
        .unwrap()
        .push(second);
    close(&mut answer, CODEX);
    orca.answers(&answer);

    // Claude Code in Orca's worktree fires nucleated's hook as it does
    // anywhere, from a folder inside that worktree.
    let inside = orca.workspaces.join("task-1").join("src");
    fs::create_dir_all(&inside).unwrap();
    hook(rig, "UserPromptSubmit", &inside);
    let hooked = rig.spooled_in_container();
    assert_eq!(hooked.len(), 1);
    assert_eq!(hooked[0]["event"]["env_id"], "client-a");
    assert_eq!(hooked[0]["event"]["session_id"], HOOKED);
    assert_eq!(hooked[0]["event"]["claim"]["driver_env"], "orca");
    // The daemon takes it in, and with it the claim.
    gate::tick(rig);
    assert!(rig.spooled_in_container().is_empty());

    // Orca lists the same agent. It is not reported a second time; the
    // agent beside it that has no hook adapter is, and so is the Claude
    // Code in a worktree no hook has spoken from.
    orca.look();
    let seen = spooled(rig, 0);
    assert!(
        !seen.iter().any(|(session, _)| session == CLAUDE),
        "{seen:?}"
    );
    assert!(seen.contains(&(GEMINI.to_string(), "session_start".to_string())));
    assert!(seen.contains(&(other.to_string(), "user_prompt_submit".to_string())));

    // What marks the worktree as spoken for is a number, not its name.
    let claims: Vec<PathBuf> = files_under(&rig.home.state_dir().join("claims"))
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(claims.len(), 1);
    assert!(
        !claims[0].to_string_lossy().contains("task-1"),
        "{claims:?}"
    );

    // The hook's session ends, and Orca's view of the agent is the only one.
    hook(rig, "SessionEnd", &inside);
    gate::tick(rig);
    let before = rig.spooled().len();
    orca.look();
    assert_eq!(
        spooled(rig, before),
        events(&[(CLAUDE, "session_start"), (CLAUDE, "user_prompt_submit")])
    );
    assert!(files_under(&rig.home.state_dir().join("claims")).is_empty());
    gate::assert_clean(rig, &mock);
    orca.assert_only_asked(2);
}

#[test]
fn one_agents_hook_stands_in_for_that_agent_and_not_for_another_kind_beside_it() {
    let orca = Orca::new();
    let rig = &orca.rig;
    let mut answer = plain(&orca);
    // A Codex in the same worktree as the Claude Code.
    let beside = "88888888-8888-4888-8888-888888888888";
    let mut second = row(&mut answer, CODEX).clone();
    second["orcaSessionId"] = json!(beside);
    answer["result"]["worktrees"][0]["agents"]
        .as_array_mut()
        .unwrap()
        .push(second);
    orca.answers(&answer);

    // Only the Claude Code there has nucleated's hooks. Its hook speaks for
    // it, and the Codex beside it is still Orca's to report.
    let inside = orca.workspaces.join("task-1");
    hook(rig, "UserPromptSubmit", &inside);
    gate::tick(rig);
    orca.look();
    let seen = spooled(rig, 0);
    assert!(
        !seen.iter().any(|(session, _)| session == CLAUDE),
        "{seen:?}"
    );
    assert!(
        seen.contains(&(beside.to_string(), "session_start".to_string())),
        "{seen:?}"
    );

    // Then the Codex is given its hooks too, and speaks for itself.
    let out = run(
        rig.in_container(rig.nucleated().args([
            "hook",
            "UserPromptSubmit",
            "--env",
            "client-a",
            "--agent",
            "codex",
        ])),
        &json!({ "session_id": "0c9d8e7a-58b4-4d0c-9a6e-7f3c2a1e2b1f", "cwd": inside }).to_string(),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    gate::tick(rig);
    // Orca shows that Codex blocked on its owner, and its hook has said no
    // such thing. What the owner needs to hear is not taken back on the
    // hook's word that it is there.
    let before = rig.spooled().len();
    let memory = orca.look();
    assert_eq!(spooled(rig, before), events(&[]));
    assert!(memory.sessions.contains_key(beside));
    // Once Orca shows it at work again, the hook's word is enough.
    row(&mut answer, beside)["state"] = json!("working");
    orca.answers(&answer);
    orca.look();
    assert_eq!(spooled(rig, before), events(&[(beside, "session_end")]));
    // The Codex in the other worktree, where no hook has spoken, is not
    // touched by either.
    assert!(seen.contains(&(CODEX.to_string(), "session_start".to_string())));
    let memory = orca.look();
    assert!(memory.sessions.contains_key(CODEX));
    assert!(!memory.sessions.contains_key(beside));
    assert!(!memory.sessions.contains_key(CLAUDE));
    orca.assert_only_asked(4);
}

/// Everything about a project, its worktrees and its remote that a job
/// could change.
fn fingerprint(orca: &Orca, project: &Path, worktrees: &[PathBuf]) -> Vec<String> {
    let mut all = vec![
        git(project, &["for-each-ref"]),
        git(project, &["worktree", "list", "--porcelain"]),
        git(project, &["count-objects"]),
        git(&orca.rig.upstream, &["for-each-ref"]),
    ];
    for worktree in worktrees {
        all.push(git(worktree, &["status", "--porcelain"]));
    }
    let mut files: Vec<String> = files_under(&orca.workspaces)
        .into_iter()
        .map(|(path, bytes)| format!("{} {}", path.display(), bytes.len()))
        .collect();
    files.sort();
    all.extend(files);
    all
}

fn commit(worktree: &Path, file: &str) {
    fs::write(worktree.join(file), "work\n").unwrap();
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-q", "-m", "work"]);
}

#[test]
fn no_count_is_sent_while_orca_lists_a_worktree_that_is_in_none_of_its_folders() {
    let orca = Orca::new();
    let rig = &orca.rig;
    let mock = gate::watch();
    rig.sign_in(&mock.url);

    let project = orca.project("web", false);
    let near = orca.worktree(
        &project,
        &orca.workspaces.join("task-near"),
        "orca/near",
        None,
    );
    fs::write(near.join("notes.txt"), "not committed\n").unwrap();
    // Orca was told to keep this one somewhere else, and the config does
    // not name the place.
    let elsewhere = rig.home.dir.join("elsewhere");
    let far = orca.worktree(&project, &elsewhere.join("task-far"), "orca/far", None);
    fs::write(far.join("notes.txt"), "not committed either\n").unwrap();
    // Orca also lists the project it was pointed at, which is its owner's,
    // and a nucleus session opened in it, which the nucleus counts.
    fs::write(project.join("scratch.txt"), "the owner's own\n").unwrap();
    let session = rig.session("fix-login");
    fs::write(session.join("notes.txt"), "the nucleus's\n").unwrap();
    let listed = |path: &Path| json!({ "workspaceKind": "git", "path": path, "agents": [] });
    orca.answers(
        &json!({ "ok": true, "result": { "truncated": false, "worktrees": [
        listed(&near), listed(&far), listed(&project), listed(&session),
    ] } }),
    );
    assert_eq!(orca.look().condition, Condition::Ok);

    let beat = |settings: &nucleate::settings::Settings| {
        let _ = fs::remove_file(rig.home.state_dir().join("unsaved.json"));
        Reporter::default()
            .running_with(rig.looker())
            .tick(settings, SystemTime::now());
        gate::tick(rig);
        mock.to(contract::HEARTBEATS).pop().unwrap().json()
    };
    // One uncommitted worktree would be a number, and it would be wrong.
    let before = beat(&rig.settings());
    assert_eq!(before["environments"][1]["env_id"], "orca");
    assert!(before["environments"][1].get("unsaved").is_none());
    assert!(before["machine"].get("uncommitted_repos").is_none());
    assert_eq!(before["environments"][0]["unsaved"]["uncommitted_repos"], 1);
    let status = run(rig.nucleated().arg("status"), "");
    let status = String::from_utf8_lossy(&status.stdout).to_string();
    assert!(
        status.contains("1 worktree outside its folders"),
        "{status}"
    );

    // Named in `driver_dirs`, it is counted with the rest. The project and
    // the nucleus's session were never in the way.
    orca.configure(&format!("driver_dirs = [\"{}\"]\n", elsewhere.display()));
    assert_eq!(orca.look().condition, Condition::Ok);
    let after = beat(&rig.settings());
    assert_eq!(after["environments"][1]["unsaved"]["uncommitted_repos"], 2);
    assert_eq!(after["environments"][0]["unsaved"]["uncommitted_repos"], 1);
    assert_eq!(after["machine"]["uncommitted_repos"], 3);
    orca.assert_only_asked(2);
    gate::assert_clean(rig, &mock);
}

#[test]
fn unsaved_work_in_orcas_worktrees_is_counted_with_orca_stopped_and_nothing_is_touched() {
    let orca = Orca::new();
    let rig = &orca.rig;
    // Orca is not running, and its program is not even there.
    fs::remove_file(&orca.cli).unwrap();
    let more = rig.home.dir.join("more-worktrees");
    fs::create_dir_all(&more).unwrap();
    orca.configure(&format!("driver_dirs = [\"{}\"]\n", more.display()));
    let mock = gate::watch();
    rig.sign_in(&mock.url);

    let project = orca.project("web", false);
    let dirty = orca.worktree(
        &project,
        &orca.workspaces.join("task-dirty"),
        "orca/dirty",
        None,
    );
    fs::write(dirty.join("notes.txt"), "not committed\n").unwrap();
    // Two commits on a branch that tracks nothing, in the nested layout.
    let ahead = orca.worktree(
        &project,
        &orca.workspaces.join("web").join("task-ahead"),
        "orca/ahead",
        None,
    );
    commit(&ahead, "a.txt");
    commit(&ahead, "b.txt");
    let clean = orca.worktree(
        &project,
        &orca.workspaces.join("task-clean"),
        "orca/clean",
        None,
    );
    // One commit ahead of the branch it tracks, in a folder of `driver_dirs`.
    let tracked = orca.worktree(
        &project,
        &more.join("task-tracked"),
        "orca/tracked",
        Some("origin/main"),
    );
    commit(&tracked, "c.txt");
    // A folder workspace has no git, and nothing to count.
    fs::create_dir_all(orca.workspaces.join("notes").join("drafts")).unwrap();
    fs::write(orca.workspaces.join("notes").join("todo.txt"), "todo\n").unwrap();
    // The project is its owner's, not Orca's: work in it is not Orca's to
    // count, and a link to it from Orca's folder is not followed.
    fs::write(project.join("scratch.txt"), "the owner's own\n").unwrap();
    std::os::unix::fs::symlink(&project, orca.workspaces.join("linked-project")).unwrap();

    let settings = rig.settings();
    let environment = settings.environment("orca").unwrap();
    let adapter = environment.driver_adapter().unwrap();
    let mut expected = vec![ahead.clone(), clean.clone(), dirty.clone(), tracked.clone()];
    expected.sort();
    assert_eq!(adapter.worktrees(environment), Some(expected.clone()));

    let before = fingerprint(&orca, &project, &expected);
    Reporter::default()
        .running_with(rig.looker())
        .tick(&settings, SystemTime::now());
    gate::tick(rig);
    let beat = mock.to(contract::HEARTBEATS)[0].json();
    let seen = &beat["environments"][1];
    assert_eq!(seen["env_id"], "orca");
    assert_eq!(seen["unsaved"]["uncommitted_repos"], 1);
    assert_eq!(seen["unsaved"]["unpushed_commits"], 3);
    // Orca's worktrees are never snapshotted, so there is no time to give.
    assert!(seen.get("last_snapshot_at").is_none());
    assert_eq!(beat["environments"][0]["unsaved"]["uncommitted_repos"], 0);
    assert_eq!(beat["machine"]["uncommitted_repos"], 1);
    assert_eq!(beat["machine"]["unpushed_commits"], 3);

    // Every housekeeping job, run for real over the whole config, and asked
    // for by hand for Orca's environment: nothing of Orca's has moved.
    // The login's own nucleus is the container's, and its jobs run in there.
    rig.looker();
    let path = common::path_with(&rig.home, &[]);
    for job in ["refresh", "cleanup", "snapshot"] {
        let all = run(
            rig.nucleated()
                .env("PATH", &path)
                .args(["housekeeping", "run", job]),
            "",
        );
        assert!(all.status.success(), "{job}: {}", stderr(&all));
        let reports = stdout_json(&all);
        let ran: Vec<&Value> = reports
            .as_array()
            .unwrap()
            .iter()
            .map(|report| &report["environment"])
            .collect();
        assert_eq!(ran, [&json!("client-a")], "{job}: {}", stderr(&all));
        let by_hand = run(
            rig.nucleated()
                .args(["housekeeping", "run", job, "--env", "orca"]),
            "",
        );
        assert!(!by_hand.status.success(), "{job} ran for Orca");
    }
    assert_eq!(fingerprint(&orca, &project, &expected), before);
    gate::assert_clean(rig, &mock);

    // A folder that cannot be read is a count that is not known, and no
    // number is put in its place.
    fs::remove_dir_all(&more).unwrap();
    assert_eq!(adapter.worktrees(environment), None);
    fs::remove_file(rig.home.state_dir().join("unsaved.json")).unwrap();
    Reporter::default()
        .running_with(rig.looker())
        .tick(&settings, SystemTime::now());
    gate::tick(rig);
    let beat = mock.to(contract::HEARTBEATS).pop().unwrap().json();
    assert!(beat["environments"][1].get("unsaved").is_none());
    assert_eq!(beat["environments"][0]["unsaved"]["uncommitted_repos"], 0);
    assert!(beat["machine"].get("uncommitted_repos").is_none());
}

// ---- the privacy gate -------------------------------------------------------
//
// If any test below fails, content can leave the box. Do not release.

/// Every field of Orca's answer that holds content, a path or a name a
/// person chose, and some it does not have yet.
const CONTENT_FIELDS: [&str; 34] = [
    "prompt",
    "taskTitle",
    "displayName",
    "conversationName",
    "lastAssistantMessage",
    "toolName",
    "toolInput",
    "agentWait",
    "programStatus",
    "children",
    "subagents",
    "paneKey",
    "address",
    "preview",
    "comment",
    "linkedPR",
    "linkedItems",
    "linkedLinearIssue",
    "automationName",
    "bot",
    "removalError",
    "recipeEnvironment",
    "worktreeId",
    "priorWorktreeIds",
    "parentWorktreeId",
    "childWorktreeIds",
    "repo",
    "branch",
    "installationId",
    // Not Orca's today.
    "transcript",
    "errorMessage",
    "title",
    "notes",
    "files",
];

#[test]
fn the_sentinel_fixture_would_catch_a_leak() {
    gate::assert_fixture_would_catch_a_leak(SENTINEL, 400, &CONTENT_FIELDS);
    // The sentinel also sits where each field that is read belongs.
    for read in [
        "orcaSessionId",
        "state",
        "agentType",
        "updatedAt",
        "restoredUnconfirmed",
        "outcome",
        "workspaceKind",
        "path",
    ] {
        assert!(
            SENTINEL.contains(&format!("\"{read}\": \"{}", gate::MARK)),
            "{read} never holds the sentinel"
        );
    }
}

/// An id in the sentinel fixture: one hex digit, in the shape of a UUID.
fn id(digit: char) -> String {
    let run = |length: usize| digit.to_string().repeat(length);
    format!("{}-{}-4{}-8{}-{}", run(8), run(4), run(3), run(3), run(12))
}

#[test]
fn no_content_orca_hands_over_reaches_the_wire_the_disk_or_the_status() {
    let mark = gate::MARK;
    // Everything of Orca's is under a folder named with the sentinel, and so
    // is each worktree, each file in it and each commit.
    let orca = Orca::under(&format!("{mark}_orca_parent"));
    let rig = &orca.rig;
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let on_github = orca.project("web", true);
    let elsewhere = orca.project(&format!("{mark}-private-project"), false);
    let first = orca.worktree(
        &on_github,
        &orca.workspaces.join(format!("{mark}-task-1")),
        "orca/task-7",
        Some("origin/main"),
    );
    let second = orca.worktree(
        &elsewhere,
        &orca.workspaces.join(format!("{mark}-task-2")),
        "orca/task-8",
        None,
    );
    for worktree in [&first, &second] {
        fs::write(
            worktree.join(format!("{mark}_committed.txt")),
            gate::mark("file content"),
        )
        .unwrap();
        git(worktree, &["add", "."]);
        let subject = gate::mark("commit subject");
        git(worktree, &["commit", "-q", "-m", &subject]);
        fs::write(
            worktree.join(format!("{mark}_uncommitted.txt")),
            gate::mark("file content"),
        )
        .unwrap();
    }
    let notes = orca.workspaces.join(format!("{mark}-notes"));
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join(format!("{mark}.md")), gate::mark("a note")).unwrap();

    orca.answers(&orca.fixture(SENTINEL));
    orca.look();
    // Each agent with an id and a state is reported, by those alone. One
    // with content where its id or its state belongs is not.
    let (working, asking) = ("user_prompt_submit", "input_prompt");
    assert_eq!(
        spooled(rig, 0),
        events(&[
            (&id('1'), "session_start"),
            (&id('1'), working),
            (&id('5'), "session_start"),
            (&id('5'), asking),
            (&id('7'), "session_start"),
            (&id('7'), "stop_failure"),
            (&id('8'), "session_start"),
            (&id('8'), working),
            (&id('9'), "session_start"),
            (&id('9'), working),
            (&id('a'), "session_start"),
            (&id('a'), working),
            (&id('b'), "session_start"),
            (&id('b'), asking),
            (&id('c'), "session_start"),
            (&id('c'), working),
        ])
    );
    // A repo on GitHub is named as GitHub names it, with the branch.
    let placed = the_event(rig, &id('1'), "session_start");
    assert_eq!(placed["repos"], json!(["acme/web"]));
    assert_eq!(placed["branch"], "orca/task-7");
    assert_eq!(
        the_event(rig, &id('7'), "stop_failure")["error_kind"],
        "failure"
    );
    // A worktree of a repo that is not: its folder is named after the task,
    // which is no repo's name, so nothing is said of where it is. The same
    // for a kind that is not a kind and a path that is not a path.
    for unplaced in ['9', 'a', 'b', 'c'] {
        let event = the_event(rig, &id(unplaced), "session_start");
        assert!(event.get("repos").is_none(), "{event}");
        assert!(event.get("branch").is_none(), "{event}");
    }

    // Claude Code's own hook speaks from inside the first worktree, and the
    // agent Orca lists there is left to it.
    let inside = first.join("src").join(format!("{mark}_subfolder"));
    fs::create_dir_all(&inside).unwrap();
    hook(rig, "UserPromptSubmit", &inside);
    gate::tick(rig);
    let before = rig.spooled().len();
    orca.look();
    assert_eq!(spooled(rig, before), events(&[(&id('1'), "session_end")]));

    // The count of what the worktrees hold, the daemon, and what is shown.
    Reporter::default()
        .running_with(rig.looker())
        .tick(&rig.settings(), SystemTime::now());
    gate::tick(rig);
    // The heartbeat after the count, from the daemon that ticked last.
    let beat = gate::said(&mock);
    assert_eq!(beat["environments"][1]["unsaved"]["uncommitted_repos"], 2);
    assert_eq!(beat["environments"][1]["unsaved"]["unpushed_commits"], 2);
    assert!(!mock.to(contract::EVENTS).is_empty());
    gate::assert_clean(rig, &mock);

    // Nor is the sentinel in the name of anything the box keeps: not in the
    // state folder, and not in what is remembered of Orca.
    let kept = files_under(&rig.home.state_dir());
    assert!(kept
        .iter()
        .any(|(path, _)| path.ends_with("drivers/orca.json")));
    assert!(kept.iter().any(|(path, _)| path.ends_with("unsaved.json")));
    let state_dir = rig.home.state_dir();
    for (path, _) in &kept {
        let name = path.strip_prefix(&state_dir).unwrap().to_string_lossy();
        assert!(!name.contains(mark), "a name on disk: {name}");
    }
    orca.assert_only_asked(2);
}
