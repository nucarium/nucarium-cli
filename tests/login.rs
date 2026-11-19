//! Signing a box in: the device flow against a local stand-in for the Watch
//! API, including every way it can go wrong, and the three commands as a
//! person or a playbook runs them.

mod common;

use common::{dead_api, mode, run, stderr, stdout_json, Home, Mock, Reply, Request};
use nucleate::login::{device_flow, Clock, Flow};
use nucleate::watch::{ApiError, Client};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::fs;
use std::time::{Duration, Instant};

const TOKEN: &str = "nuc_4m7q1x8z5b6n2f9k_3Yt0v1c2SECRETSECRET";
const OLD_TOKEN: &str = "nuc_0000000000000000_OLDOLDOLDOLDOLDOLD";
const DEVICE_CODE: &str = "dev_GmRhmhcxhwAzkoEqiMEg_DnyEysNkuNhszIySk9eS";

fn authorization(interval: u64, expires_in: u64) -> Reply {
    Reply::json(
        200,
        json!({
            "device_code": DEVICE_CODE,
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://app.nucarium.com/activate",
            "verification_uri_complete": "https://app.nucarium.com/activate?user_code=WDJB-MJHT",
            "expires_in": expires_in,
            "interval": interval,
        }),
    )
}

fn grant() -> Reply {
    Reply::json(
        200,
        json!({
            "access_token": TOKEN,
            "token_type": "Bearer",
            "scope": "ingest",
            "machine_id": "mac_7h2k9m4q1x8z5b6n",
            "machine_name": "agentbox",
            "account_name": "Your Org",
        }),
    )
}

/// A Watch API whose token endpoint gives `answers` in turn, then repeats the
/// last one.
fn api(interval: u64, expires_in: u64, answers: Vec<Reply>) -> Mock {
    Mock::start(move |request: &Request| match request.path.as_str() {
        "/v1/oauth/device_authorization" => authorization(interval, expires_in),
        "/v1/oauth/token" => answers[request.nth.min(answers.len() - 1)].clone(),
        "/v1/oauth/revoke" => Reply::json(200, json!({})),
        "/v1/ingest/events" => Reply::problem(400, "validation_failed"),
        _ => Reply::problem(404, "not_found"),
    })
}

/// A clock that never waits: each sleep moves it forward and is written down.
struct FakeClock {
    start: Instant,
    elapsed: Cell<Duration>,
    sleeps: RefCell<Vec<Duration>>,
    cancel_after: Option<usize>,
}

impl FakeClock {
    fn new() -> FakeClock {
        FakeClock {
            start: Instant::now(),
            elapsed: Cell::new(Duration::ZERO),
            sleeps: RefCell::new(Vec::new()),
            cancel_after: None,
        }
    }

    fn sleeps(&self) -> Vec<u64> {
        self.sleeps.borrow().iter().map(Duration::as_secs).collect()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.start + self.elapsed.get()
    }

    fn sleep(&self, duration: Duration) {
        self.elapsed.set(self.elapsed.get() + duration);
        self.sleeps.borrow_mut().push(duration);
    }

    fn cancelled(&self) -> bool {
        self.cancel_after
            .is_some_and(|after| self.sleeps.borrow().len() >= after)
    }
}

fn flow(mock_url: &str, clock: &FakeClock) -> (Result<Flow, ApiError>, String) {
    let client = Client::new(mock_url, Duration::from_secs(5)).unwrap();
    let mut shown = Vec::new();
    let result = device_flow(&client, Some("agentbox"), &mut shown, clock);
    (result, String::from_utf8(shown).unwrap())
}

// ---- the flow ---------------------------------------------------------

#[test]
fn approval_is_polled_for_at_the_servers_interval() {
    let mock = api(
        5,
        900,
        vec![
            Reply::oauth("authorization_pending"),
            Reply::oauth("authorization_pending"),
            grant(),
        ],
    );
    let clock = FakeClock::new();
    let (result, shown) = flow(&mock.url, &clock);
    let Ok(Flow::Granted(grant)) = result else {
        panic!("not granted: {result:?}");
    };
    assert_eq!(grant.token.expose(), TOKEN);
    assert_eq!(grant.machine_name.as_deref(), Some("agentbox"));
    assert_eq!(grant.account_name.as_deref(), Some("Your Org"));
    assert_eq!(clock.sleeps(), vec![5, 5, 5]);

    let asked = &mock.to("/v1/oauth/device_authorization")[0];
    assert_eq!(asked.method, "POST");
    assert_eq!(
        asked.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    let form = asked.form();
    assert_eq!(form["client_id"], "nucleate");
    assert_eq!(form["scope"], "ingest");
    assert_eq!(form["machine_name"], "agentbox");

    let polls = mock.to("/v1/oauth/token");
    assert_eq!(polls.len(), 3);
    let form = polls[0].form();
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(form["device_code"], DEVICE_CODE);
    assert_eq!(form["client_id"], "nucleate");

    // The person sees the code and the address; the device code stays private.
    assert!(shown.contains("WDJB-MJHT"));
    assert!(shown.contains("https://app.nucarium.com/activate"));
    assert!(!shown.contains(DEVICE_CODE));
}

#[test]
fn slow_down_lengthens_every_later_wait_by_five_seconds() {
    let mock = api(
        5,
        900,
        vec![
            Reply::oauth("slow_down"),
            Reply::oauth("authorization_pending"),
            Reply::oauth("slow_down"),
            grant(),
        ],
    );
    let clock = FakeClock::new();
    let (result, _) = flow(&mock.url, &clock);
    assert!(matches!(result, Ok(Flow::Granted(_))));
    assert_eq!(clock.sleeps(), vec![5, 10, 10, 15]);
}

#[test]
fn a_missing_interval_means_five_seconds() {
    let mock = Mock::start(|request: &Request| match request.path.as_str() {
        "/v1/oauth/device_authorization" => Reply::json(
            200,
            json!({
                "device_code": DEVICE_CODE, "user_code": "WDJB-MJHT",
                "verification_uri": "https://app.nucarium.com/activate", "expires_in": 900,
            }),
        ),
        _ => grant(),
    });
    let clock = FakeClock::new();
    let (result, shown) = flow(&mock.url, &clock);
    assert!(matches!(result, Ok(Flow::Granted(_))));
    assert_eq!(clock.sleeps(), vec![5]);
    // No complete link was offered, so none is shown.
    assert!(!shown.contains("already carries the code"));
}

#[test]
fn a_denial_ends_the_wait() {
    let mock = api(
        5,
        900,
        vec![
            Reply::oauth("authorization_pending"),
            Reply::oauth("access_denied"),
        ],
    );
    let (result, _) = flow(&mock.url, &FakeClock::new());
    assert!(matches!(result, Ok(Flow::Denied)));
    assert_eq!(mock.to("/v1/oauth/token").len(), 2);
}

#[test]
fn the_server_saying_the_code_expired_ends_the_wait() {
    let mock = api(5, 900, vec![Reply::oauth("expired_token")]);
    let (result, _) = flow(&mock.url, &FakeClock::new());
    assert!(matches!(result, Ok(Flow::Expired)));
}

#[test]
fn the_box_stops_asking_once_the_code_has_run_out() {
    let mock = api(5, 20, vec![Reply::oauth("authorization_pending")]);
    let clock = FakeClock::new();
    let (result, _) = flow(&mock.url, &clock);
    assert!(matches!(result, Ok(Flow::Expired)));
    // Polls at 5, 10 and 15 seconds; at 20 the code is spent and no more is asked.
    assert_eq!(mock.to("/v1/oauth/token").len(), 3);
    assert_eq!(clock.sleeps().iter().sum::<u64>(), 20);
}

#[test]
fn any_other_oauth_error_is_reported_not_retried() {
    let mock = api(5, 900, vec![Reply::oauth("invalid_grant")]);
    let (result, _) = flow(&mock.url, &FakeClock::new());
    let Err(ApiError::Refused { status, code, .. }) = result else {
        panic!("expected a refusal: {result:?}");
    };
    assert_eq!((status, code.as_deref()), (400, Some("invalid_grant")));
    assert_eq!(mock.to("/v1/oauth/token").len(), 1);
}

#[test]
fn a_refused_request_for_a_code_is_reported() {
    let mock = Mock::start(|_: &Request| {
        Reply::json(
            400,
            json!({ "error": "invalid_client", "error_description": "Unknown client." }),
        )
    });
    let (result, shown) = flow(&mock.url, &FakeClock::new());
    let Err(failure) = result else {
        panic!("expected a refusal");
    };
    assert!(failure.to_string().contains("invalid_client"));
    assert!(failure.to_string().contains("Unknown client."));
    assert_eq!(shown, "");
}

#[test]
fn a_dead_api_is_an_error_before_anything_is_shown() {
    let (result, shown) = flow(&dead_api(), &FakeClock::new());
    assert!(
        matches!(result, Err(ApiError::Unreachable(_))),
        "{result:?}"
    );
    assert_eq!(shown, "");
}

#[test]
fn a_slow_api_is_given_up_on() {
    let mock = Mock::start(|_: &Request| authorization(5, 900).after(Duration::from_millis(1500)));
    let client = Client::new(&mock.url, Duration::from_millis(300)).unwrap();
    let started = Instant::now();
    let result = device_flow(&client, None, &mut Vec::new(), &FakeClock::new());
    assert!(
        matches!(result, Err(ApiError::Unreachable(_))),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(1400));
}

#[test]
fn an_api_that_falters_during_the_wait_is_asked_less_often_and_then_succeeds() {
    let mock = api(
        5,
        900,
        vec![
            Reply::oauth("authorization_pending"),
            Reply::problem(503, "internal_error"),
            Reply::problem(502, "internal_error"),
            Reply::problem(500, "internal_error"),
            grant(),
        ],
    );
    let clock = FakeClock::new();
    let (result, _) = flow(&mock.url, &clock);
    assert!(matches!(result, Ok(Flow::Granted(_))), "{result:?}");
    assert_eq!(clock.sleeps(), vec![5, 5, 10, 20, 40]);
}

#[test]
fn an_api_that_never_comes_back_is_an_error_once_the_code_runs_out() {
    let mock = api(5, 120, vec![Reply::problem(503, "internal_error")]);
    let clock = FakeClock::new();
    let (result, _) = flow(&mock.url, &clock);
    assert_eq!(result.unwrap_err(), ApiError::Server { status: 503 });
    // The wait doubles to its ceiling of a minute and never passes the code's life.
    assert_eq!(clock.sleeps(), vec![5, 10, 20, 40, 45]);
}

#[test]
fn a_rate_limit_is_honoured() {
    let mock = api(
        5,
        900,
        vec![
            Reply::problem(429, "rate_limited").header("Retry-After", "30"),
            grant(),
        ],
    );
    let clock = FakeClock::new();
    let (result, _) = flow(&mock.url, &clock);
    assert!(matches!(result, Ok(Flow::Granted(_))));
    assert_eq!(clock.sleeps(), vec![5, 30]);
}

#[test]
fn cancelling_stops_the_wait_without_another_request() {
    let mock = api(5, 900, vec![grant()]);
    let mut clock = FakeClock::new();
    clock.cancel_after = Some(1);
    let (result, _) = flow(&mock.url, &clock);
    assert!(matches!(result, Ok(Flow::Cancelled)));
    assert!(mock.to("/v1/oauth/token").is_empty());
}

#[test]
fn a_grant_without_a_usable_token_is_not_accepted() {
    let mock = api(
        5,
        900,
        vec![Reply::json(
            200,
            json!({ "access_token": "", "token_type": "Bearer" }),
        )],
    );
    let (result, _) = flow(&mock.url, &FakeClock::new());
    assert!(matches!(result, Err(ApiError::Unexpected(_))), "{result:?}");
}

// ---- nucleate login ---------------------------------------------------

fn login(home: &Home, mock: &Mock, args: &[&str], stdin: &str) -> std::process::Output {
    run(
        home.nucleate()
            .arg("login")
            .args(args)
            .env("NUCARIUM_API_URL", &mock.url),
        stdin,
    )
}

fn seed_token(home: &Home, token: &str, api_url: &str) {
    let out = run(
        home.nucleate()
            .args(["login", "--token-stdin", "--api-url", api_url]),
        token,
    );
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn login_stores_the_token_privately_and_never_prints_it() {
    let home = Home::new();
    let mock = api(1, 900, vec![Reply::oauth("authorization_pending"), grant()]);
    let out = login(&home, &mock, &["--name", "agentbox"], "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let printed = stdout_json(&out);
    assert_eq!(printed["logged_in"], true);
    assert_eq!(printed["api_url"], mock.url);
    assert_eq!(printed["machine_id"], "mac_7h2k9m4q1x8z5b6n");
    assert_eq!(printed["machine_name"], "agentbox");
    assert_eq!(printed["account_name"], "Your Org");
    assert_eq!(printed["scope"], "ingest");
    assert_eq!(printed["replaced"], false);
    assert_eq!(printed["token_file"], home.token_file().to_str().unwrap());

    let said = stderr(&out);
    assert!(said.contains("WDJB-MJHT"));
    assert!(said.contains("https://app.nucarium.com/activate\n"));
    assert!(said.contains("https://app.nucarium.com/activate?user_code=WDJB-MJHT"));
    // What was just granted, in one line.
    assert!(said.contains("agentbox can now report to Your Org. It can read nothing."));

    // The token is in its file and nowhere else.
    for output in [&out.stdout, &out.stderr] {
        assert!(!String::from_utf8_lossy(output).contains(TOKEN));
        assert!(!String::from_utf8_lossy(output).contains("SECRET"));
    }
    assert_eq!(fs::read_to_string(home.token_file()).unwrap().trim(), TOKEN);
    assert_eq!(mode(&home.token_file()), 0o600);
    assert_eq!(mode(&home.login_file()), 0o600);
    assert_eq!(mode(home.token_file().parent().unwrap()), 0o700);
    let details = fs::read_to_string(home.login_file()).unwrap();
    assert!(!details.contains(TOKEN));
    assert!(details.contains("mac_7h2k9m4q1x8z5b6n"));

    assert_eq!(
        mock.to("/v1/oauth/device_authorization")[0].form()["machine_name"],
        "agentbox"
    );
}

#[test]
fn login_draws_the_code_large() {
    let home = Home::new();
    let mock = api(1, 900, vec![grant()]);
    let said = stderr(&login(&home, &mock, &[], ""));
    // Five rows of block letters above the code itself.
    let rows: Vec<&str> = said.lines().filter(|line| line.contains("#   #")).collect();
    assert!(rows.len() >= 4, "{said}");
    assert!(said.lines().any(|line| line.trim() == "WDJB-MJHT"));
}

#[test]
fn a_denied_login_exits_3_and_stores_nothing() {
    let home = Home::new();
    let mock = api(1, 900, vec![Reply::oauth("access_denied")]);
    let out = login(&home, &mock, &[], "");
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout_json(&out)["logged_in"], false);
    assert_eq!(stdout_json(&out)["reason"], "access_denied");
    assert!(stderr(&out).contains("denied"));
    assert!(!home.token_file().exists());
    assert!(!home.login_file().exists());
}

#[test]
fn an_expired_login_exits_4_and_stores_nothing() {
    let home = Home::new();
    let mock = api(1, 900, vec![Reply::oauth("expired_token")]);
    let out = login(&home, &mock, &[], "");
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(stdout_json(&out)["reason"], "expired_token");
    assert!(stderr(&out).contains("expired"));
    assert!(!home.token_file().exists());
}

#[test]
fn an_unreachable_api_exits_1_and_stores_nothing() {
    let home = Home::new();
    let out = run(
        home.nucleate().args(["login", "--api-url", &dead_api()]),
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(
        stderr(&out).contains("could not be reached"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("Nothing was stored"));
    assert!(!home.token_file().exists());
}

#[test]
fn ctrl_c_cancels_cleanly() {
    let home = Home::new();
    let mock = api(1, 900, vec![Reply::oauth("authorization_pending")]);
    let child = home
        .nucleate()
        .arg("login")
        .env("NUCARIUM_API_URL", &mock.url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until it is polling, then interrupt it as a terminal would.
    let started = Instant::now();
    while mock.to("/v1/oauth/token").is_empty() {
        assert!(started.elapsed() < Duration::from_secs(20), "never polled");
        std::thread::sleep(Duration::from_millis(50));
    }
    // `kill` is a shell builtin everywhere; not every system has it as a program.
    let killed = std::process::Command::new("sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    assert!(killed.success());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["reason"], "cancelled");
    assert!(stderr(&out).contains("Cancelled. Nothing was stored."));
    assert!(!home.token_file().exists());
}

#[test]
fn the_api_address_comes_from_the_flag_then_the_environment_then_the_config() {
    let home = Home::new();
    let (from_flag, from_env, from_config) = (
        api(1, 900, vec![grant()]),
        api(1, 900, vec![grant()]),
        api(1, 900, vec![grant()]),
    );
    home.write_config(&format!("[watch]\nurl = \"{}\"\n", from_config.url));

    let out = run(home.nucleate().arg("login"), "");
    assert_eq!(stdout_json(&out)["api_url"], from_config.url);

    let out = run(
        home.nucleate()
            .arg("login")
            .env("NUCARIUM_API_URL", &from_env.url),
        "",
    );
    assert_eq!(stdout_json(&out)["api_url"], from_env.url);

    let out = run(
        home.nucleate()
            .args(["login", "--api-url", &from_flag.url])
            .env("NUCARIUM_API_URL", &from_env.url),
        "",
    );
    assert_eq!(stdout_json(&out)["api_url"], from_flag.url);
    for mock in [&from_flag, &from_env, &from_config] {
        assert_eq!(mock.to("/v1/oauth/device_authorization").len(), 1);
    }
}

#[test]
fn plain_http_is_refused_for_any_host_but_this_machine() {
    let home = Home::new();
    let out = run(
        home.nucleate()
            .args(["login", "--api-url", "http://watch.example.test"]),
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not https"));
    assert!(!home.token_file().exists());
}

#[test]
fn the_machine_name_defaults_to_the_one_in_the_config() {
    let home = Home::new();
    let mock = api(1, 900, vec![grant()]);
    home.write_config("machine = \"ops\"\n");
    login(&home, &mock, &[], "");
    assert_eq!(
        mock.to("/v1/oauth/device_authorization")[0].form()["machine_name"],
        "ops"
    );
}

#[test]
fn logging_in_again_rotates_the_token() {
    let home = Home::new();
    let mock = api(1, 900, vec![grant()]);
    seed_token(&home, OLD_TOKEN, &mock.url);
    let out = login(&home, &mock, &[], "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let printed = stdout_json(&out);
    assert_eq!(printed["replaced"], true);
    assert_eq!(printed["previous_revoked"], true);
    assert_eq!(fs::read_to_string(home.token_file()).unwrap().trim(), TOKEN);
    // The token that was replaced is the one revoked.
    let revoked = mock.to("/v1/oauth/revoke");
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].form()["token"], OLD_TOKEN);
    assert!(!String::from_utf8_lossy(&out.stderr).contains(OLD_TOKEN));
}

// ---- nucleate login --token-stdin ------------------------------------

#[test]
fn a_minted_token_is_stored_from_stdin_without_contacting_watch() {
    let home = Home::new();
    let mock = api(1, 900, vec![grant()]);
    let out = login(&home, &mock, &["--token-stdin"], &format!("{TOKEN}\n"));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let printed = stdout_json(&out);
    assert_eq!(printed["logged_in"], true);
    assert_eq!(printed["api_url"], mock.url);
    assert!(printed["machine_id"].is_null());
    assert!(!String::from_utf8_lossy(&out.stdout).contains(TOKEN));
    assert!(!stderr(&out).contains(TOKEN));
    assert!(stderr(&out).contains("It can read nothing."));
    assert_eq!(fs::read_to_string(home.token_file()).unwrap().trim(), TOKEN);
    assert_eq!(mode(&home.token_file()), 0o600);
    assert!(mock.requests().is_empty());
}

#[test]
fn something_that_is_not_a_token_is_not_stored() {
    let home = Home::new();
    for input in ["", "two words here\n", "short"] {
        let out = run(home.nucleate().args(["login", "--token-stdin"]), input);
        assert_eq!(out.status.code(), Some(1), "{input:?}");
        assert!(!home.token_file().exists());
    }
}

#[test]
fn the_token_file_setting_moves_the_credential() {
    let home = Home::new();
    let elsewhere = home.dir.join("secrets/watch-token");
    home.write_config(&format!(
        "[watch]\ntoken_file = \"{}\"\n",
        elsewhere.display()
    ));
    let out = run(home.nucleate().args(["login", "--token-stdin"]), TOKEN);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&elsewhere).unwrap().trim(), TOKEN);
    assert_eq!(mode(&elsewhere), 0o600);
    assert!(elsewhere.with_file_name("login.json").is_file());
    assert!(!home.token_file().exists());
}

// ---- nucleate whoami --------------------------------------------------

#[test]
fn whoami_says_who_the_box_reports_as_and_that_the_token_works() {
    let home = Home::new();
    let mock = api(1, 900, vec![grant()]);
    login(&home, &mock, &[], "");
    let out = run(home.nucleate().arg("whoami"), "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let printed = stdout_json(&out);
    assert_eq!(printed["logged_in"], true);
    assert_eq!(printed["machine_name"], "agentbox");
    assert_eq!(printed["account_name"], "Your Org");
    assert_eq!(printed["api_url"], mock.url);
    assert_eq!(printed["token"], "valid");
    assert!(!String::from_utf8_lossy(&out.stdout).contains(TOKEN));

    // The check reports nothing: no heartbeat, and a batch with no events.
    assert!(mock.to("/v1/ingest/heartbeat").is_empty());
    let probe = &mock.to("/v1/ingest/events")[0];
    assert_eq!(
        probe.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(probe.json(), json!({ "events": [] }));
}

#[test]
fn whoami_exits_6_when_watch_rejects_the_token() {
    let home = Home::new();
    let mock = Mock::start(|_: &Request| Reply::problem(401, "unauthorized"));
    seed_token(&home, TOKEN, &mock.url);
    let out = run(home.nucleate().arg("whoami"), "");
    assert_eq!(out.status.code(), Some(6));
    assert_eq!(stdout_json(&out)["token"], "rejected");
    assert!(stderr(&out).contains("nucleate login"));
}

#[test]
fn whoami_exits_1_when_watch_cannot_be_asked() {
    let home = Home::new();
    seed_token(&home, TOKEN, &dead_api());
    let out = run(home.nucleate().arg("whoami"), "");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout_json(&out)["logged_in"], true);
    assert_eq!(stdout_json(&out)["token"], "unknown");

    let home = Home::new();
    let failing = Mock::start(|_: &Request| Reply::problem(500, "internal_error"));
    seed_token(&home, TOKEN, &failing.url);
    let out = run(home.nucleate().arg("whoami"), "");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout_json(&out)["token"], "unknown");
}

#[test]
fn whoami_exits_5_when_the_box_is_not_signed_in() {
    let home = Home::new();
    let out = run(home.nucleate().arg("whoami"), "");
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(stdout_json(&out)["logged_in"], false);
}

#[test]
fn a_token_file_others_can_read_is_not_used() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    let mock = api(1, 900, vec![grant()]);
    seed_token(&home, TOKEN, &mock.url);
    fs::set_permissions(home.token_file(), fs::Permissions::from_mode(0o644)).unwrap();
    let out = run(home.nucleate().arg("whoami"), "");
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(stdout_json(&out)["reason"], "token_file_permissions");
    assert!(stderr(&out).contains("644"));
    assert!(mock.requests().is_empty());
}

// ---- nucleate logout --------------------------------------------------

#[test]
fn logout_revokes_the_token_then_deletes_it() {
    let home = Home::new();
    let mock = api(1, 900, vec![grant()]);
    login(&home, &mock, &[], "");
    let out = run(home.nucleate().arg("logout"), "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let printed = stdout_json(&out);
    assert_eq!(printed["logged_out"], true);
    assert_eq!(printed["revoked"], true);
    let revoke = &mock.to("/v1/oauth/revoke")[0];
    assert_eq!(revoke.form()["token"], TOKEN);
    assert_eq!(revoke.form()["client_id"], "nucleate");
    assert!(!home.token_file().exists());
    assert!(!home.login_file().exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains(TOKEN));
    assert!(!stderr(&out).contains(TOKEN));
}

#[test]
fn logout_still_deletes_the_token_when_watch_cannot_be_reached_and_says_so() {
    let home = Home::new();
    seed_token(&home, TOKEN, &dead_api());
    let out = run(home.nucleate().arg("logout"), "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let printed = stdout_json(&out);
    assert_eq!(printed["logged_out"], true);
    assert_eq!(printed["revoked"], false);
    assert!(stderr(&out).contains("Watch could not be told"));
    assert!(stderr(&out).contains("revoke it in the console"));
    assert!(!home.token_file().exists());
    assert!(!home.login_file().exists());
}

#[test]
fn logout_still_deletes_the_token_when_watch_refuses_the_revocation() {
    let home = Home::new();
    let mock = Mock::start(|_: &Request| Reply::problem(500, "internal_error"));
    seed_token(&home, TOKEN, &mock.url);
    let out = run(home.nucleate().arg("logout"), "");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout_json(&out)["revoked"], false);
    assert!(!home.token_file().exists());
}

#[test]
fn logout_with_nothing_to_log_out_of_is_not_an_error() {
    let home = Home::new();
    let out = run(home.nucleate().arg("logout"), "");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout_json(&out)["logged_out"], false);
    assert_eq!(stdout_json(&out)["reason"], "not_logged_in");
}
