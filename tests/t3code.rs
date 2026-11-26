//! T3 Code: what the adapter does, and its part of the privacy gate.
//!
//! The stand-in below answers as a T3 Code server does on the two routes the
//! adapter asks, built from T3 Code's own contracts (pingdotgg/t3code,
//! packages/contracts/src/environmentHttp.ts and orchestrationV2.ts). Its
//! shell answer, `fixtures/t3code/shell-sentinel.json`, is a whole
//! `OrchestrationV2ShellHttpSnapshot` with the sentinel in every field that
//! carries content, in fields where a short word belongs, in fields that do
//! not exist yet and in path components.
//!
//! If the gate tests here fail, content can leave the box. Do not release.

mod common;

use common::{
    contract, gate, git, run, stderr, Mock, Reply, Request, Rig, CLIENT_A_CONTAINER_UNIT,
};
use gate::MARK;
use nucleate::driver::{self, Condition, Memory, LOOK_LIMIT};
use nucleate::envelope::rfc3339;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SHELL: &str = include_str!("fixtures/t3code/shell-sentinel.json");
const ENVIRONMENT: &str = include_str!("fixtures/t3code/environment.json");

/// The two requests the adapter may make, and the only two.
const DESCRIPTOR_PATH: &str = "/.well-known/t3/environment";
const SHELL_PATH: &str = "/api/orchestration/shell?omitIdleSubagents=1&omitOlderSettled=1";
/// What `t3 auth session issue --token-only` would have printed.
const T3_TOKEN: &str = "t3s_8Qm2vX4LrN7cK1pZ_read_only";
const ENVIRONMENT_ID: &str = "env_5b1f0c7e-2d9a";

// ---- a stand-in T3 Code server ----------------------------------------------

/// What the stand-in answers with next. A test changes it between looks.
struct Stand {
    descriptor: (u16, String),
    shell: (u16, String),
    delay: Duration,
}

/// A T3 Code server on a local port: the descriptor route, open to anyone,
/// and the shell route, which answers 401 to a request without the bearer
/// token it issued (`EnvironmentAuthInvalidError`). Every request it receives
/// is recorded, whatever it asks for.
struct T3 {
    mock: Mock,
    stand: Arc<Mutex<Stand>>,
}

impl T3 {
    fn start(shell: &Value) -> T3 {
        let stand = Arc::new(Mutex::new(Stand {
            descriptor: (200, ENVIRONMENT.to_string()),
            shell: (200, shell.to_string()),
            delay: Duration::ZERO,
        }));
        let answers = stand.clone();
        let mock = Mock::start(move |request: &Request| {
            let stand = answers.lock().unwrap();
            let reply = |(status, body): &(u16, String)| Reply {
                status: *status,
                body: body.clone(),
                content_type: "application/json",
                headers: Vec::new(),
                delay: stand.delay,
            };
            let route = request.path.split('?').next().unwrap_or_default();
            match route {
                DESCRIPTOR_PATH => reply(&stand.descriptor),
                "/api/orchestration/shell" => {
                    let bearer = format!("Bearer {T3_TOKEN}");
                    if request.header("authorization") != Some(bearer.as_str()) {
                        return Reply::json(401, json!({ "_tag": "EnvironmentAuthInvalidError" }));
                    }
                    reply(&stand.shell)
                }
                _ => Reply::json(404, json!({ "_tag": "EnvironmentResourceNotFoundError" })),
            }
        });
        T3 { mock, stand }
    }

    fn url(&self) -> &str {
        &self.mock.url
    }

    fn answer_shell(&self, status: u16, body: impl ToString) {
        self.stand.lock().unwrap().shell = (status, body.to_string());
    }

    fn answer_descriptor(&self, status: u16, body: impl ToString) {
        self.stand.lock().unwrap().descriptor = (status, body.to_string());
    }

    fn hang_for(&self, delay: Duration) {
        self.stand.lock().unwrap().delay = delay;
    }

    /// Every request received so far, as (method, path with query).
    fn calls(&self) -> Vec<(String, String)> {
        self.mock
            .requests()
            .into_iter()
            .map(|request| (request.method, request.path))
            .collect()
    }
}

/// Observe only: everything the stand-in was ever sent is one of the two
/// reads, with nothing in its body.
fn only_the_two_reads(t3: &T3) {
    let shells = [
        "/api/orchestration/shell",
        "/api/orchestration/shell?omitIdleSubagents=1",
        SHELL_PATH,
    ];
    let requests = t3.mock.requests();
    assert!(!requests.is_empty());
    for request in requests {
        assert_eq!(request.method, "GET");
        assert!(
            request.path == DESCRIPTOR_PATH || shells.contains(&request.path.as_str()),
            "the server was asked for {}",
            request.path
        );
        assert!(request.body.is_empty(), "a request carried a body");
    }
}

fn get(path: &str) -> (String, String) {
    ("GET".to_string(), path.to_string())
}

// ---- the fixture ------------------------------------------------------------

/// The shell fixture, with the two folders and the one time a test owns
/// written into it.
fn shell(worktree: &Path, checkout: &Path) -> Value {
    // T3 Code writes the time a limit resets to the millisecond. Two hours
    // from now, on a whole second, so that it reads back as it was written.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let resets = rfc3339(UNIX_EPOCH + Duration::from_secs(now.as_secs() + 2 * 3600));
    let text = SHELL
        .replace("__WORKTREE__", &worktree.display().to_string())
        .replace("__CHECKOUT__", &checkout.display().to_string())
        .replace("__RESET_AT__", &resets);
    serde_json::from_str(&text).unwrap()
}

/// Change one thread of a shell answer, as the server would have.
fn change(shell: &mut Value, id: &str, with: Value) {
    let thread = shell["threads"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|thread| thread["id"] == id)
        .unwrap_or_else(|| panic!("{id} is not in the fixture"));
    for (key, value) in with.as_object().unwrap() {
        thread[key] = value.clone();
    }
}

fn remove(shell: &mut Value, id: &str) {
    shell["threads"]
        .as_array_mut()
        .unwrap()
        .retain(|thread| thread["id"] != id);
}

/// One more thread, a copy of one in the fixture under another id.
fn add(shell: &mut Value, like: &str, id: &str, with: Value) {
    let mut thread = shell["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["id"] == like)
        .unwrap()
        .clone();
    thread["id"] = json!(id);
    thread["lineage"]["rootThreadId"] = json!(id);
    shell["threads"].as_array_mut().unwrap().push(thread);
    change(shell, id, with);
}

const RUNNING: &str =
    r#"{ "status": "running", "activityRunStatus": "running", "activeRunId": "run-2" }"#;
const COMPLETED: &str =
    r#"{ "status": "completed", "activityRunStatus": null, "activeRunId": null }"#;

fn words(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

// ---- a box with a T3 Code environment ---------------------------------------

/// A git checkout that is no nucleus session, in a folder whose path holds
/// the sentinel: where a T3 Code project with no worktree of its own lives.
fn checkout(rig: &Rig) -> PathBuf {
    let dir = rig
        .home
        .dir
        .join(format!("{MARK}_projects"))
        .join("billing");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(
        &dir,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/billing.git",
        ],
    );
    dir
}

fn token_file(rig: &Rig) -> PathBuf {
    rig.home.dir.join(".config/nucleated/t3code.token")
}

fn write_token(rig: &Rig, token: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let file = token_file(rig);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Add a `t3` environment, beside `client-a`, whose driver is the T3 Code
/// server the `where_it_is` lines name, and issue it its token.
fn configure(rig: &Rig, where_it_is: &str) {
    write_token(rig, T3_TOKEN, 0o600);
    rig.configure(&format!(
        "[[environment]]\nname = \"t3\"\nroot = \"{}\"\ndriver = \"t3code\"\ncheck = \"true\"\n\
         driver_token_file = \"{}\"\n{where_it_is}\n",
        rig.root.display(),
        token_file(rig).display(),
    ));
}

/// A box whose `t3` environment is the stand-in at `url`.
fn rig_at(rig: Rig, url: &str) -> Rig {
    configure(&rig, &format!("driver_url = \"{url}\""));
    rig
}

/// A box, a nucleus session on it, a plain checkout, and a T3 Code server
/// whose threads work in them.
fn boxed(rig: Rig) -> (Rig, T3, Value) {
    let worktree = rig.session("fix-login");
    let fixture = shell(&worktree, &checkout(&rig));
    let t3 = T3::start(&fixture);
    let rig = rig_at(rig, t3.url());
    (rig, t3, fixture)
}

fn look_at(rig: &Rig, now: SystemTime) -> Memory {
    common::look(rig, "t3", now)
}

fn look(rig: &Rig) -> Memory {
    look_at(rig, SystemTime::now())
}

/// What the spool holds from its `from`th record on, as each session's event
/// types in the order they were written.
fn told(rig: &Rig, from: usize) -> BTreeMap<String, Vec<String>> {
    let mut by_session: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in rig.spooled().iter().skip(from) {
        assert_eq!(record["event"]["env_id"], "t3");
        by_session
            .entry(record["event"]["session_id"].as_str().unwrap().to_string())
            .or_default()
            .push(record["event"]["type"].as_str().unwrap().to_string());
    }
    by_session
}

/// `(session, "its event types in order")`, as [`told`] returns them.
fn expect(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<String>> {
    pairs
        .iter()
        .map(|(session, kinds)| {
            (
                session.to_string(),
                kinds.split(' ').map(str::to_string).collect(),
            )
        })
        .collect()
}

fn spooled_for(rig: &Rig, session: &str, kind: &str) -> Value {
    rig.spooled()
        .into_iter()
        .find(|record| record["event"]["session_id"] == session && record["event"]["type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} for {session}"))
}

// ---- what the adapter does --------------------------------------------------

#[test]
fn with_no_t3_code_server_there_the_environment_is_alive_and_nothing_else_is_said() {
    let rig = Rig::new();
    configure(
        &rig,
        &format!(
            "driver_home = \"{}\"",
            rig.home.dir.join("t3-home").display()
        ),
    );
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);

    let memory = look(&rig);
    assert_eq!(memory.condition, Condition::Absent);
    assert!(memory.sessions.is_empty());
    assert!(rig.spooled().is_empty());

    gate::tick(&rig);
    let beat = gate::said(&mock);
    let t3 = &beat["environments"][1];
    assert_eq!(t3["env_id"], "t3");
    assert_eq!(t3["driver"], "t3code");
    // Whether the server is alive is its supervisor's to say, not the adapter's.
    assert_eq!(t3["server_alive"], true);
    assert!(t3.get("driver_ref").is_none());
    assert!(mock.to(contract::EVENTS).is_empty());
    gate::assert_clean(&rig, &mock);
}

#[test]
fn each_thread_is_a_session_and_each_change_of_state_the_event_a_hook_would_have_sent() {
    let (rig, t3, mut fixture) = boxed(Rig::new());
    // Looks a few seconds apart, the last of them now.
    let start = SystemTime::now() - Duration::from_secs(600);
    let at = |seconds: u64| start + Duration::from_secs(seconds);

    // The first look: what is idle is history, and everything else is told.
    let memory = look_at(&rig, at(0));
    assert_eq!(memory.condition, Condition::Ok);
    assert_eq!(memory.link_ref.as_deref(), Some(ENVIRONMENT_ID));
    assert_eq!(
        told(&rig, 0),
        expect(&[
            ("thread-working", "session_start user_prompt_submit"),
            ("thread-permission", "session_start permission_prompt"),
            ("thread-input", "session_start input_prompt"),
            ("thread-elicitation", "session_start elicitation_prompt"),
            ("thread-failed", "session_start stop_failure"),
            ("thread-checkout", "session_start user_prompt_submit"),
            ("thread-nowhere", "session_start user_prompt_submit"),
            // Its own run is over, and a subagent below it waits on the owner.
            ("thread-parent", "session_start permission_prompt"),
            ("thread-odd-class", "session_start stop_failure"),
            ("thread-odd-reset", "session_start stop_failure"),
            ("thread-odd-project", "session_start user_prompt_submit"),
        ])
    );
    // Remembered without a word: idle threads. Not sessions at all: the
    // settled and the deleted, subagents and side chats, a thread whose id
    // is a sentence. Listed with no opinion: a thread whose state is not one
    // of T3 Code's words.
    for quiet in ["thread-idle", "thread-odd-settled"] {
        assert!(memory.sessions.contains_key(quiet), "{quiet}");
    }
    for unknown in [
        "thread-settled",
        "thread-deleted",
        "thread:subagent:9f2c",
        "thread-side-chat",
        "thread-odd-status",
        "thread-odd-activity",
        "thread-odd-kind",
        "thread-odd-lineage",
        "thread-odd-source",
        "thread-odd-shapes",
        "thread-odd-request",
    ] {
        assert!(!memory.sessions.contains_key(unknown), "{unknown}");
    }
    assert_eq!(memory.sessions.len(), 13);

    // Where a thread works says which repo and branch, and no more.
    let working = spooled_for(&rig, "thread-working", "user_prompt_submit");
    assert_eq!(working["event"]["repos"], json!(["web"]));
    assert_eq!(working["event"]["branch"], "workspace/fix-login");
    assert_eq!(working["workspace"], "fix-login");
    assert!(working["event"].get("bridge_session_id").is_none());
    let checkout = spooled_for(&rig, "thread-checkout", "user_prompt_submit");
    assert_eq!(checkout["event"]["repos"], json!(["acme/billing"]));
    assert_eq!(checkout["event"]["branch"], "main");
    let nowhere = spooled_for(&rig, "thread-nowhere", "user_prompt_submit");
    assert!(nowhere["event"].get("repos").is_none());
    assert!(nowhere["event"].get("branch").is_none());

    // A failure carries T3 Code's class for it, and a usage limit its reset.
    let failed = spooled_for(&rig, "thread-failed", "stop_failure");
    assert_eq!(failed["event"]["error_kind"], "usage_limit");
    assert_eq!(
        failed["event"]["limit_resets_at"],
        fixture["threads"][5]["usageLimitResetAt"]
    );
    let odd_class = spooled_for(&rig, "thread-odd-class", "stop_failure");
    assert!(odd_class["event"].get("error_kind").is_none());
    assert!(odd_class["event"].get("limit_resets_at").is_none());
    let odd_reset = spooled_for(&rig, "thread-odd-reset", "stop_failure");
    assert_eq!(odd_reset["event"]["error_kind"], "usage_limit");
    assert!(odd_reset["event"].get("limit_resets_at").is_none());

    // The second look: every thread has moved on.
    let seen = rig.spooled().len();
    change(&mut fixture, "thread-working", words(COMPLETED));
    change(
        &mut fixture,
        "thread-permission",
        json!({ "pendingRuntimeRequest": null, "activityRunStatus": "running" }),
    );
    change(
        &mut fixture,
        "thread-input",
        json!({ "deletedAt": "2026-11-20T10:05:00.000Z" }),
    );
    change(&mut fixture, "thread-idle", words(RUNNING));
    change(&mut fixture, "thread-failed", words(RUNNING));
    change(
        &mut fixture,
        "thread:subagent:9f2c",
        json!({ "pendingRuntimeRequest": null }),
    );
    change(&mut fixture, "thread-checkout", json!({ "itemCount": 13 }));
    remove(&mut fixture, "thread-elicitation");
    add(&mut fixture, "thread-idle", "thread-late", words(COMPLETED));
    t3.answer_shell(200, &fixture);
    look_at(&rig, at(20));
    assert_eq!(
        told(&rig, seen),
        expect(&[
            ("thread-working", "stop"),
            // The owner answered, and the agent is working again.
            ("thread-permission", "activity"),
            ("thread-input", "session_end"),
            ("thread-idle", "user_prompt_submit"),
            ("thread-failed", "user_prompt_submit"),
            ("thread-parent", "stop"),
            // A thread that turns up idle after the first look is new.
            ("thread-late", "session_start"),
        ])
    );

    // The third: a thread missing from two looks in a row is over.
    let seen = rig.spooled().len();
    let memory = look_at(&rig, at(40));
    assert_eq!(
        told(&rig, seen),
        expect(&[("thread-elicitation", "session_end")])
    );
    assert!(!memory.sessions.contains_key("thread-elicitation"));

    // Later: a thread still working whose item count has grown is heard
    // from once in a while, one that has not grown is not, and a deleted
    // thread's id seen live again is a new session.
    let seen = rig.spooled().len();
    change(&mut fixture, "thread-checkout", json!({ "itemCount": 40 }));
    let mut alive = words(RUNNING);
    alive["deletedAt"] = Value::Null;
    change(&mut fixture, "thread-input", alive);
    change(
        &mut fixture,
        "thread-input",
        json!({ "pendingRuntimeRequest": null }),
    );
    t3.answer_shell(200, &fixture);
    look_at(&rig, at(400));
    assert_eq!(
        told(&rig, seen),
        expect(&[
            ("thread-checkout", "activity"),
            ("thread-input", "session_start user_prompt_submit"),
        ])
    );

    // Every kind of event a hook reports has now come from the driver.
    let mut kinds: Vec<String> = told(&rig, 0).into_values().flatten().collect();
    kinds.sort();
    kinds.dedup();
    assert_eq!(
        kinds,
        [
            "activity",
            "elicitation_prompt",
            "input_prompt",
            "permission_prompt",
            "session_end",
            "session_start",
            "stop",
            "stop_failure",
            "user_prompt_submit"
        ]
    );
}

#[test]
fn the_server_is_sent_two_reads_and_nothing_else() {
    let (rig, t3, _) = boxed(Rig::new());
    for _ in 0..3 {
        assert_eq!(look(&rig).condition, Condition::Ok);
    }
    // The descriptor once, the shell at every look, and no third thing.
    assert_eq!(
        t3.calls(),
        vec![
            get(DESCRIPTOR_PATH),
            get(SHELL_PATH),
            get(SHELL_PATH),
            get(SHELL_PATH)
        ]
    );
    only_the_two_reads(&t3);
    let requests = t3.mock.requests();
    // The token goes to the route that needs it and to no other.
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(requests[0].header("x-t3-orchestration-protocol"), None);
    for asked in &requests[1..] {
        assert_eq!(
            asked.header("authorization"),
            Some(format!("Bearer {T3_TOKEN}").as_str())
        );
        assert_eq!(asked.header("x-t3-orchestration-protocol"), Some("2"));
    }
}

#[test]
fn a_server_that_does_not_advertise_the_smaller_shell_is_asked_for_the_whole_one() {
    let (rig, t3, _) = boxed(Rig::new());
    let mut plain: Value = serde_json::from_str(ENVIRONMENT).unwrap();
    plain["capabilities"] = json!({ "shellOlderSettled": true });
    t3.answer_descriptor(200, &plain);
    assert_eq!(look(&rig).condition, Condition::Ok);
    assert_eq!(
        t3.calls(),
        vec![get(DESCRIPTOR_PATH), get("/api/orchestration/shell")]
    );

    let (rig, t3, _) = boxed(Rig::new());
    plain["capabilities"] = json!({ "shellThreadFamilies": true });
    t3.answer_descriptor(200, &plain);
    assert_eq!(look(&rig).condition, Condition::Ok);
    assert_eq!(
        t3.calls(),
        vec![
            get(DESCRIPTOR_PATH),
            get("/api/orchestration/shell?omitIdleSubagents=1")
        ]
    );
}

#[test]
fn the_running_server_is_found_from_its_own_runtime_file_on_this_machine_only() {
    let rig = Rig::new();
    let fixture = shell(&rig.session("fix-login"), &checkout(&rig));
    let t3 = T3::start(&fixture);
    let home = rig.home.dir.join("t3-home");
    let runtime = home.join("userdata/server-runtime.json");
    std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    configure(&rig, &format!("driver_home = \"{}\"", home.display()));
    let written = |version: u64, origin: &str| {
        json!({
            "version": version, "pid": 4242, "port": 3773, "origin": origin,
            "startedAt": "2026-11-20T09:00:00.000Z", "serviceManaged": true,
            "ownerId": gate::mark("ownerId"), "serverVersion": gate::mark("serverVersion"),
        })
        .to_string()
    };

    std::fs::write(&runtime, written(1, t3.url())).unwrap();
    let memory = look(&rig);
    assert_eq!(memory.condition, Condition::Ok);
    assert!(memory.sessions.contains_key("thread-working"));
    let asked = t3.calls().len();

    // A server listening on the network names another address. It is not
    // asked there: the token and the answer stay on this machine.
    for elsewhere in [
        "http://192.168.1.20:3773",
        "https://t3.example.test",
        "http://127.0.0.1.example.test:3773",
        "http://127.0.0.1:3773/some/path",
    ] {
        std::fs::write(&runtime, written(1, elsewhere)).unwrap();
        assert_eq!(look(&rig).condition, Condition::Unsupported, "{elsewhere}");
    }
    // A runtime file of a version this adapter does not know is not read on.
    std::fs::write(&runtime, written(2, t3.url())).unwrap();
    assert_eq!(look(&rig).condition, Condition::Unsupported);
    std::fs::write(&runtime, "not json").unwrap();
    assert_eq!(look(&rig).condition, Condition::BadReply);
    std::fs::remove_file(&runtime).unwrap();
    let memory = look(&rig);
    assert_eq!(memory.condition, Condition::Absent);
    assert_eq!(t3.calls().len(), asked);
    // Nothing that went wrong ended anything.
    assert!(memory.sessions.contains_key("thread-working"));
    assert!(gate::leaks_on_disk(&rig.home.state_dir()).is_empty());
}

#[test]
fn a_config_that_names_a_server_on_another_machine_is_refused() {
    let rig = Rig::new();
    configure(&rig, "driver_url = \"http://t3.example.test:3773\"");
    let error = nucleate::settings::load_from(&rig.home.config_file()).unwrap_err();
    assert!(format!("{error:#}").contains("driver_url"), "{error:#}");
}

#[test]
fn an_absent_or_unhappy_server_changes_a_word_and_nothing_else() {
    let (rig, t3, fixture) = boxed(Rig::new());
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let good = look(&rig);
    assert_eq!(good.condition, Condition::Ok);
    let spooled = rig.spooled().len();
    assert!(spooled > 0);

    // After each: the word, no event, and every session as it was.
    let unhappy = |word: Condition, why: &str| {
        let memory = look(&rig);
        assert_eq!(memory.condition, word, "{why}");
        assert_eq!(memory.sessions, good.sessions, "{why}");
        assert_eq!(memory.link_ref, good.link_ref, "{why}");
        assert_eq!(rig.spooled().len(), spooled, "{why}");
    };
    let recovers = |why: &str| {
        t3.answer_descriptor(200, ENVIRONMENT);
        t3.answer_shell(200, &fixture);
        let memory = look(&rig);
        assert_eq!(memory.condition, Condition::Ok, "{why}");
        assert_eq!(memory.sessions, good.sessions, "{why}");
        assert_eq!(rig.spooled().len(), spooled, "{why}");
    };

    t3.answer_shell(401, json!({ "_tag": "EnvironmentAuthInvalidError" }));
    unhappy(Condition::Rejected, "the server no longer knows the token");
    recovers("401");
    t3.answer_shell(403, json!({ "_tag": "EnvironmentScopeRequiredError" }));
    unhappy(Condition::Rejected, "the token lacks the read scope");
    recovers("403");

    for (status, body) in [
        (
            500,
            json!({ "_tag": "EnvironmentInternalError" }).to_string(),
        ),
        (
            400,
            json!({ "_tag": "EnvironmentRequestInvalidError" }).to_string(),
        ),
        (302, String::new()),
        (200, "<html>not the shell</html>".to_string()),
        (200, "[]".to_string()),
        (200, json!({ "resumable": true }).to_string()),
        (200, json!({ "threads": gate::mark("threads") }).to_string()),
        (200, fixture.to_string()[..4000].to_string()),
    ] {
        t3.answer_shell(status, &body);
        unhappy(Condition::BadReply, &format!("{status} {body:.40}"));
        recovers(&body);
    }

    // A list with something in it that is no thread is not taken as whole:
    // nothing is ended for being missing from it.
    let mut partial = fixture.clone();
    remove(&mut partial, "thread-working");
    partial["threads"]
        .as_array_mut()
        .unwrap()
        .push(json!(gate::mark("a row that is a sentence")));
    t3.answer_shell(200, &partial);
    for _ in 0..3 {
        let memory = look(&rig);
        assert_eq!(memory.condition, Condition::Ok);
        assert!(memory.sessions.contains_key("thread-working"));
    }
    assert_eq!(rig.spooled().len(), spooled);
    recovers("a partial list");

    // A newer protocol: the descriptor says so, and the shell is not asked.
    let mut newer: Value = serde_json::from_str(ENVIRONMENT).unwrap();
    newer["orchestrationProtocolVersion"] = json!(3);
    t3.answer_descriptor(200, &newer);
    // The bump is found when the route first refuses the old header.
    t3.answer_shell(400, json!({ "_tag": "EnvironmentRequestInvalidError" }));
    unhappy(Condition::BadReply, "the route refuses protocol 2");
    let asked = t3.calls().len();
    unhappy(Condition::Unsupported, "the descriptor says protocol 3");
    assert_eq!(t3.calls()[asked..], [get(DESCRIPTOR_PATH)]);
    newer
        .as_object_mut()
        .unwrap()
        .remove("orchestrationProtocolVersion");
    t3.answer_descriptor(200, &newer);
    unhappy(Condition::Unsupported, "an older server names no protocol");
    t3.answer_descriptor(200, "not a descriptor");
    unhappy(Condition::BadReply, "the descriptor is not one");
    t3.answer_descriptor(503, "");
    unhappy(Condition::BadReply, "the descriptor route is down");
    recovers("a newer protocol");

    // No token, or one that anyone on the box could read: nothing is asked.
    let asked = t3.calls().len();
    std::fs::remove_file(token_file(&rig)).unwrap();
    unhappy(Condition::NoToken, "no token file");
    write_token(&rig, "", 0o600);
    unhappy(Condition::NoToken, "an empty token file");
    write_token(&rig, &gate::mark("a token file holding a sentence"), 0o600);
    unhappy(Condition::NoToken, "a token file that holds no token");
    write_token(&rig, T3_TOKEN, 0o644);
    unhappy(Condition::NoToken, "a token file others can read");
    assert_eq!(t3.calls().len(), asked);
    write_token(&rig, "t3s_a_token_the_server_never_issued", 0o600);
    unhappy(Condition::Rejected, "a token the server does not know");
    write_token(&rig, T3_TOKEN, 0o600);
    recovers("the token");

    // A server that does not answer costs the look, inside its limit.
    t3.hang_for(Duration::from_secs(6));
    let asked = Instant::now();
    unhappy(Condition::Timeout, "the server hangs");
    assert!(asked.elapsed() < LOOK_LIMIT);
    t3.hang_for(Duration::ZERO);

    // The server stops.
    drop(t3);
    unhappy(Condition::Absent, "nothing is listening");
    unhappy(Condition::Absent, "nothing is listening, again");

    // Through all of it the environment's server is as alive as its
    // supervisor says, and the sessions are as they were.
    gate::tick(&rig);
    let beat = gate::said(&mock);
    assert_eq!(beat["environments"][1]["env_id"], "t3");
    assert_eq!(beat["environments"][1]["server_alive"], true);
    assert_eq!(beat["environments"][1]["driver_ref"], ENVIRONMENT_ID);
    let shown = gate::status_shown(&rig);
    assert_eq!(shown["environments"][1]["driver"]["condition"], "absent");
    gate::assert_clean(&rig, &mock);
}

#[test]
fn status_shows_the_driver_its_last_look_and_the_link_that_opens_a_thread() {
    let (rig, _t3, _) = boxed(Rig::new());
    look(&rig);
    let shown = gate::status_shown(&rig);
    let t3 = &shown["environments"][1]["driver"];
    assert_eq!(t3["name"], "t3code");
    assert_eq!(t3["condition"], "ok");
    assert_eq!(t3["link_ref"], ENVIRONMENT_ID);
    assert_eq!(
        t3["link_template"],
        "https://app.t3.codes/threads/{driver_ref}/{session_id}"
    );
    assert_eq!(shown["environments"][0]["driver"], Value::Null);
}

#[test]
fn an_agent_whose_own_hook_reports_it_is_not_reported_again_as_its_thread() {
    let (rig, t3, mut fixture) = boxed(Rig::new());
    // T3 Code runs in `client-a`'s container, beside the agent's login.
    write_token(&rig, T3_TOKEN, 0o600);
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"t3\"\nroot = \"{}\"\ndriver = \"t3code\"\ncheck = \"true\"\n\
         {CLIENT_A_CONTAINER_UNIT}driver_token_file = \"{}\"\ndriver_url = \"{}\"\n",
        rig.root.display(),
        token_file(&rig).display(),
        t3.url(),
    ));
    let worktree = rig.root.join(".worktrees/fix-login/web");
    // T3 Code launches an agent with its thread's id in the environment,
    // and its hook writes the container's spool, which the daemon takes in.
    let hook = |thread: &str, session: &str, event: &str| {
        let out = run(
            rig.in_container(
                rig.nucleated()
                    .args(["hook", event, "--env", "client-a"])
                    .env("T3CODE_THREAD_ID", thread),
            ),
            &json!({ "session_id": session, "cwd": worktree, "hook_event_name": event })
                .to_string(),
        );
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(rig.spooled_in_container().len(), 1);
        gate::tick(&rig);
        assert!(rig.spooled_in_container().is_empty());
    };

    // The driver saw the thread first.
    let memory = look(&rig);
    assert!(memory.sessions.contains_key("thread-working"));

    // Then the agent's own hook spoke up from inside it. The hook wins: the
    // driver's session for that thread ends, and is not started again.
    hook("thread-working", "claude-session-1", "SessionStart");
    let memory = look(&rig);
    assert!(!memory.sessions.contains_key("thread-working"));
    assert_eq!(told(&rig, 0), expect(&[("thread-working", "session_end")]));
    let after = rig.spooled().len();
    look(&rig);
    assert_eq!(rig.spooled().len(), after);

    // A thread whose agent's hook is heard before the driver lists it is
    // never the driver's to report.
    hook("thread-late", "claude-session-2", "SessionStart");
    add(&mut fixture, "thread-working", "thread-late", json!({}));
    t3.answer_shell(200, &fixture);
    let memory = look(&rig);
    assert!(!memory.sessions.contains_key("thread-late"));
    assert!(rig.spooled().is_empty());
    // Every other thread is still the driver's.
    assert!(memory.sessions.contains_key("thread-permission"));
}

#[test]
fn a_hook_heard_from_the_shared_spool_or_another_container_does_not_hide_its_thread() {
    let (rig, t3, _) = boxed(Rig::new());
    // T3 Code in a container of its own, apart from `client-a`'s.
    write_token(&rig, T3_TOKEN, 0o600);
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"t3\"\nroot = \"{}\"\ndriver = \"t3code\"\ncheck = \"true\"\n\
         unit = \"agent-env-t3.service\"\ndriver_token_file = \"{}\"\ndriver_url = \"{}\"\n",
        rig.root.display(),
        token_file(&rig).display(),
        t3.url(),
    ));
    let worktree = rig.root.join(".worktrees/fix-login/web");
    let payload = json!({ "session_id": "claude-session-1", "cwd": worktree }).to_string();
    // A hook in `client-a`'s container, and one writing the shared spool,
    // each saying it runs in a thread of T3 Code's.
    let inside = run(
        rig.in_container(
            rig.nucleated()
                .args(["hook", "SessionStart", "--env", "client-a"])
                .env("T3CODE_THREAD_ID", "thread-working"),
        ),
        &payload,
    );
    assert_eq!(inside.status.code(), Some(0));
    let shared = run(
        rig.nucleated()
            .args(["hook", "SessionStart", "--env", "client-a"])
            .env("T3CODE_THREAD_ID", "thread-permission"),
        &json!({ "session_id": "claude-session-2", "cwd": worktree }).to_string(),
    );
    assert_eq!(shared.status.code(), Some(0));
    gate::tick(&rig);
    // Both sessions are known; neither claim counted.
    let settings = rig.settings();
    let store = nucleate::sessions::Store::new(&settings.state_dir);
    assert!(store.load("client-a", "claude-session-1").is_some());
    assert!(store.load("client-a", "claude-session-2").is_some());
    assert!(!rig.home.state_dir().join("claims").exists());
    let memory = look(&rig);
    assert!(memory.sessions.contains_key("thread-working"));
    assert!(memory.sessions.contains_key("thread-permission"));
}

// ---- the privacy gate -------------------------------------------------------

/// Every field of T3 Code's answers that carries content, a name a person
/// chose, a path or an address. Each holds the sentinel in the fixture.
const CONTENT_FIELDS: [&str; 30] = [
    "title",
    "text",
    "lastError",
    "lastErrorCode",
    "objective",
    "lastCheck",
    "description",
    "command",
    "taskId",
    "tags",
    "label",
    "branch",
    "model",
    "linkedPullRequest",
    "pullRequests",
    "branchPullRequest",
    "issues",
    "displayKey",
    "titleRegeneration",
    "activeProviderThreadId",
    "archivedThreads",
    "workspaceRoot",
    "scripts",
    "repositoryIdentity",
    "remoteUrl",
    "rootPath",
    "faviconPath",
    "olderSettled",
    "projectIdentities",
    "transcript",
];

#[test]
fn the_fixtures_would_catch_a_leak() {
    gate::assert_fixture_would_catch_a_leak(SHELL, 700, &CONTENT_FIELDS);
    gate::assert_fixture_would_catch_a_leak(ENVIRONMENT, 4, &["label", "serverVersion"]);

    // The sentinel is where a careless parser would read it: a struct that
    // named `title`, or the latest message, would have held it for every
    // thread there is.
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Careless {
        threads: Vec<CarelessThread>,
        archived_threads: Vec<CarelessThread>,
        projects: Vec<CarelessThread>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CarelessThread {
        title: String,
        latest_visible_message: Option<CarelessMessage>,
    }
    #[derive(Deserialize)]
    struct CarelessMessage {
        text: String,
    }
    let careless: Careless = serde_json::from_str(SHELL).unwrap();
    assert!(careless.threads.len() >= 26);
    let rows = careless
        .threads
        .iter()
        .chain(&careless.archived_threads)
        .chain(&careless.projects);
    for row in rows {
        assert!(row.title.contains(MARK));
        if let Some(message) = &row.latest_visible_message {
            assert!(message.text.contains(MARK));
        }
    }

    // And where a short word belongs: the state fields the adapter does
    // read, holding a sentence, a list or an object.
    let shell: Value = serde_json::from_str(SHELL).unwrap();
    let threads = shell["threads"].as_array().unwrap();
    let marked = |field: &str| {
        threads
            .iter()
            .filter(|thread| thread[field].to_string().contains(MARK))
            .count()
    };
    for field in [
        "id",
        "projectId",
        "status",
        "activityRunStatus",
        "pendingRuntimeRequest",
        "lastErrorClass",
        "usageLimitResetAt",
        "lineage",
        "creationSource",
        "settledOverride",
        "itemCount",
        "pendingBackgroundTasks",
        "worktreePath",
    ] {
        assert!(marked(field) >= 1, "{field} never holds the sentinel");
    }
}

#[test]
fn nothing_t3_code_says_beside_the_state_leaves_the_box_or_stays_on_it() {
    // The nucleus, and so the worktree a thread works in, is below a folder
    // whose name is the sentinel too.
    let (rig, t3, mut fixture) = boxed(Rig::under(&format!("{MARK}_parent_folder")));
    let mock = gate::watch_ahead();
    rig.sign_in(&mock.url);
    let start = SystemTime::now() - Duration::from_secs(600);
    let at = |seconds: u64| start + Duration::from_secs(seconds);

    assert_eq!(look_at(&rig, at(0)).condition, Condition::Ok);
    // Every thread moves, so every kind of event is written.
    change(&mut fixture, "thread-working", words(COMPLETED));
    change(
        &mut fixture,
        "thread-permission",
        json!({ "pendingRuntimeRequest": null, "activityRunStatus": "running" }),
    );
    change(
        &mut fixture,
        "thread-input",
        json!({ "deletedAt": gate::mark("deletedAt") }),
    );
    change(&mut fixture, "thread-idle", words(RUNNING));
    remove(&mut fixture, "thread-elicitation");
    t3.answer_shell(200, &fixture);
    look_at(&rig, at(20));
    look_at(&rig, at(40));
    change(&mut fixture, "thread-checkout", json!({ "itemCount": 40 }));
    t3.answer_shell(200, &fixture);
    assert_eq!(look_at(&rig, at(400)).condition, Condition::Ok);

    let spooled = rig.spooled();
    let mut kinds: Vec<&str> = spooled
        .iter()
        .map(|record| record["event"]["type"].as_str().unwrap())
        .collect();
    kinds.sort();
    kinds.dedup();
    assert_eq!(kinds.len(), 9, "{kinds:?}");
    // What was written holds ids, fixed words, repo names and a branch.
    assert!(!serde_json::to_string(&spooled).unwrap().contains(MARK));

    // Unhappy answers leave nothing behind either.
    t3.answer_shell(
        500,
        json!({ "_tag": "EnvironmentInternalError", "message": gate::mark("message") }),
    );
    assert_eq!(look(&rig).condition, Condition::BadReply);
    t3.answer_shell(200, json!({ "threads": gate::mark("threads") }));
    assert_eq!(look(&rig).condition, Condition::BadReply);
    t3.answer_descriptor(
        200,
        json!({ "environmentId": gate::mark("environmentId"), "label": gate::mark("label") }),
    );
    assert_eq!(look(&rig).condition, Condition::Unsupported);
    // An environment id that is a sentence is no id: no link is offered.
    t3.answer_descriptor(
        200,
        json!({
            "environmentId": gate::mark("environmentId"), "label": gate::mark("label"),
            "orchestrationProtocolVersion": 2,
            "capabilities": gate::mark("capabilities"),
        }),
    );
    t3.answer_shell(200, &fixture);
    let memory = look(&rig);
    assert_eq!(memory.condition, Condition::Ok);
    assert_eq!(memory.link_ref, None);
    t3.answer_descriptor(200, ENVIRONMENT);
    t3.answer_shell(401, "");
    assert_eq!(look(&rig).condition, Condition::Rejected);
    t3.answer_shell(200, &fixture);
    assert_eq!(look(&rig).link_ref.as_deref(), Some(ENVIRONMENT_ID));

    // The daemon sends what the looks wrote.
    gate::tick(&rig);
    let sent: Vec<Value> = mock
        .to(contract::EVENTS)
        .iter()
        .flat_map(|request| request.json()["events"].as_array().unwrap().clone())
        .collect();
    assert!(!sent.is_empty());
    assert!(sent.iter().all(|event| event["env_id"] == "t3"));
    let beat = gate::said(&mock);
    let reported = &beat["environments"][1];
    assert_eq!(reported["driver"], "t3code");
    assert_eq!(reported["driver_ref"], ENVIRONMENT_ID);
    // A driver environment is not one login.
    assert!(reported.get("agent_version").is_none());
    assert!(reported.get("login_expires_at").is_none());

    // The gate: no content on the wire, under the state folder or in status.
    gate::assert_clean(&rig, &mock);
    // The owner's token for T3 Code went to T3 Code and nowhere else.
    assert!(!mock.everything().contains(T3_TOKEN));
    for (path, bytes) in common::files_under(&rig.home.state_dir()) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(T3_TOKEN),
            "the token is in {}",
            path.display()
        );
    }
    // What is remembered between looks is ids, fixed words and numbers.
    let remembered =
        std::fs::read_to_string(driver::memory_file(&rig.home.state_dir(), "t3")).unwrap();
    let remembered: Value = serde_json::from_str(&remembered).unwrap();
    assert_eq!(remembered["driver"], "t3code");
    assert_eq!(remembered["condition"], "ok");

    // Observe only: all of that took the two reads, and nothing else.
    only_the_two_reads(&t3);
}

#[test]
fn the_daemon_itself_asks_the_server_and_logs_only_a_fixed_word() {
    let (rig, t3, _) = boxed(Rig::under(&format!("{MARK}_parent_folder")));
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    // A thread waiting on its owner is sent as soon as the look finds it.
    let out = gate::daemon_until(&rig, None, "a waiting thread to reach Watch", &|| {
        mock.to(contract::EVENTS).iter().any(|request| {
            request.json()["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["session_id"] == "thread-permission")
        })
    });

    // What the look left for the next heartbeat to carry.
    let memory = driver::read_memory(&rig.home.state_dir(), "t3").unwrap();
    assert_eq!(memory.condition, Condition::Ok);
    assert_eq!(memory.link_ref.as_deref(), Some(ENVIRONMENT_ID));

    let logged = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    assert!(logged.contains("driver t3code for t3: ok"), "{logged}");
    assert!(!logged.contains(MARK), "content in the log:\n{logged}");
    assert!(!logged.contains(T3_TOKEN));
    gate::assert_clean(&rig, &mock);
    only_the_two_reads(&t3);
}
