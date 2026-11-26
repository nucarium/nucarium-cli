//! Each container environment's own spool folder, read by the daemon as
//! something a hostile container writes: regular files only, never through a
//! link, capped; a record only for the environment whose folder it is in; a
//! hook's claim on a driver's session counted only inside the driver's
//! container; and a session's driver reference put on its events by the
//! daemon alone.

mod common;

use common::{gate, Rig, CLIENT_A_CONTAINER_UNIT};
use nucleate::envelope::{unix_ms, Claim, Draft, Envelope, EventType};
use nucleate::sessions::Store;
use nucleate::spool::{Record, Spool, MAX_FILE};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Where a hook wrote its record.
enum Folder {
    /// `client-a`'s container's own.
    Container,
    /// The shared one, which host environments and containers started
    /// before the change write.
    Shared,
}

fn folder(rig: &Rig, folder: Folder) -> PathBuf {
    match folder {
        Folder::Container => rig.container_spool(),
        Folder::Shared => rig.spool_dir(),
    }
}

/// The claim a hook in a T3 Code thread makes on it.
fn claim() -> Claim {
    Claim {
        driver_env: "client-a-t3".into(),
        key: "thread_01HZX".into(),
        folders: vec!["0123456789abcdef".into()],
    }
}

/// Write one record to `dir`, `ms` milliseconds from now; returns its id.
fn write(dir: &Path, kind: EventType, ms: u64, draft: Draft) -> String {
    let at = SystemTime::now() + Duration::from_millis(ms);
    let record = Record {
        event: Envelope::new(kind, draft, at).unwrap(),
        at_ms: unix_ms(at),
        workspace: None,
        process: None,
    };
    Spool::new(dir).write(&record).unwrap();
    record.event.event_id
}

fn session(env_id: &str, session_id: &str) -> Draft {
    Draft {
        env_id: env_id.into(),
        session_id: session_id.into(),
        ..Draft::default()
    }
}

/// A box with `client-a` in its container, and a T3 Code environment,
/// `client-a-t3`, given `unit`.
fn box_with_t3(unit: &str) -> Rig {
    let rig = Rig::new();
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"client-a-t3\"\nroot = \"{root}\"\ndriver = \"t3code\"\n\
         check = \"true\"\n{unit}driver_home = \"{home}\"\ndriver_poll = \"off\"\n",
        root = rig.root.display(),
        home = rig.home.dir.join("no-t3-here").display(),
    ));
    rig
}

/// T3 Code in `client-a`'s container, as the recipe runs a driver.
fn box_with_t3_beside_the_login() -> Rig {
    box_with_t3(CLIENT_A_CONTAINER_UNIT)
}

/// What every batch sent carried, in order.
fn sent(mock: &common::Mock) -> Vec<Value> {
    assert_eq!(common::contract::violations(mock), Vec::<String>::new());
    mock.to(common::contract::EVENTS)
        .iter()
        .flat_map(|request| request.json()["events"].as_array().cloned().unwrap())
        .collect()
}

fn names(events: &[Value]) -> Vec<(String, String)> {
    events
        .iter()
        .map(|event| {
            (
                event["env_id"].as_str().unwrap().to_string(),
                event["session_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn claims(rig: &Rig) -> Vec<(PathBuf, Vec<u8>)> {
    common::files_under(&rig.home.state_dir().join("claims"))
}

// ---- the folders -------------------------------------------------------------

#[test]
fn each_environments_own_folder_is_read_beside_the_shared_one() {
    let rig = Rig::new();
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"client-b\"\nroot = \"{}\"\ncheck = \"true\"\n",
        rig.root.display()
    ));
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    write(
        &rig.container_spool(),
        EventType::PermissionPrompt,
        0,
        session("client-a", "in-the-container"),
    );
    // A host environment, and a container started before the change,
    // still write the shared folder.
    write(
        &rig.spool_dir(),
        EventType::PermissionPrompt,
        1,
        session("client-b", "on-the-host"),
    );
    write(
        &rig.spool_dir(),
        EventType::PermissionPrompt,
        2,
        session("client-a", "from-before-the-restart"),
    );
    // A folder named for no environment of this box is not read.
    let stranger = rig.home.state_dir().join("spools").join("client-z");
    write(
        &stranger,
        EventType::PermissionPrompt,
        3,
        session("client-a", "from-a-stranger"),
    );
    gate::tick(&rig);
    let mut told = names(&sent(&mock));
    told.sort();
    assert_eq!(
        told,
        [
            (
                "client-a".to_string(),
                "from-before-the-restart".to_string()
            ),
            ("client-a".to_string(), "in-the-container".to_string()),
            ("client-b".to_string(), "on-the-host".to_string()),
        ]
    );
    assert!(rig.spooled_in_container().is_empty());
    assert!(rig.spooled().is_empty());
    assert_eq!(entries(&stranger).len(), 1);
}

#[test]
fn a_record_in_an_environments_folder_for_another_environment_is_dropped() {
    let rig = Rig::new();
    rig.configure_in_container(&format!(
        "[[environment]]\nname = \"client-b\"\nroot = \"{}\"\ncheck = \"true\"\n",
        rig.root.display()
    ));
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    write(
        &rig.container_spool(),
        EventType::PermissionPrompt,
        0,
        session("client-b", "spoken-for-another"),
    );
    write(
        &rig.container_spool(),
        EventType::PermissionPrompt,
        1,
        session("client-a", "its-own"),
    );
    gate::tick(&rig);
    assert_eq!(
        names(&sent(&mock)),
        [("client-a".to_string(), "its-own".to_string())]
    );
    assert!(rig.spooled_in_container().is_empty());
    let store = Store::new(&rig.home.state_dir());
    assert!(store.load("client-b", "spoken-for-another").is_none());
    assert!(store.load("client-a", "its-own").is_some());
}

#[test]
fn a_name_planted_in_one_folder_never_hides_the_same_event_in_another() {
    let rig = Rig::new();
    rig.configure_in_container("");
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    // The real event, in `client-a`'s own folder.
    let id = write(
        &rig.container_spool(),
        EventType::PermissionPrompt,
        0,
        session("client-a", "its-own"),
    );
    // In the shared folder, which is read first and which a container
    // started before its own folder existed still writes: the same id under
    // the name a set-aside event has, and a folder under its very name.
    let shared = rig.spool_dir();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join(format!("{id}@x.json")), "planted").unwrap();
    std::fs::create_dir(shared.join(format!("{id}.json"))).unwrap();
    gate::tick(&rig);
    let told = sent(&mock);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["event_id"], id.as_str());
}

#[test]
fn links_pipes_and_files_too_big_to_be_records_are_not_read_and_hold_nothing_up() {
    let rig = Rig::new();
    rig.configure_in_container("");
    let mock = gate::watch();
    rig.sign_in(&mock.url);
    let spool = rig.container_spool();
    let good = write(
        &spool,
        EventType::PermissionPrompt,
        0,
        session("client-a", "its-own"),
    );

    // Each planted file has a name a record would have.
    let named = |n: u64| {
        let id = write(
            &rig.home.dir.join("scratch"),
            EventType::Stop,
            10 + n,
            session("client-a", "planted"),
        );
        (id.clone(), spool.join(format!("{id}.json")))
    };
    // A link to a good record somewhere else on the box.
    let (linked, link) = named(1);
    let target = rig.home.dir.join("scratch").join(format!("{linked}.json"));
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // A pipe no one will ever write to.
    let (_, pipe) = named(2);
    let made = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .unwrap();
    assert!(made.success());
    // A link to a device that never ends.
    let (_, endless) = named(3);
    std::os::unix::fs::symlink("/dev/zero", &endless).unwrap();
    // A good record padded past what any record can be.
    let (padded, big) = named(4);
    let text = std::fs::read_to_string(rig.home.dir.join("scratch").join(format!("{padded}.json")))
        .unwrap();
    std::fs::write(&big, format!("{text}{}", " ".repeat(MAX_FILE as usize + 1))).unwrap();
    // And where a refused event would be set aside, a link out of the folder.
    let elsewhere = rig.home.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, spool.join("rejected")).unwrap();
    // A file that is no record at all.
    let broken = write(&spool, EventType::Stop, 20, session("client-a", "broken"));
    std::fs::write(spool.join(format!("{broken}.json")), "{ not a record").unwrap();

    let started = Instant::now();
    gate::tick(&rig);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let told = sent(&mock);
    assert_eq!(told.len(), 1);
    assert_eq!(told[0]["event_id"], good.as_str());
    // What was planted is gone from the folder, and nothing it named was
    // touched: the record a link named is still there, and nothing went
    // through the link to the set-aside folder.
    assert_eq!(entries(&spool), ["rejected"]);
    assert!(target.is_file());
    assert!(entries(&elsewhere).is_empty());
    // The file that is no record is set aside where no container writes.
    assert_eq!(
        entries(&rig.home.state_dir().join("spools").join(".rejected")),
        [format!("{broken}@client-a.json")]
    );
    assert_eq!(gate::tick(&rig).state(true).events.rejected, 1);
}

// ---- claims ------------------------------------------------------------------

#[test]
fn a_claim_counts_only_from_the_folder_of_an_environment_in_the_drivers_container() {
    let claimed = |draft: Draft| Draft {
        claim: Some(claim()),
        ..draft
    };

    // Inside the driver's container: the claim counts, and only where the
    // daemon keeps its own state.
    let rig = box_with_t3_beside_the_login();
    write(
        &folder(&rig, Folder::Container),
        EventType::UserPromptSubmit,
        0,
        claimed(session("client-a", "s1")),
    );
    gate::tick(&rig);
    let held = claims(&rig);
    assert_eq!(held.len(), 1);
    assert!(held[0]
        .0
        .ends_with("claims/client-a-t3/thread_01HZX/s1.json"));
    assert_eq!(
        String::from_utf8(held[0].1.clone()).unwrap(),
        "client-a\n0123456789abcdef"
    );

    // From the shared folder: the event is kept, the claim is not.
    let rig = box_with_t3_beside_the_login();
    write(
        &folder(&rig, Folder::Shared),
        EventType::UserPromptSubmit,
        0,
        claimed(session("client-a", "s1")),
    );
    gate::tick(&rig);
    assert!(claims(&rig).is_empty());
    assert!(Store::new(&rig.home.state_dir())
        .load("client-a", "s1")
        .is_some());

    // From another container than the driver's.
    let rig = box_with_t3("unit = \"agent-env-client-a-t3.service\"\n");
    write(
        &folder(&rig, Folder::Container),
        EventType::UserPromptSubmit,
        0,
        claimed(session("client-a", "s1")),
    );
    gate::tick(&rig);
    assert!(claims(&rig).is_empty());
    assert!(Store::new(&rig.home.state_dir())
        .load("client-a", "s1")
        .is_some());

    // From environments on the host, which run in no container at all.
    let rig = Rig::new();
    rig.configure(&format!(
        "[[environment]]\nname = \"client-a-t3\"\nroot = \"{root}\"\ndriver = \"t3code\"\n\
         check = \"true\"\ndriver_home = \"{home}\"\ndriver_poll = \"off\"\n",
        root = rig.root.display(),
        home = rig.home.dir.join("no-t3-here").display(),
    ));
    write(
        &folder(&rig, Folder::Container),
        EventType::UserPromptSubmit,
        0,
        claimed(session("client-a", "s1")),
    );
    gate::tick(&rig);
    assert!(claims(&rig).is_empty());

    // Naming an environment that is no driver's, or none of this box's.
    for driver_env in ["client-a", "client-z"] {
        let rig = box_with_t3_beside_the_login();
        write(
            &folder(&rig, Folder::Container),
            EventType::UserPromptSubmit,
            0,
            Draft {
                claim: Some(Claim {
                    driver_env: driver_env.into(),
                    ..claim()
                }),
                ..session("client-a", "s1")
            },
        );
        gate::tick(&rig);
        assert!(claims(&rig).is_empty(), "{driver_env}");
    }
}

#[test]
fn a_hook_in_a_second_containers_t3_thread_claims_that_containers_t3_code() {
    // Two logins, each in its own container with T3 Code beside it, `client-a`'s
    // listed first.
    let rig = Rig::new();
    let root_b = rig.home.dir.join("client-b");
    std::fs::create_dir_all(&root_b).unwrap();
    let t3 = |name: &str, unit: &str| {
        format!(
            "[[environment]]\nname = \"{name}\"\nroot = \"{root}\"\ndriver = \"t3code\"\n\
             check = \"true\"\nunit = \"{unit}\"\ndriver_home = \"{home}\"\n\
             driver_poll = \"off\"\n",
            root = rig.root.display(),
            home = rig.home.dir.join("no-t3-here").display(),
        )
    };
    rig.configure_in_container(&format!(
        "{}{}[[environment]]\nname = \"client-b\"\nroot = \"{}\"\ncheck = \"true\"\n\
         unit = \"agent-env-client-b.service\"\n",
        t3("client-a-t3", "agent-env-client-a.service"),
        t3("client-b-t3", "agent-env-client-b.service"),
        root_b.display(),
    ));
    // Claude Code in one of `client-b`'s T3 threads fires its hook, inside
    // `client-b`'s container, with that container's spool.
    let spool_b = rig.home.state_dir().join("spools").join("client-b");
    let config = std::fs::read_to_string(rig.home.config_file()).unwrap();
    let inside_b = rig.home.config_file().with_file_name("in-client-b.toml");
    std::fs::write(
        &inside_b,
        format!("{config}\n[spool]\ndir = \"{}\"\n", spool_b.display()),
    )
    .unwrap();
    let out = common::run(
        rig.nucleated()
            .args(["hook", "UserPromptSubmit", "--env", "client-b"])
            .env("NUCLEATED_CONFIG", &inside_b)
            .env("T3CODE_THREAD_ID", "thread_01HZX"),
        &serde_json::json!({ "session_id": "s1", "cwd": root_b }).to_string(),
    );
    assert_eq!(out.status.code(), Some(0), "{}", common::stderr(&out));
    let hooked = entries(&spool_b);
    assert_eq!(hooked.len(), 1, "{hooked:?}");
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(spool_b.join(&hooked[0])).unwrap()).unwrap();
    assert_eq!(record["event"]["claim"]["driver_env"], "client-b-t3");

    // The daemon counts it, for `client-b`'s T3 Code.
    gate::tick(&rig);
    let held = claims(&rig);
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(held[0]
        .0
        .ends_with("claims/client-b-t3/thread_01HZX/s1.json"));
    let note = String::from_utf8(held[0].1.clone()).unwrap();
    assert_eq!(note.lines().next(), Some("client-b"), "{note}");
}

#[test]
fn a_counted_claim_puts_the_driver_reference_on_the_sessions_events_while_it_stands() {
    let rig = box_with_t3_beside_the_login();
    let mock = gate::watch_taking(&["driver_session"]);
    rig.sign_in(&mock.url);
    let spool = rig.container_spool();
    write(
        &spool,
        EventType::PermissionPrompt,
        0,
        Draft {
            claim: Some(claim()),
            ..session("client-a", "s1")
        },
    );
    gate::tick(&rig);
    // A later event of that session, whose hook said nothing of a driver,
    // carries the reference too; another session's does not.
    write(&spool, EventType::Stop, 1, session("client-a", "s1"));
    write(
        &spool,
        EventType::PermissionPrompt,
        2,
        session("client-a", "s2"),
    );
    gate::tick(&rig);
    // Once the claim no longer stands, the session's events carry none.
    std::fs::remove_dir_all(rig.home.state_dir().join("claims")).unwrap();
    write(
        &spool,
        EventType::PermissionPrompt,
        3,
        session("client-a", "s1"),
    );
    gate::tick(&rig);

    let told: Vec<(String, Option<String>, Option<String>)> = sent(&mock)
        .iter()
        .map(|event| {
            (
                format!("{} {}", event["session_id"], event["type"]).replace('"', ""),
                event["driver_env"].as_str().map(str::to_string),
                event["driver_session"].as_str().map(str::to_string),
            )
        })
        .collect();
    let with = |what: &str| {
        (
            what.to_string(),
            Some("client-a-t3".to_string()),
            Some("thread_01HZX".to_string()),
        )
    };
    let without = |what: &str| (what.to_string(), None, None);
    assert_eq!(
        told,
        [
            with("s1 permission_prompt"),
            with("s1 stop"),
            without("s2 permission_prompt"),
            without("s1 permission_prompt"),
        ]
    );
    gate::only_listed_fields(&mock);
}

#[test]
fn a_driver_reference_a_hook_writes_is_dropped_from_every_folder() {
    for from in [Folder::Container, Folder::Shared] {
        let rig = box_with_t3_beside_the_login();
        let mock = gate::watch_taking(&["driver_session"]);
        rig.sign_in(&mock.url);
        write(
            &folder(&rig, from),
            EventType::PermissionPrompt,
            0,
            Draft {
                driver_env: Some("client-a-t3".into()),
                driver_session: Some("thread_01HZX".into()),
                ..session("client-a", "s1")
            },
        );
        gate::tick(&rig);
        let told = sent(&mock);
        assert_eq!(told.len(), 1);
        for field in ["driver_env", "driver_session", "claim"] {
            assert!(told[0].get(field).is_none(), "{field}: {}", told[0]);
        }
    }
}

#[test]
fn a_hooks_claim_crosses_inside_its_spool_record_and_nowhere_else() {
    let rig = box_with_t3_beside_the_login();
    let worktree = rig.session("fix-login");
    let out = common::run(
        rig.in_container(
            rig.nucleated()
                .args(["hook", "SessionStart", "--env", "client-a"])
                .env("T3CODE_THREAD_ID", "thread_01HZX"),
        ),
        &serde_json::json!({ "session_id": "s1", "cwd": worktree }).to_string(),
    );
    assert_eq!(out.status.code(), Some(0));
    // The hook wrote its record, with its claim in it, and nothing else.
    let records = rig.spooled_in_container();
    assert_eq!(records.len(), 1);
    let claim = &records[0]["event"]["claim"];
    assert_eq!(claim["driver_env"], "client-a-t3");
    assert_eq!(claim["key"], "thread_01HZX");
    assert!(!claim["folders"].as_array().unwrap().is_empty());
    assert!(!claim.to_string().contains("fix-login"), "{claim}");
    assert!(claims(&rig).is_empty());
    assert!(records[0]["event"].get("driver_env").is_none());
    // The daemon reads it and counts it.
    gate::tick(&rig);
    assert_eq!(claims(&rig).len(), 1);
}

#[test]
fn thousands_of_claims_a_container_left_behind_do_not_slow_the_daemon_down() {
    // The same records, read by a box with no claims and by one holding
    // thousands, each made for some session that never ends.
    let pass = |planted: usize| {
        let rig = box_with_t3_beside_the_login();
        let driver = rig.home.state_dir().join("claims/client-a-t3");
        for n in 0..planted {
            let key = driver.join(format!("thread_{n:05}"));
            std::fs::create_dir_all(&key).unwrap();
            std::fs::write(key.join(format!("other{n}.json")), "client-a").unwrap();
        }
        let spool = rig.container_spool();
        for n in 0..200 {
            write(
                &spool,
                EventType::PermissionPrompt,
                n,
                session("client-a", &format!("s{n}")),
            );
        }
        write(
            &spool,
            EventType::PermissionPrompt,
            200,
            Draft {
                claim: Some(claim()),
                ..session("client-a", "s200")
            },
        );
        let started = Instant::now();
        gate::tick(&rig);
        let took = started.elapsed();
        let store = Store::new(&rig.home.state_dir());
        assert_eq!(store.list("client-a").len(), 201);
        assert!(entries(&spool).is_empty());
        (took, driver.join("thread_01HZX").exists())
    };
    let (plain, noted) = pass(0);
    assert!(noted);
    let (held, noted) = pass(10_000);
    // No more claims fit beside the ones already held.
    assert!(!noted);
    assert!(
        held < plain * 2 + Duration::from_millis(500),
        "{held:?} against {plain:?}"
    );
}
