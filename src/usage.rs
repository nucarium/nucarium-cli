//! Plan usage: how much of its Claude subscription each Claude Code login on
//! the box has used, read on a thread of its own and said in the heartbeat.
//!
//! Every [`EVERY`], for each environment that is one Claude Code login with a
//! config folder, the thread reads the login's access token from
//! `<config_dir>/.credentials.json` and asks the vendor's usage endpoint,
//! [`USAGE_URL`], how much of each limit is used and when it resets. A good
//! answer is kept in `usage.json` in the state folder, where the heartbeat
//! reads it, and it survives a restart of the daemon.
//!
//! The endpoint is not documented by its vendor, and its shape may change
//! without notice. Unknown fields are passed over, and an answer this file
//! cannot read counts as a failed read. A failed read changes nothing that
//! was kept: the heartbeat goes on saying the last good reading, with the
//! time it was read, so Watch can tell how old it is. An environment that has
//! never had a good reading says nothing.
//!
//! The token: it is read from the file into a [`Token`], whose `Debug` hides
//! it, and goes into one request's `Authorization` header and nowhere else.
//! It is never logged, never in an error, never written down and never sent
//! to any other address. It is never refreshed either: a refresh would
//! replace the agent's own refresh token and could sign the environment out.
//! A token that has expired, or is about to, waits for the agent to refresh
//! it. Nothing is ever written into a config folder.

use crate::credentials::Token;
use crate::envelope::unix_ms;
use crate::settings::{self, Environment};
use crate::{housekeeping, paths};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The vendor's usage endpoint for a subscription login.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// How often each environment's usage is read.
pub const EVERY: Duration = Duration::from_secs(300);
/// The most limits a heartbeat carries for one environment.
pub const MAX_LIMITS: usize = 16;
/// How long after the daemon starts the first read is made.
const FIRST_AFTER: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(15);
/// A token that expires within this is left for the agent to refresh.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const MAX_ANSWER: u64 = 256 * 1024;
const MAX_CREDENTIALS: u64 = 64 * 1024;

/// The window a limit counts over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    FiveHour,
    SevenDay,
}

/// One limit as it was read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    pub window: Window,
    /// The model the limit is for. Nothing for a limit on all use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub used_pct: f64,
    /// Nothing when the window has not started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
}

/// The last good reading for one environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub read_at_ms: u64,
    pub limits: Vec<Limit>,
}

/// What `usage.json` holds, by environment name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Readings {
    #[serde(default)]
    pub environments: BTreeMap<String, Reading>,
}

/// Why a read did not give a reading. Fixed words and an HTTP status: never
/// the answer's body, and never anything from the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No credentials file that holds a token and its expiry.
    NoLogin,
    /// The token has expired, or is about to.
    Expired,
    /// The endpoint answered with a status other than 200.
    Status(u16),
    /// No answer: a timeout, no route, a TLS failure.
    Transport(&'static str),
    /// An answer this file cannot read.
    Unreadable,
    /// The reading could not be kept in the state folder.
    NotKept,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::NoLogin => f.write_str("no_login"),
            Failure::Expired => f.write_str("login_expired"),
            Failure::Status(status) => write!(f, "status {status}"),
            Failure::Transport(kind) => f.write_str(kind),
            Failure::Unreadable => f.write_str("unreadable_answer"),
            Failure::NotKept => f.write_str("not_written"),
        }
    }
}

pub fn readings_file(state_dir: &Path) -> PathBuf {
    state_dir.join("usage.json")
}

/// What the thread last kept, or nothing when it has kept nothing. One small
/// file: the heartbeat reads it.
pub fn read_readings(state_dir: &Path) -> Option<Readings> {
    serde_json::from_str(&fs::read_to_string(readings_file(state_dir)).ok()?).ok()
}

fn write_readings(state_dir: &Path, readings: &Readings) -> Result<()> {
    paths::write_private(
        &readings_file(state_dir),
        serde_json::to_string(readings)?.as_bytes(),
    )
}

/// Whether an environment's plan usage is read: one Claude Code login, with
/// a config folder. An environment watched through a driver is not one login.
pub fn reads(environment: &Environment) -> bool {
    environment.agent == "claude-code"
        && environment.driver.is_none()
        && environment.config_dir.is_some()
}

/// What a heartbeat says about an environment's plan usage: its last good
/// reading, held to the shape Watch takes, or nothing.
pub fn report(
    readings: Option<&Readings>,
    environment: &Environment,
    now: SystemTime,
) -> Option<crate::watch::UsageReport> {
    if !reads(environment) {
        return None;
    }
    let reading = readings?.environments.get(&environment.name)?;
    Some(crate::watch::UsageReport::new(
        UNIX_EPOCH + Duration::from_millis(reading.read_at_ms),
        &reading.limits,
        now,
    ))
}

/// `^[0-9A-Za-z .+-]{1,32}$`: a model's name and nothing longer.
fn valid_scope(scope: &str) -> bool {
    (1..=32).contains(&scope.len())
        && scope
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b' ' | b'.' | b'+' | b'-'))
}

/// Hold limits to what Watch takes: a percentage from 0 to 100, a scope that
/// is a model's name, no two for the same window and scope (the first one
/// stays), and at most [`MAX_LIMITS`].
pub fn tidy(limits: impl IntoIterator<Item = Limit>) -> Vec<Limit> {
    let mut seen = BTreeSet::new();
    limits
        .into_iter()
        .filter(|limit| limit.used_pct.is_finite())
        .filter(|limit| limit.scope.as_deref().is_none_or(valid_scope))
        .filter(|limit| seen.insert((limit.window, limit.scope.clone())))
        .map(|limit| Limit {
            used_pct: limit.used_pct.clamp(0.0, 100.0),
            ..limit
        })
        .take(MAX_LIMITS)
        .collect()
}

/// The access token in a Claude Code config folder, when there is one that
/// has not expired. Only `accessToken` and `expiresAt` are read.
pub fn access_token(config_dir: &Path, now: SystemTime) -> Result<Token, Failure> {
    #[derive(Deserialize)]
    struct File {
        #[serde(rename = "claudeAiOauth")]
        oauth: Option<OAuth>,
    }
    #[derive(Deserialize)]
    struct OAuth {
        #[serde(rename = "accessToken")]
        access_token: Option<String>,
        #[serde(rename = "expiresAt")]
        expires_at: Option<u64>,
    }
    let mut text = String::new();
    fs::File::open(config_dir.join(".credentials.json"))
        .and_then(|file| file.take(MAX_CREDENTIALS).read_to_string(&mut text))
        .map_err(|_| Failure::NoLogin)?;
    // serde's own error could quote a value from the file, so it is dropped
    // unread.
    let oauth = serde_json::from_str::<File>(&text)
        .ok()
        .and_then(|file| file.oauth)
        .ok_or(Failure::NoLogin)?;
    let expires_at = oauth.expires_at.ok_or(Failure::NoLogin)?;
    let token = oauth
        .access_token
        .and_then(|raw| Token::parse(&raw).ok())
        .ok_or(Failure::NoLogin)?;
    if expires_at <= unix_ms(now + EXPIRY_MARGIN) {
        return Err(Failure::Expired);
    }
    Ok(token)
}

/// The HTTP client for the usage endpoint: a time limit, and no redirect,
/// which could carry the token to another host.
pub fn client() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .user_agent(concat!("nucleated/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Ask the endpoint at `url` with `token`, and return its answer's body.
pub fn fetch(client: &ureq::Agent, url: &str, token: &Token) -> Result<String, Failure> {
    let mut response = client
        .get(url)
        .header("Authorization", format!("Bearer {}", token.expose()))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Accept", "application/json")
        .call()
        .map_err(transport)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(Failure::Status(status));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_ANSWER)
        .read_to_string()
        .map_err(transport)
}

/// One fixed word for an error that left no answer. ureq's own words for it
/// are not used: they may name the address.
fn transport(error: ureq::Error) -> Failure {
    Failure::Transport(match error {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Io(_) => {
            "unreachable"
        }
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "tls",
        ureq::Error::BodyExceedsLimit(_) => "oversized_answer",
        _ => "transport",
    })
}

/// A percentage: nothing for null, and no reading for anything but a number.
fn percent(value: Option<&Value>) -> Option<Option<f64>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::Number(number)) => number.as_f64().map(Some),
        Some(_) => None,
    }
}

/// A reset time: nothing for null, and no reading for anything but a time.
fn reset(value: Option<&Value>) -> Option<Option<u64>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => parse_time(text).map(|at| Some(unix_ms(at))),
        Some(_) => None,
    }
}

/// A top-level window such as `five_hour`: `{ utilization, resets_at }`, or
/// null. The outer `None` is an answer that cannot be read; the inner one,
/// nothing to say.
fn window(value: Option<&Value>, window: Window, scope: Option<&str>) -> Option<Option<Limit>> {
    let object = match value {
        None | Some(Value::Null) => return Some(None),
        Some(Value::Object(object)) => object,
        Some(_) => return None,
    };
    let used = percent(object.get("utilization"))?;
    let resets_at_ms = reset(object.get("resets_at"))?;
    Some(used.map(|used_pct| Limit {
        window,
        scope: scope.map(str::to_string),
        used_pct,
        resets_at_ms,
    }))
}

/// One entry of `limits`: a weekly limit on one model, or nothing to say.
fn scoped(entry: &Value) -> Option<Option<Limit>> {
    let entry = entry.as_object()?;
    if entry.get("group").and_then(Value::as_str) != Some("weekly") {
        return Some(None);
    }
    let Some(model) = entry
        .get("scope")
        .and_then(|scope| scope.get("model"))
        .and_then(|model| model.get("display_name"))
        .and_then(Value::as_str)
    else {
        return Some(None);
    };
    let used = percent(entry.get("percent"))?;
    let resets_at_ms = reset(entry.get("resets_at"))?;
    Some(used.map(|used_pct| Limit {
        window: Window::SevenDay,
        scope: Some(model.to_string()),
        used_pct,
        resets_at_ms,
    }))
}

/// Read the endpoint's answer. `None` when it is not an answer this file
/// can read: not an object, none of the fields it reads, or one of them of
/// another shape than it had.
pub fn parse(body: &str) -> Option<Vec<Limit>> {
    let answer: Value = serde_json::from_str(body).ok()?;
    let answer = answer.as_object()?;
    if !["five_hour", "seven_day", "limits"]
        .iter()
        .any(|key| answer.contains_key(*key))
    {
        return None;
    }
    let mut limits = Vec::new();
    limits.extend(window(answer.get("five_hour"), Window::FiveHour, None)?);
    limits.extend(window(answer.get("seven_day"), Window::SevenDay, None)?);
    match answer.get("limits") {
        None | Some(Value::Null) => {}
        Some(Value::Array(entries)) => {
            for entry in entries {
                limits.extend(scoped(entry)?);
            }
        }
        Some(_) => return None,
    }
    // Older fields for the same thing: used only when `limits` did not
    // already name the model.
    for (key, model) in [("seven_day_opus", "Opus"), ("seven_day_sonnet", "Sonnet")] {
        limits.extend(window(answer.get(key), Window::SevenDay, Some(model))?);
    }
    Some(tidy(limits))
}

/// An RFC 3339 time, such as `2026-11-25T12:59:59.365551+00:00`, to the
/// millisecond. Nothing for anything else, or a time before 1970.
fn parse_time(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    let digits = |from: usize, len: usize| -> Option<i64> {
        let part = bytes.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0i64, |n, c| n * 10 + i64::from(c - b'0')))
    };
    let sep = |at: usize, one: &[u8]| bytes.get(at).is_some_and(|c| one.contains(c));
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    let (hour, minute, second) = (digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    if !(sep(4, b"-") && sep(7, b"-") && sep(10, b"Tt ") && sep(13, b":") && sep(16, b":")) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut at = 19;
    let mut millis = 0i64;
    if sep(at, b".") {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return None;
        }
        let fraction = &text[start..at];
        millis = format!("{:0<3}", &fraction[..fraction.len().min(3)])
            .parse()
            .ok()?;
    }
    let offset = match bytes.get(at)? {
        b'Z' | b'z' if at + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if at + 6 == bytes.len() && sep(at + 3, b":") => {
            let (hours, minutes) = (digits(at + 1, 2)?, digits(at + 4, 2)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3600 + minutes * 60;
            if *sign == b'-' {
                -offset
            } else {
                offset
            }
        }
        _ => return None,
    };
    // Days since the epoch from a civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    let ms = u64::try_from(seconds * 1000 + millis).ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(ms))
}

/// Read one environment's plan usage with `fetch`, and keep it when it is
/// good. A failure keeps what was kept before.
pub fn read_one(
    state_dir: &Path,
    environment: &Environment,
    fetch: impl FnOnce(&Token) -> Result<String, Failure>,
    now: SystemTime,
) -> Result<(), Failure> {
    let config_dir = environment.config_dir.as_deref().ok_or(Failure::NoLogin)?;
    let body = fetch(&access_token(config_dir, now)?)?;
    let limits = parse(&body).ok_or(Failure::Unreadable)?;
    let mut readings = read_readings(state_dir).unwrap_or_default();
    readings.environments.insert(
        environment.name.clone(),
        Reading {
            read_at_ms: unix_ms(now),
            limits,
        },
    );
    write_readings(state_dir, &readings).map_err(|_| Failure::NotKept)
}

/// Drop what was kept for an environment whose usage is no longer read.
fn forget_others(state_dir: &Path, keep: &BTreeSet<&str>) {
    let Some(mut readings) = read_readings(state_dir) else {
        return;
    };
    let before = readings.environments.len();
    readings
        .environments
        .retain(|name, _| keep.contains(name.as_str()));
    if readings.environments.len() != before {
        let _ = write_readings(state_dir, &readings);
    }
}

/// When an environment's first read is due, after the thread's first pass:
/// spread over [`EVERY`], so a box's logins are not all asked at once.
fn spread(place: usize, count: usize) -> Duration {
    EVERY.mul_f64(place as f64 / count.max(1) as f64)
}

fn log(line: &str) {
    eprintln!("nucleated: {line}");
}

/// The plan-usage thread: a little after start, then every [`EVERY`] for
/// each environment it reads. The config is read afresh on each pass. A
/// failure is logged with the environment's name and a fixed word, once,
/// when it first appears.
pub fn run_schedule(config: &Path, stop: &AtomicBool) {
    if housekeeping::wait(stop, FIRST_AFTER) {
        return;
    }
    let client = client();
    let mut due: BTreeMap<String, Instant> = BTreeMap::new();
    let mut said: BTreeMap<String, String> = BTreeMap::new();
    let mut known: Option<BTreeSet<String>> = None;
    loop {
        if let Ok(settings) = settings::load_from(config) {
            let read: Vec<&Environment> = settings
                .environments
                .iter()
                .filter(|environment| reads(environment))
                .collect();
            for (place, environment) in read.iter().enumerate() {
                let name = &environment.name;
                let next = *due
                    .entry(name.clone())
                    .or_insert_with(|| Instant::now() + spread(place, read.len()));
                if Instant::now() < next {
                    continue;
                }
                due.insert(name.clone(), Instant::now() + EVERY);
                // A read that panics has done nothing. It costs this read and
                // never the daemon.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    read_one(
                        &settings.state_dir,
                        environment,
                        |token| fetch(&client, USAGE_URL, token),
                        SystemTime::now(),
                    )
                }));
                match outcome {
                    Ok(Ok(())) => {
                        if said.remove(name).is_some() {
                            log(&format!("environment {name}: plan usage read again"));
                        }
                    }
                    Ok(Err(failure)) => {
                        let word = failure.to_string();
                        if said.get(name) != Some(&word) {
                            log(&format!(
                                "environment {name}: plan usage not read ({word}); \
                                 the last reading stands"
                            ));
                            said.insert(name.clone(), word);
                        }
                    }
                    Err(_) => {}
                }
                if stop.load(Ordering::SeqCst) {
                    return;
                }
            }
            let names: BTreeSet<&str> = read.iter().map(|e| e.name.as_str()).collect();
            due.retain(|name, _| names.contains(name.as_str()));
            said.retain(|name, _| names.contains(name.as_str()));
            // Only when the environments read have changed: not every pass.
            let owned: BTreeSet<String> = names.iter().map(|name| name.to_string()).collect();
            if known.as_ref() != Some(&owned) {
                forget_others(&settings.state_dir, &names);
                known = Some(owned);
            }
        }
        // With nothing to read, there is nothing to be on time for.
        let pause = if due.is_empty() { 15 } else { 1 };
        if housekeeping::wait(stop, Duration::from_secs(pause)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::UsageReport;
    use serde_json::json;

    const SAMPLE: &str = r#"{"five_hour":null,"seven_day":{"utilization":100.0,"resets_at":"2026-11-25T12:59:59.365551+00:00","limit_dollars":null},"seven_day_opus":null,"seven_day_sonnet":null,"extra_usage":{"is_enabled":false},"limits":[{"kind":"weekly_all","group":"weekly","percent":100,"severity":"critical","resets_at":"2026-11-25T12:59:59.365551+00:00","scope":null,"is_active":true},{"kind":"weekly_scoped","group":"weekly","percent":71,"severity":"normal","resets_at":"2026-11-25T13:00:00.365724+00:00","scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},"is_active":false}],"seven_day_breakdown":{"as_of":"2026-11-24T08:13:34.446762+00:00","rows":[]}}"#;
    const TOKEN: &str = "sk-ant-oat01-abcdefghijklmnopqrstuvwxyz0123456789";

    fn at(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    /// 2026-11-24T08:00:00.000Z
    fn now() -> SystemTime {
        at(1_795_507_200_000)
    }

    /// What a heartbeat would carry for an answer, read at `now()`.
    fn sent(body: &str) -> Option<Value> {
        let limits = parse(body)?;
        Some(
            serde_json::to_value(UsageReport::new(now(), &limits, now())).unwrap()["limits"]
                .clone(),
        )
    }

    fn claude_env(config_dir: Option<&Path>) -> Environment {
        Environment {
            name: "client-a".into(),
            root: PathBuf::from("/nonexistent"),
            unit: "agent-client-a".into(),
            check: None,
            agent: "claude-code".into(),
            driver: None,
            config_dir: config_dir.map(Path::to_path_buf),
            github_token_file: None,
            branch_prefix: None,
        }
    }

    fn credentials(dir: &Path, expires_at_ms: u64) {
        fs::write(
            dir.join(".credentials.json"),
            json!({ "claudeAiOauth": {
                "accessToken": TOKEN,
                "refreshToken": "sk-ant-ort01-the-agents-own-refresh-token",
                "expiresAt": expires_at_ms,
                "scopes": ["user:inference"],
                "subscriptionType": "max",
                "rateLimitTier": "default",
            } })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn the_sample_answer_is_an_all_use_weekly_limit_and_one_for_a_model() {
        assert_eq!(
            sent(SAMPLE),
            Some(json!([
                { "window": "seven_day", "used_pct": 100.0, "resets_at": "2026-11-25T12:59:59.365Z" },
                { "window": "seven_day", "scope": "Fable", "used_pct": 71.0, "resets_at": "2026-11-25T13:00:00.365Z" },
            ]))
        );
    }

    #[test]
    fn a_five_hour_window_and_the_older_model_fields_are_read() {
        let body = json!({
            "five_hour": { "utilization": 23.0, "resets_at": "2026-11-24T10:00:00Z" },
            "seven_day": { "utilization": 40.5, "resets_at": null },
            "seven_day_opus": { "utilization": 12, "resets_at": "2026-11-27T00:00:00.000-05:00" },
            "seven_day_sonnet": { "utilization": null, "resets_at": null },
            "something_new": { "utilization": "a lot" },
        });
        assert_eq!(
            sent(&body.to_string()),
            Some(json!([
                { "window": "five_hour", "used_pct": 23.0, "resets_at": "2026-11-24T10:00:00.000Z" },
                { "window": "seven_day", "used_pct": 40.5 },
                { "window": "seven_day", "scope": "Opus", "used_pct": 12.0, "resets_at": "2026-11-27T05:00:00.000Z" },
            ]))
        );
        // A window not yet started, and nothing used: still a reading.
        assert_eq!(
            sent(r#"{"five_hour":null,"seven_day":null}"#),
            Some(json!([]))
        );
    }

    #[test]
    fn an_answer_of_another_shape_is_no_reading() {
        for body in [
            "",
            "not json",
            "[]",
            "null",
            r#"{"error":{"type":"authentication_error"}}"#,
            r#"{"five_hour":"23%"}"#,
            r#"{"five_hour":{"utilization":"23"}}"#,
            r#"{"seven_day":{"utilization":5,"resets_at":"next tuesday"}}"#,
            r#"{"seven_day":{"utilization":5,"resets_at":1795507200}}"#,
            r#"{"limits":{"weekly":1}}"#,
            r#"{"limits":["weekly"]}"#,
            r#"{"limits":[{"group":"weekly","percent":"71","scope":{"model":{"display_name":"Fable"}}}]}"#,
        ] {
            assert_eq!(parse(body), None, "{body}");
        }
    }

    #[test]
    fn limits_are_clamped_filtered_deduplicated_and_capped() {
        let entry = |name: &str, percent: Value| {
            json!({ "group": "weekly", "percent": percent, "resets_at": null,
                    "scope": { "model": { "display_name": name } } })
        };
        let mut entries = vec![
            entry("Fable", json!(130)),
            entry("Fable", json!(10)),
            entry("Opus", json!(-4)),
            entry("Sonnet", Value::Null),
            entry("a model\nwith a sentence in it", json!(5)),
            entry(&"x".repeat(33), json!(5)),
            json!({ "group": "daily", "percent": 50, "scope": { "model": { "display_name": "Haiku" } } }),
            json!({ "group": "weekly", "percent": 50, "scope": { "surface": "web" } }),
        ];
        entries.extend((0..30).map(|n| entry(&format!("Model {n}"), json!(n))));
        let body = json!({
            "seven_day": { "utilization": 101.5 },
            "seven_day_opus": { "utilization": 99 },
            "limits": entries,
        });
        let limits = sent(&body.to_string()).unwrap();
        let limits = limits.as_array().unwrap();
        assert_eq!(limits.len(), MAX_LIMITS);
        assert_eq!(
            limits[0],
            json!({ "window": "seven_day", "used_pct": 100.0 })
        );
        // The first of two for one model stays, a percentage over 100 is sent
        // as 100 and one under 0 as 0, and the older field does not repeat a
        // model `limits` named.
        assert_eq!(
            limits[1],
            json!({ "window": "seven_day", "scope": "Fable", "used_pct": 100.0 })
        );
        assert_eq!(
            limits[2],
            json!({ "window": "seven_day", "scope": "Opus", "used_pct": 0.0 })
        );
        assert_eq!(limits[3]["scope"], "Model 0");
        assert!(limits.iter().all(|limit| limit["scope"] != "Sonnet"));
    }

    #[test]
    fn times_are_read_to_the_millisecond_in_any_offset() {
        let ms = |text: &str| parse_time(text).map(unix_ms);
        assert_eq!(ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(ms("2026-11-24T08:00:00.000Z"), Some(1_795_507_200_000));
        assert_eq!(
            ms("2026-11-24T09:00:00.0009+01:00"),
            Some(1_795_507_200_000)
        );
        assert_eq!(ms("2026-11-24T03:00:00.5-05:00"), Some(1_795_507_200_500));
        assert!(ms("2028-02-29T00:00:00Z").is_some());
        for bad in [
            "2026-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-11-24T24:00:00Z",
            "2026-11-24T08:00:00",
            "2026-11-24T08:00:00.Z",
            "2026-11-24T08:00:00+0100",
            "1969-12-31T23:59:59Z",
            "2026-11-24",
            "tomorrow",
        ] {
            assert_eq!(ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_token_is_read_only_while_it_has_more_than_a_minute_left() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            access_token(dir.path(), now()).map(|_| ()),
            Err(Failure::NoLogin)
        );
        fs::write(dir.path().join(".credentials.json"), "{\"token\":1}").unwrap();
        assert_eq!(
            access_token(dir.path(), now()).map(|_| ()),
            Err(Failure::NoLogin)
        );
        let now_ms = unix_ms(now());
        credentials(dir.path(), now_ms + 60_000);
        assert_eq!(
            access_token(dir.path(), now()).map(|_| ()),
            Err(Failure::Expired)
        );
        credentials(dir.path(), now_ms + 3_600_000);
        let token = access_token(dir.path(), now()).unwrap();
        assert_eq!(token.expose(), TOKEN);
        assert!(!format!("{token:?}").contains("sk-ant"));
        // Nothing was written into the config folder.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [".credentials.json"]);
    }

    #[test]
    fn a_failed_read_keeps_the_last_good_reading() {
        let config = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let environment = claude_env(Some(config.path()));
        credentials(config.path(), unix_ms(now()) + 3_600_000);

        // Never read: nothing to say.
        assert_eq!(
            report(read_readings(state.path()).as_ref(), &environment, now()),
            None
        );
        let mut asked = Vec::new();
        read_one(
            state.path(),
            &environment,
            |token| {
                asked.push(token.expose().to_string());
                Ok(SAMPLE.to_string())
            },
            now(),
        )
        .unwrap();
        assert_eq!(asked, [TOKEN]);
        let kept = read_readings(state.path()).unwrap();
        let first = report(Some(&kept), &environment, now()).unwrap();
        assert_eq!(first.read_at, "2026-11-24T08:00:00.000Z");
        assert_eq!(first.limits.len(), 2);
        assert_eq!(paths::mode(&readings_file(state.path())), Some(0o600));
        // The state file holds no token.
        let text = fs::read_to_string(readings_file(state.path())).unwrap();
        assert!(!text.contains("sk-ant"), "{text}");

        // Later reads fail, each its own way, and the first reading stands
        // with the time it was read.
        let later = now() + EVERY;
        for (fetch, failure) in [
            (Err(Failure::Status(401)), Failure::Status(401)),
            (
                Err(Failure::Transport("timeout")),
                Failure::Transport("timeout"),
            ),
            (Ok("{\"shape\":\"new\"}".to_string()), Failure::Unreadable),
        ] {
            assert_eq!(
                read_one(state.path(), &environment, |_| fetch, later),
                Err(failure)
            );
        }
        credentials(config.path(), unix_ms(later));
        assert_eq!(
            read_one(state.path(), &environment, |_| unreachable!(), later),
            Err(Failure::Expired)
        );
        let kept = read_readings(state.path()).unwrap();
        assert_eq!(report(Some(&kept), &environment, later), Some(first));

        // An environment whose usage is not read says nothing, whatever was kept.
        let codex = Environment {
            agent: "codex".into(),
            ..environment.clone()
        };
        assert_eq!(report(Some(&kept), &codex, later), None);
        assert!(!reads(&claude_env(None)));
    }

    #[test]
    fn a_failure_says_a_fixed_word_or_a_status_and_nothing_more() {
        let words: Vec<String> = [
            Failure::NoLogin,
            Failure::Expired,
            Failure::Status(429),
            Failure::Transport("unreachable"),
            Failure::Unreadable,
            Failure::NotKept,
        ]
        .iter()
        .map(Failure::to_string)
        .collect();
        assert_eq!(
            words,
            [
                "no_login",
                "login_expired",
                "status 429",
                "unreachable",
                "unreadable_answer",
                "not_written"
            ]
        );
    }

    #[test]
    fn the_endpoint_is_asked_with_the_token_and_the_beta_header_only() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/oauth/usage", server.server_addr());
        let token = Token::parse(TOKEN).unwrap();
        let client = client();
        let answering = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in [(200, SAMPLE), (401, "{\"error\":\"the body\"}"), (302, "")] {
                let request = server.recv().unwrap();
                let header = |name: &'static str| {
                    request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv(name))
                        .map(|h| h.value.to_string())
                };
                seen.push((
                    request.method().to_string(),
                    header("Authorization"),
                    header("anthropic-beta"),
                    header("User-Agent"),
                ));
                let mut response = tiny_http::Response::from_string(body).with_status_code(status);
                if status == 302 {
                    response.add_header(
                        tiny_http::Header::from_bytes("Location", "http://127.0.0.1:9/elsewhere")
                            .unwrap(),
                    );
                }
                request.respond(response).unwrap();
            }
            seen
        });
        assert_eq!(fetch(&client, &url, &token).as_deref(), Ok(SAMPLE));
        // A status other than 200 is the status, never the body; a redirect
        // is not followed.
        assert_eq!(fetch(&client, &url, &token), Err(Failure::Status(401)));
        assert_eq!(fetch(&client, &url, &token), Err(Failure::Status(302)));
        let seen = answering.join().unwrap();
        let version = concat!("nucleated/", env!("CARGO_PKG_VERSION"));
        for (method, authorization, beta, agent) in seen {
            assert_eq!(method, "GET");
            assert_eq!(authorization, Some(format!("Bearer {TOKEN}")));
            assert_eq!(beta.as_deref(), Some("oauth-2025-04-20"));
            assert_eq!(agent.as_deref(), Some(version));
        }
        // No server: one fixed word.
        let closed = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let gone = format!("http://{}/api/oauth/usage", closed.server_addr());
        drop(closed);
        assert_eq!(
            fetch(&client, &gone, &token),
            Err(Failure::Transport("unreachable"))
        );
    }

    #[test]
    fn readings_for_environments_no_longer_read_are_forgotten() {
        let state = tempfile::tempdir().unwrap();
        let reading = Reading {
            read_at_ms: unix_ms(now()),
            limits: Vec::new(),
        };
        write_readings(
            state.path(),
            &Readings {
                environments: BTreeMap::from([
                    ("client-a".to_string(), reading.clone()),
                    ("gone".to_string(), reading),
                ]),
            },
        )
        .unwrap();
        forget_others(state.path(), &BTreeSet::from(["client-a"]));
        let kept = read_readings(state.path()).unwrap();
        assert_eq!(kept.environments.keys().collect::<Vec<_>>(), ["client-a"]);
    }

    #[test]
    fn first_reads_are_spread_over_the_interval() {
        assert_eq!(spread(0, 3), Duration::ZERO);
        assert_eq!(spread(1, 3), EVERY / 3);
        assert_eq!(spread(2, 4), EVERY / 2);
        assert_eq!(spread(0, 0), Duration::ZERO);
    }
}
