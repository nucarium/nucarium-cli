//! Herdr: what the two adapters do, and their part of the privacy gate.
//!
//! The gate's shared pieces are in `common::gate`. These adapters' fixtures,
//! stand-in herdr-core and Herdr TUI sockets whose every answer is full of
//! the sentinel, and the test that each stand-in was sent only the one read
//! call the design names, are in this file.
//!
//! The stand-ins speak what the two products document: herdr-core's
//! `schema/README.md` and `hc.openrpc.json` (JSON-RPC 2.0, one message per
//! line, `terminal.list` answering `{terminals, hidden_finished, machines}`),
//! and the Herdr TUI's `socket-api` page and `AgentInfo` (one JSON message
//! per line, `agent.list` answering `{type: "agent_list", agents}`).

mod common;

use common::gate::{mark, MARK};
use common::{contract, gate, run, Rig, CLIENT_A_CONTAINER_UNIT};
use nucleate::driver::{Condition, Memory, LOOK_LIMIT};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The one line herdr-core is ever sent, and the one line the Herdr TUI is.
const TERMINAL_LIST: &str =
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"terminal.list\",\"params\":{}}\n";
const AGENT_LIST: &str = "{\"id\":\"n1\",\"method\":\"agent.list\",\"params\":{}}\n";

const HOOKED: &str = "7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a";
const CREATED: u64 = 1_795_140_000_000;

// ---- a stand-in Herdr socket ------------------------------------------------

/// What the stand-in does with a connection once it has read its line.
enum Answer {
    /// Write this, and a newline.
    Line(String),
    /// Keep the connection open and say nothing.
    Silence,
    /// Hang up.
    Close,
}

struct Told {
    answer: Answer,
    received: Vec<String>,
    held: Vec<UnixStream>,
}

/// A server on a Unix socket in a folder of the test's own. It records every
/// line it is sent, and answers each connection with whatever it was last
/// told to.
struct Fake {
    path: PathBuf,
    told: Arc<Mutex<Told>>,
    _dir: tempfile::TempDir,
}

impl Fake {
    fn start() -> Fake {
        // A socket's path may be no longer than about a hundred bytes.
        let dir = tempfile::Builder::new().prefix("hd").tempdir().unwrap();
        let path = dir.path().join("h.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let told = Arc::new(Mutex::new(Told {
            answer: Answer::Close,
            received: Vec::new(),
            held: Vec::new(),
        }));
        let shared = told.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let mut told = shared.lock().unwrap();
                told.received.push(line);
                match &told.answer {
                    Answer::Line(answer) => {
                        let mut out = &stream;
                        // A client that has heard enough hangs up mid-answer.
                        let _ = out
                            .write_all(answer.as_bytes())
                            .and_then(|()| out.write_all(b"\n"));
                    }
                    Answer::Silence => told.held.push(stream),
                    Answer::Close => {}
                }
            }
        });
        Fake {
            path,
            told,
            _dir: dir,
        }
    }

    fn say(&self, answer: Answer) {
        self.told.lock().unwrap().answer = answer;
    }

    /// Answer `terminal.list` as herdr-core does.
    fn terminals(&self, terminals: Value) -> String {
        let answer = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "terminals": terminals, "hidden_finished": 0, "machines": [] },
        })
        .to_string();
        self.say(Answer::Line(answer.clone()));
        answer
    }

    /// Answer `agent.list` as the Herdr TUI does.
    fn agents(&self, agents: Value) -> String {
        let answer =
            json!({ "id": "n1", "result": { "type": "agent_list", "agents": agents } }).to_string();
        self.say(Answer::Line(answer.clone()));
        answer
    }

    fn received(&self) -> Vec<String> {
        self.told.lock().unwrap().received.clone()
    }
}

// ---- a box that watches one -------------------------------------------------

/// Add a `herdr` environment watched through `driver` on `socket`.
fn watch_herdr(rig: &Rig, driver: &str, socket: &Path) {
    rig.configure(&format!(
        "[[environment]]\nname = \"herdr\"\nroot = \"{}\"\ndriver = \"{driver}\"\ncheck = \"true\"\n\
         driver_socket = \"{}\"\n",
        rig.root.display(),
        socket.display(),
    ));
}

/// A box with a `herdr` environment watched through `driver`, whose socket
/// is a path nothing listens on.
fn rig(driver: &str) -> Rig {
    let rig = Rig::new();
    watch_herdr(&rig, driver, &rig.home.dir.join("no-such.sock"));
    rig
}

/// A box with a `herdr` environment watched through `driver`, and the
/// stand-in it is watched on.
fn rig_with(driver: &str) -> (Rig, Fake) {
    let rig = Rig::new();
    let fake = Fake::start();
    watch_herdr(&rig, driver, &fake.path);
    (rig, fake)
}

/// As [`rig_with`], with Herdr running in `client-a`'s container, beside
/// the agent's login, as the recipe runs a driver.
fn rig_in_container(driver: &str) -> (Rig, Fake) {
    let rig = Rig::new();
    let fake = Fake::start();
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"herdr\"\nroot = \"{}\"\ndriver = \"{driver}\"\ncheck = \"true\"\n\
         {CLIENT_A_CONTAINER_UNIT}driver_socket = \"{}\"\n",
        rig.root.display(),
        fake.path.display(),
    ));
    (rig, fake)
}

/// The moment of the `n`th look of a test: a second apart, so that what one
/// look writes sorts before what the next does.
fn at(n: u64) -> SystemTime {
    SystemTime::now() + Duration::from_secs(n)
}

fn look(rig: &Rig, n: u64) -> Memory {
    common::look(rig, "herdr", at(n))
}

/// What is waiting in the spool, as (session, event type), oldest first.
fn spooled(rig: &Rig) -> Vec<(String, String)> {
    rig.spooled()
        .iter()
        .map(|record| {
            assert_eq!(record["event"]["env_id"], "herdr");
            (
                record["event"]["session_id"].as_str().unwrap().to_string(),
                record["event"]["type"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Empty the spool, returning what was in it.
fn drain(rig: &Rig) -> Vec<Value> {
    let held = rig.spooled();
    for entry in std::fs::read_dir(rig.spool_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        std::fs::remove_file(entry.path()).unwrap();
    }
    held
}

/// Take one look, and return what it wrote to the spool, leaving it empty.
fn looked(rig: &Rig, n: u64) -> Vec<(String, String)> {
    let memory = look(rig, n);
    assert_eq!(memory.condition, Condition::Ok);
    let wrote = spooled(rig);
    drain(rig);
    wrote
}

fn events(session: &str, kinds: &[&str]) -> Vec<(String, String)> {
    kinds
        .iter()
        .map(|kind| (session.to_string(), kind.to_string()))
        .collect()
}

/// Call the hook entry point for `client-a`, inside its container, from a
/// process with `vars` set.
fn hook(rig: &Rig, event: &str, session: &str, cwd: &Path, vars: &[(&str, &str)]) {
    let payload = json!({ "session_id": session, "cwd": cwd });
    let mut command = rig.nucleated();
    rig.in_container(&mut command);
    command.args(["hook", event, "--env", "client-a"]);
    for (name, value) in vars {
        command.env(name, value);
    }
    let out = run(&mut command, &payload.to_string());
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

fn core_id(address: &str) -> String {
    format!("hc:{address}:{CREATED}")
}

/// One herdr-core terminal with nothing in it but what the adapter reads.
fn terminal(address: &str, status: Value) -> Value {
    json!({
        "address": address, "state": "live", "created_ms": CREATED, "reported": true,
        "labels": {}, "argv": ["agent"], "cwd": "/", "screen": "exact", "cols": 80, "rows": 24,
        "status": status,
    })
}

/// One Herdr TUI agent with nothing in it but what the adapter reads.
fn agent(terminal: &str, pane: &str, status: &str) -> Value {
    json!({
        "terminal_id": terminal, "pane_id": pane, "agent_status": status,
        "workspace_id": "w1", "tab_id": "w1:t1", "focused": false, "revision": 1,
    })
}

// ---- when Herdr is not there, or not itself ---------------------------------

#[test]
fn with_no_herdr_socket_there_the_environment_is_alive_and_nothing_else_is_said() {
    for name in ["herdr-core", "herdr-tui"] {
        let rig = rig(name);
        let mock = gate::watch_ahead();
        rig.sign_in(&mock.url);
        let memory = common::look(&rig, "herdr", SystemTime::now());
        assert_eq!(memory.condition, Condition::Absent, "{name}");
        assert!(memory.sessions.is_empty(), "{name}");
        assert!(rig.spooled().is_empty(), "{name}");

        gate::tick(&rig);
        let beat = gate::said(&mock);
        let herdr = &beat["environments"][1];
        assert_eq!(herdr["env_id"], "herdr");
        assert_eq!(herdr["driver"], name);
        // Whether the server is alive is its supervisor's to say, not the adapter's.
        assert_eq!(herdr["server_alive"], true);
        assert!(herdr.get("driver_ref").is_none());
        assert!(mock.to(contract::EVENTS).is_empty(), "{name}");
        gate::assert_clean(&rig, &mock);
    }
}

#[test]
fn a_herdr_that_stops_answering_properly_ends_nothing_and_says_nothing() {
    for name in ["herdr-core", "herdr-tui"] {
        let (rig, fake) = rig_with(name);
        let working = |fake: &Fake| match name {
            "herdr-core" => fake.terminals(json!([terminal("t7", json!({ "state": "working" }))])),
            _ => fake.agents(json!([agent("term_a1", "w1:p1", "working")])),
        };
        working(&fake);
        assert_eq!(looked(&rig, 0).len(), 2, "{name}");

        let padding = "x".repeat(5 * 1024 * 1024);
        let broken = [
            (Answer::Line("not json at all".into()), Condition::BadReply),
            (Answer::Line("[1, 2, 3]".into()), Condition::BadReply),
            (
                Answer::Line("{\"id\":1,\"result\":{}}".into()),
                Condition::BadReply,
            ),
            (
                Answer::Line("{\"id\":1,\"result\":\"soon\"}".into()),
                Condition::BadReply,
            ),
            (Answer::Close, Condition::BadReply),
            // An answer longer than any real one is not read to its end.
            (
                Answer::Line(format!("{{\"padding\":\"{padding}\",\"result\":{{}}}}")),
                Condition::BadReply,
            ),
            // The server answered the call with an error, such as a newer
            // or older Herdr that does not know it. What the error says is
            // not read.
            (
                Answer::Line(
                    json!({ "id": 1, "error": { "code": -32601, "message": "no such method" } })
                        .to_string(),
                ),
                Condition::Error,
            ),
        ];
        for (n, (answer, condition)) in broken.into_iter().enumerate() {
            fake.say(answer);
            let memory = look(&rig, 1 + n as u64);
            assert_eq!(memory.condition, condition, "{name}, answer {n}");
            assert_eq!(memory.sessions.len(), 1, "{name}, answer {n}");
            assert!(rig.spooled().is_empty(), "{name}, answer {n}");
        }

        // A socket file nothing listens on any more.
        let stale = rig.home.dir.join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap());
        watch_herdr(&rig, name, &stale);
        let memory = look(&rig, 20);
        assert_eq!(memory.condition, Condition::Unreachable, "{name}");
        assert_eq!(memory.sessions.len(), 1, "{name}");
        assert!(rig.spooled().is_empty(), "{name}");

        // When it is back, the session is where it was, and nothing is said twice.
        watch_herdr(&rig, name, &fake.path);
        working(&fake);
        assert_eq!(looked(&rig, 21), Vec::new(), "{name}");
    }
}

#[test]
fn a_herdr_that_takes_the_call_and_never_answers_costs_one_short_look() {
    for name in ["herdr-core", "herdr-tui"] {
        let (rig, fake) = rig_with(name);
        fake.say(Answer::Silence);
        let started = Instant::now();
        let memory = look(&rig, 0);
        assert!(started.elapsed() < LOOK_LIMIT, "{name}");
        assert_eq!(memory.condition, Condition::Timeout, "{name}");
        assert!(rig.spooled().is_empty(), "{name}");
    }
}

// ---- herdr-core --------------------------------------------------------------

#[test]
fn a_herdr_core_terminal_is_followed_from_its_first_word_to_its_last() {
    let (rig, fake) = rig_with("herdr-core");
    let worktree = rig.session("fix-login");
    let id = core_id("t7");
    let said = |status: Value, turn: u64| {
        let mut one = terminal("t7", status);
        one["turn"] = json!(turn);
        one["cwd"] = json!(rig.root);
        one["foreground"] = json!({ "pid": 1, "name": "node", "argv": ["node"], "cwd": worktree });
        // A shell beside it, whose program never reported a status.
        let mut shell = terminal("t2", Value::Null);
        shell["reported"] = json!(false);
        fake.terminals(json!([shell, one]));
    };

    said(json!({ "state": "working" }), 0);
    assert_eq!(
        looked(&rig, 0),
        events(&id, &["session_start", "user_prompt_submit"])
    );
    // Where it works is used to say which repo and branch, and no more.
    said(json!({ "state": "blocked", "kind": "permission" }), 1);
    let memory = look(&rig, 1);
    assert_eq!(memory.condition, Condition::Ok);
    let asked = drain(&rig);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["event"]["type"], "permission_prompt");
    assert_eq!(asked[0]["event"]["repos"], json!(["web"]));
    assert_eq!(asked[0]["event"]["branch"], "workspace/fix-login");
    assert!(asked[0]["event"].get("bridge_session_id").is_none());
    assert_eq!(looked(&rig, 2), Vec::new());

    said(json!({ "state": "working" }), 1);
    assert_eq!(looked(&rig, 3), events(&id, &["activity"]));
    said(json!({ "state": "blocked", "kind": "question" }), 2);
    assert_eq!(looked(&rig, 4), events(&id, &["input_prompt"]));
    said(json!({ "state": "working" }), 2);
    assert_eq!(looked(&rig, 5), events(&id, &["activity"]));
    said(json!({ "state": "done" }), 3);
    assert_eq!(looked(&rig, 6), events(&id, &["stop"]));
    // Done and idle differ only in whether a person has looked.
    said(json!({ "state": "idle" }), 3);
    assert_eq!(looked(&rig, 7), Vec::new());
    // A whole turn between two looks shows in the turn counter alone.
    said(json!({ "state": "idle" }), 4);
    assert_eq!(
        looked(&rig, 8),
        events(&id, &["user_prompt_submit", "stop"])
    );
    said(json!({ "state": "working" }), 4);
    assert_eq!(looked(&rig, 9), events(&id, &["user_prompt_submit"]));
    // herdr-core says why only in the program's own words, which are not read.
    said(json!({ "state": "error", "msg": "the model said no" }), 5);
    let memory = look(&rig, 10);
    assert_eq!(memory.condition, Condition::Ok);
    let failed = drain(&rig);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["event"]["type"], "stop_failure");
    assert!(failed[0]["event"].get("error_kind").is_none());
    assert!(failed[0]["event"].get("limit_resets_at").is_none());
    assert_eq!(looked(&rig, 11), Vec::new());
    // A word this adapter does not know changes nothing.
    said(json!({ "state": "pondering" }), 5);
    assert_eq!(looked(&rig, 12), Vec::new());
    // The program ended and its shell came back: the status is cleared.
    said(Value::Null, 5);
    assert_eq!(looked(&rig, 13), events(&id, &["session_end"]));
    assert_eq!(looked(&rig, 14), Vec::new());
    // Another agent started in the same terminal is a session again.
    said(json!({ "state": "working" }), 5);
    assert_eq!(
        looked(&rig, 15),
        events(&id, &["session_start", "user_prompt_submit"])
    );

    assert!(fake.received().iter().all(|line| line == TERMINAL_LIST));
    assert_eq!(fake.received().len(), 16);
}

#[test]
fn a_herdr_core_terminal_that_is_gone_is_ended_once_a_second_look_agrees() {
    let (rig, fake) = rig_with("herdr-core");
    fake.terminals(json!([
        terminal("t7", json!({ "state": "working" })),
        terminal("t8", json!({ "state": "working" })),
    ]));
    assert_eq!(looked(&rig, 0).len(), 4);
    // One finished, which the list says in so many words when asked to.
    let mut finished = terminal("t8", Value::Null);
    finished["state"] = json!("finished");
    fake.terminals(json!([finished]));
    assert_eq!(looked(&rig, 1), events(&core_id("t8"), &["session_end"]));
    fake.terminals(json!([]));
    assert_eq!(looked(&rig, 2), events(&core_id("t7"), &["session_end"]));
    assert_eq!(looked(&rig, 3), Vec::new());
}

#[test]
fn a_terminal_on_another_machine_is_never_reported_from_this_one() {
    let (rig, fake) = rig_with("herdr-core");
    fake.terminals(json!([
        terminal("buildbox/t7", json!({ "state": "working" })),
        terminal("t3", json!({ "state": "working" })),
    ]));
    assert_eq!(
        looked(&rig, 0),
        events(&core_id("t3"), &["session_start", "user_prompt_submit"])
    );
}

#[test]
fn a_herdr_core_terminal_whose_agent_has_hooks_of_its_own_is_left_to_them() {
    let (rig, fake) = rig_in_container("herdr-core");
    let worktree = rig.session("fix-login");
    let socket = fake.path.to_str().unwrap();
    // Claude Code in terminal t7 of this core.
    hook(
        &rig,
        "SessionStart",
        HOOKED,
        &worktree,
        &[("HC_TERMINAL_ID", "t7"), ("HC_TERMINAL_SOCKET", socket)],
    );
    // Another in a terminal t8 of some other core, which is not this t8.
    hook(
        &rig,
        "SessionStart",
        "0b8e6a7c-1f7b-4a53-9a44-6a1f3f1f8f10",
        &worktree,
        &[
            ("HC_TERMINAL_ID", "t8"),
            ("HC_TERMINAL_SOCKET", "/run/user/1/herdr-core/scratch.sock"),
        ],
    );
    assert_eq!(rig.spooled_in_container().len(), 2);
    // The daemon takes the hooks' events in, and with them their claims.
    gate::tick(&rig);
    assert!(rig.spooled_in_container().is_empty());

    fake.terminals(json!([
        terminal("t7", json!({ "state": "working" })),
        terminal("t8", json!({ "state": "working" })),
    ]));
    let memory = look(&rig, 0);
    assert_eq!(
        memory.sessions.keys().collect::<Vec<_>>(),
        vec![&core_id("t8")]
    );
    let from_driver: Vec<Value> = rig
        .spooled()
        .into_iter()
        .filter(|record| record["event"]["env_id"] == "herdr")
        .collect();
    assert_eq!(from_driver.len(), 2);
    assert!(from_driver
        .iter()
        .all(|record| record["event"]["session_id"] == core_id("t8").as_str()));
}

// ---- Herdr TUI ---------------------------------------------------------------

#[test]
fn a_herdr_tui_agent_is_followed_from_its_first_word_to_its_last() {
    let (rig, fake) = rig_with("herdr-tui");
    let worktree = rig.session("fix-login");
    let id = "herdr:term_65f1a2b3c4d5e1";
    let said = |status: &str, completed: Option<u64>| {
        let mut one = agent("term_65f1a2b3c4d5e1", "w1:p2", status);
        one["completion_seq"] = json!(completed);
        one["cwd"] = json!(rig.root);
        one["foreground_cwd"] = json!(worktree);
        fake.agents(json!([one]));
    };

    // Herdr is not sure yet, and neither is anyone else.
    said("unknown", None);
    assert_eq!(looked(&rig, 0), Vec::new());
    said("working", None);
    let memory = look(&rig, 1);
    assert_eq!(memory.condition, Condition::Ok);
    let started = drain(&rig);
    assert_eq!(started.len(), 2);
    assert_eq!(started[0]["event"]["type"], "session_start");
    assert_eq!(started[0]["event"]["session_id"], id);
    assert_eq!(started[0]["event"]["repos"], json!(["web"]));
    assert_eq!(started[0]["event"]["branch"], "workspace/fix-login");
    assert_eq!(looked(&rig, 2), Vec::new());

    // Herdr says an agent is blocked, and not on what.
    said("blocked", None);
    assert_eq!(looked(&rig, 3), events(id, &["input_prompt"]));
    said("working", None);
    assert_eq!(looked(&rig, 4), events(id, &["activity"]));
    said("unknown", None);
    assert_eq!(looked(&rig, 5), Vec::new());
    said("done", Some(41));
    assert_eq!(looked(&rig, 6), events(id, &["stop"]));
    // Done and idle differ only in whether a person has looked.
    said("idle", Some(41));
    assert_eq!(looked(&rig, 7), Vec::new());
    // Work that began and was completed between two looks.
    said("done", Some(47));
    assert_eq!(looked(&rig, 8), events(id, &["user_prompt_submit", "stop"]));
    // The pane went back to its shell: the agent is no longer listed.
    fake.agents(json!([]));
    assert_eq!(looked(&rig, 9), Vec::new());
    assert_eq!(looked(&rig, 10), events(id, &["session_end"]));

    assert!(fake.received().iter().all(|line| line == AGENT_LIST));
    assert_eq!(fake.received().len(), 11);
}

#[test]
fn a_herdr_tui_agent_that_has_hooks_of_its_own_is_left_to_them() {
    let (rig, fake) = rig_in_container("herdr-tui");
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let worktree = rig.session("fix-login");
    let mut sessions = [HOOKED, "5d2c9f3e-7a41-4b8e-8f0d-3c6b1a2e4f50"].into_iter();
    let here = fake.path.to_str().unwrap();
    let mut in_pane = |pane: &'static str, socket: &str| {
        let vars = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", pane),
            ("HERDR_SOCKET_PATH", socket),
        ];
        hook(
            &rig,
            "SessionStart",
            sessions.next().unwrap(),
            &worktree,
            &vars,
        );
    };
    // Claude Code in pane w1:p1 of this server.
    in_pane("w1:p1", here);
    // And one in a pane w1:p3 of some other Herdr server, which is not this
    // server's w1:p3.
    in_pane(
        "w1:p3",
        "/home/agent/.config/herdr/sessions/other/herdr.sock",
    );
    // Another whose hook ran with none of Herdr's variables, but whose own
    // session id Herdr's integration passed on.
    let unmarked = "0b8e6a7c-1f7b-4a53-9a44-6a1f3f1f8f10";
    hook(&rig, "SessionStart", unmarked, &worktree, &[]);
    // The daemon takes the hooks' events in, so their sessions are known.
    gate::tick(&rig);
    assert!(rig.spooled().is_empty());
    assert!(rig.spooled_in_container().is_empty());

    let mut second = agent("term_b2", "w1:p2", "working");
    second["agent_session"] =
        json!({ "source": "herdr:claude", "agent": "claude", "kind": "id", "value": unmarked });
    fake.agents(json!([
        agent("term_a1", "w1:p1", "working"),
        second,
        agent("term_c3", "w1:p3", "working"),
    ]));
    assert_eq!(
        looked(&rig, 0),
        events("herdr:term_c3", &["session_start", "user_prompt_submit"])
    );
}

// ---- the privacy gate --------------------------------------------------------

#[test]
fn on_a_box_that_sends_names_herdr_s_own_title_for_the_agent_is_its_name() {
    let names = |rig: &Rig| -> Vec<Option<String>> {
        drain(rig)
            .iter()
            .map(|record| record["event"]["session_name"].as_str().map(str::to_string))
            .collect()
    };
    for on in [false, true] {
        // herdr-core: the program's own label in its 7501 status, never the
        // terminal's name or its window title.
        let (rig, fake) = rig_with("herdr-core");
        if on {
            rig.send_names();
        }
        let mut one = terminal(
            "t7",
            json!({ "state": "working", "title": "Speed up\nthe login page" }),
        );
        one["name"] = json!("reviewer");
        one["title"] = json!("node ~/work");
        fake.terminals(json!([one]));
        assert_eq!(look(&rig, 0).condition, Condition::Ok);
        let sent = names(&rig);
        assert_eq!(sent.len(), 2, "names on: {on}");
        for name in sent {
            assert_eq!(name.as_deref(), on.then_some("Speed up the login page"));
        }

        // Herdr TUI: the title an integration reported, never the agent's
        // name or the terminal's own title.
        let (rig, fake) = rig_with("herdr-tui");
        if on {
            rig.send_names();
        }
        let mut one = agent("term_65f1a2b3c4d5e1", "w1:p2", "working");
        one["title"] = json!("Refactor auth middleware");
        one["name"] = json!("reviewer");
        one["terminal_title"] = json!("node ~/work");
        one["display_agent"] = json!("Claude: auth");
        fake.agents(json!([one]));
        assert_eq!(look(&rig, 0).condition, Condition::Ok);
        let sent = names(&rig);
        assert_eq!(sent.len(), 2, "names on: {on}");
        for name in sent {
            assert_eq!(name.as_deref(), on.then_some("Refactor auth middleware"));
        }
    }
}

/// The sentinel in every field of a herdr-core terminal that holds content,
/// around the few fields the adapter reads.
fn full_terminal(address: &str, status: Value, cwd: &Path) -> Value {
    let mut status = status;
    if let Some(record) = status.as_object_mut() {
        record.insert("app".into(), json!(mark("status.app")));
        record.insert("title".into(), json!(mark("status.title")));
        record.insert("msg".into(), json!(mark("status.msg")));
        record.insert("progress".into(), json!(40));
        record.insert(
            "detail".into(),
            json!(mark("a status field that does not exist yet")),
        );
    }
    json!({
        "address": address,
        "name": mark("name"),
        "argv": ["claude", mark("argv, which can hold a whole prompt")],
        "cwd": cwd,
        "state": "live",
        "reason": mark("reason"),
        "exit_code": null,
        "signal": null,
        "created_ms": CREATED,
        "finished_ms": null,
        "pid": 4242,
        "parent": mark("parent"),
        "labels": { mark("a label key"): mark("a label value") },
        "template": mark("template"),
        "first_message": { "text": mark("first_message"), "sent": false },
        "foreground": {
            "pid": 4243,
            "name": mark("foreground.name"),
            "argv": ["node", mark("foreground.argv")],
            "cwd": cwd,
            "environment": mark("a foreground field that does not exist yet"),
        },
        "identity": mark("identity"),
        "title": mark("title"),
        "status": status,
        "status_ms": CREATED + 5,
        "records": {
            mark("a record id"): {
                "state": "working", "title": mark("records.title"), "msg": mark("records.msg"),
            },
        },
        "reported": true,
        "turn": 3,
        "keep": false,
        "cols": 120,
        "rows": 40,
        "screen": "exact",
        "stop": mark("stop"),
        "submit": mark("submit"),
        "transcript": [mark("a terminal field that does not exist yet")],
    })
}

/// A whole `terminal.list` answer around `t7`, with the sentinel everywhere
/// herdr-core can put content and in places it does not put anything yet.
fn full_terminals(status: Value, cwd: &Path) -> Value {
    let mut worded = full_terminal("t8", json!({}), cwd);
    // Content where a short word belongs.
    worded["status"]["state"] = json!(mark("status.state"));
    worded["status"]["kind"] = json!(mark("status.kind"));
    let mut unstated = full_terminal("t9", json!({ "state": "working" }), cwd);
    unstated["state"] = json!(mark("state"));
    let mut unkind = full_terminal("t4", json!({ "state": "blocked" }), cwd);
    unkind["status"]["kind"] = json!(mark("status.kind"));
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "note": mark("a field of the answer that does not exist yet"),
        "result": {
            "terminals": [
                full_terminal("t7", status, cwd),
                worded,
                unstated,
                unkind,
                // Another machine's terminal, named for the machine.
                full_terminal(&format!("{MARK}-box/t3"), json!({ "state": "working" }), cwd),
                // An address that is no address.
                full_terminal(&mark("address"), json!({ "state": "working" }), cwd),
            ],
            "hidden_finished": 2,
            "machines": [{
                "name": mark("machines.name"),
                "target": format!("{MARK}_user@{MARK}.example.com"),
                "host": mark("machines.host"),
                "connected": true,
            }],
            "summary": mark("a result field that does not exist yet"),
        },
    })
}

#[test]
fn nothing_herdr_core_says_beside_the_state_leaves_the_box() {
    let (rig, fake) = rig_with("herdr-core");
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let cwd = rig
        .session("fix-login")
        .join("src")
        .join(format!("{MARK}_subfolder"));
    std::fs::create_dir_all(&cwd).unwrap();
    let say = |status: Value| {
        let answer = full_terminals(status, &cwd).to_string();
        fake.say(Answer::Line(answer.clone()));
        answer
    };

    let fixture = say(json!({ "state": "working" }));
    gate::assert_fixture_would_catch_a_leak(
        &fixture,
        120,
        &[
            "name",
            "argv",
            "cwd",
            "title",
            "labels",
            "template",
            "first_message",
            "identity",
            "parent",
            "records",
            "reason",
            "foreground",
            "app",
            "msg",
            "machines",
            "target",
            "host",
            "state",
            "kind",
            "address",
        ],
    );
    assert_eq!(look(&rig, 0).condition, Condition::Ok);
    say(json!({ "state": "blocked", "kind": "permission" }));
    look(&rig, 1);
    say(json!({ "state": "done" }));
    look(&rig, 2);
    say(json!({ "state": "error" }));
    look(&rig, 3);
    // The same, as one line too long to be an answer.
    let mut long = full_terminals(json!({ "state": "working" }), &cwd);
    long["padding"] = json!(MARK.repeat(5 * 1024 * 1024 / MARK.len()));
    fake.say(Answer::Line(long.to_string()));
    assert_eq!(look(&rig, 4).condition, Condition::BadReply);
    // Gone, and gone again.
    fake.terminals(json!([]));
    look(&rig, 5);
    let memory = look(&rig, 6);
    assert!(memory.sessions.is_empty());

    // A hook whose process claims to be in a terminal named with content.
    hook(
        &rig,
        "Stop",
        HOOKED,
        &cwd,
        &[
            ("HC_TERMINAL_ID", &mark("HC_TERMINAL_ID")),
            ("HC_TERMINAL_SOCKET", fake.path.to_str().unwrap()),
        ],
    );
    assert!(!rig.home.state_dir().join("claims").exists());

    let t7 = core_id("t7");
    let t4 = core_id("t4");
    let mut from_driver: Vec<(String, String)> = spooled_from(&rig, "herdr");
    from_driver.sort();
    let mut expected = events(
        &t7,
        &[
            "session_start",
            "user_prompt_submit",
            "permission_prompt",
            "stop",
            "stop_failure",
            "session_end",
        ],
    );
    // Blocked, on a kind that is not one of herdr-core's: the plain prompt.
    expected.extend(events(
        &t4,
        &["session_start", "input_prompt", "session_end"],
    ));
    expected.sort();
    assert_eq!(from_driver, expected);
    for record in rig.spooled() {
        for key in record["event"].as_object().unwrap().keys() {
            assert!(gate::EVENT_FIELDS.contains(&key.as_str()), "{key}");
        }
        assert!(record["event"].get("error_kind").is_none());
    }

    gate::tick(&rig);
    assert!(!mock.to(contract::EVENTS).is_empty());
    gate::assert_clean(&rig, &mock);
    // Observe only: the one read call, each time, and not a byte more.
    assert_eq!(fake.received(), vec![TERMINAL_LIST.to_string(); 7]);
}

/// What is in the spool for one environment, as (session, event type).
fn spooled_from(rig: &Rig, environment: &str) -> Vec<(String, String)> {
    rig.spooled()
        .iter()
        .filter(|record| record["event"]["env_id"] == environment)
        .map(|record| {
            (
                record["event"]["session_id"].as_str().unwrap().to_string(),
                record["event"]["type"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The sentinel in every field of a Herdr TUI agent that holds content,
/// around the few fields the adapter reads.
fn full_agent(terminal: &str, pane: &str, status: &str, cwd: &Path) -> Value {
    json!({
        "terminal_id": terminal,
        "name": mark("name"),
        "agent": mark("agent"),
        "title": mark("title"),
        "terminal_title": mark("terminal_title"),
        "terminal_title_stripped": mark("terminal_title_stripped"),
        "display_agent": mark("display_agent"),
        "agent_status": status,
        "screen_detection_skipped": false,
        "state_labels": { mark("a state label key"): mark("a state label") },
        "tokens": { mark("a token name"): mark("a token") },
        "agent_session": {
            "source": mark("agent_session.source"),
            "agent": mark("agent_session.agent"),
            "kind": "path",
            "value": format!("/home/agent/.codex/sessions/{MARK}_rollout.jsonl"),
            "resume_argv": ["codex", "resume", mark("an agent_session field that does not exist yet")],
        },
        "workspace_id": mark("workspace_id"),
        "tab_id": mark("tab_id"),
        "pane_id": pane,
        "focused": false,
        "launch_pending": false,
        "interactive_ready": true,
        "state_change_seq": 12,
        "completion_seq": null,
        "cwd": cwd,
        "foreground_cwd": cwd,
        "revision": 7,
        "last_message": mark("an agent field that does not exist yet"),
    })
}

/// A whole `agent.list` answer around one agent, with the sentinel
/// everywhere Herdr can put content and in places it puts nothing yet.
fn full_agents(status: &str, cwd: &Path) -> Value {
    // An agent's own session id that is a sentence.
    let mut worded = full_agent("term_b2", "w1:p2", "working", cwd);
    worded["agent_session"]["kind"] = json!("id");
    worded["agent_session"]["value"] = json!(mark("agent_session.value"));
    json!({
        "id": "n1",
        "note": mark("a field of the answer that does not exist yet"),
        "result": {
            "type": "agent_list",
            "agents": [
                full_agent("term_a1", "w1:p1", status, cwd),
                worded,
                // Content where a short word belongs, and where an id does.
                full_agent("term_c3", "w1:p3", &mark("agent_status"), cwd),
                full_agent(&mark("terminal_id"), "w1:p4", "working", cwd),
                full_agent("term_e5", &mark("pane_id"), "working", cwd),
            ],
            "summary": mark("a result field that does not exist yet"),
        },
    })
}

#[test]
fn nothing_the_herdr_tui_says_beside_the_state_leaves_the_box() {
    let (rig, fake) = rig_with("herdr-tui");
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let cwd = rig
        .session("fix-login")
        .join("src")
        .join(format!("{MARK}_subfolder"));
    std::fs::create_dir_all(&cwd).unwrap();
    let say = |status: &str| {
        let answer = full_agents(status, &cwd).to_string();
        fake.say(Answer::Line(answer.clone()));
        answer
    };

    let fixture = say("working");
    gate::assert_fixture_would_catch_a_leak(
        &fixture,
        90,
        &[
            "name",
            "agent",
            "title",
            "terminal_title",
            "terminal_title_stripped",
            "display_agent",
            "state_labels",
            "tokens",
            "agent_session",
            "source",
            "value",
            "workspace_id",
            "tab_id",
            "pane_id",
            "terminal_id",
            "agent_status",
            "cwd",
            "foreground_cwd",
        ],
    );
    assert_eq!(look(&rig, 0).condition, Condition::Ok);
    say("blocked");
    look(&rig, 1);
    say("done");
    look(&rig, 2);
    let mut long = full_agents("working", &cwd);
    long["padding"] = json!(MARK.repeat(5 * 1024 * 1024 / MARK.len()));
    fake.say(Answer::Line(long.to_string()));
    assert_eq!(look(&rig, 3).condition, Condition::BadReply);
    fake.agents(json!([]));
    look(&rig, 4);
    let memory = look(&rig, 5);
    assert!(memory.sessions.is_empty());

    // A hook whose process claims to be in a pane named with content.
    hook(
        &rig,
        "Stop",
        HOOKED,
        &cwd,
        &[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", &mark("HERDR_PANE_ID")),
            ("HERDR_SOCKET_PATH", fake.path.to_str().unwrap()),
        ],
    );
    assert!(!rig.home.state_dir().join("claims").exists());

    let mut from_driver = spooled_from(&rig, "herdr");
    from_driver.sort();
    let mut expected = events(
        "herdr:term_a1",
        &[
            "session_start",
            "user_prompt_submit",
            "input_prompt",
            "stop",
            "session_end",
        ],
    );
    for other in ["herdr:term_b2", "herdr:term_e5"] {
        expected.extend(events(
            other,
            &["session_start", "user_prompt_submit", "session_end"],
        ));
    }
    expected.sort();
    assert_eq!(from_driver, expected);
    for record in rig.spooled() {
        for key in record["event"].as_object().unwrap().keys() {
            assert!(gate::EVENT_FIELDS.contains(&key.as_str()), "{key}");
        }
    }

    gate::tick(&rig);
    assert!(!mock.to(contract::EVENTS).is_empty());
    gate::assert_clean(&rig, &mock);
    // Observe only: the one read call, each time, and not a byte more.
    assert_eq!(fake.received(), vec![AGENT_LIST.to_string(); 6]);
}
