//! `nucleated status` and `nucleated check`: what Watch would show, read from
//! the box itself, for a box with no account or no network.

use crate::credentials::{self, Stored};
use crate::daemon::{self, Timings};
use crate::envelope::{rfc3339, unix_ms};
use crate::health;
use crate::housekeeping;
use crate::sessions::{Session, Status, Store};
use crate::settings::{self, Settings, Target};
use crate::watch::{Client, EnvironmentReport, Heartbeat};
use serde_json::{json, Value};
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn at(ms: u64) -> String {
    rfc3339(UNIX_EPOCH + Duration::from_millis(ms))
}

/// Why a session is on the list, most urgent first, as Watch orders its inbox.
fn attention(session: &Session) -> (&'static str, u8) {
    match session.status {
        Status::NeedsInput => ("needs_input", 0),
        Status::Failed => ("failed", 1),
        Status::Working => ("working", 3),
        Status::Idle => ("idle", 4),
        Status::Ended => ("ended", 5),
    }
}

/// Build the local view.
pub fn view(settings: &Settings) -> Value {
    let now = SystemTime::now();
    let now_ms = unix_ms(now);
    let store = Store::new(&settings.state_dir);
    let stored = credentials::read(&settings.token_file);
    let (signed_in, mut token, login) = match &stored {
        Ok(Stored::Present(credential)) => (true, "ok", credential.login.clone()),
        Ok(Stored::Missing) => (false, "missing", None),
        Ok(Stored::Insecure { .. }) => (false, "insecure", None),
        Err(_) => (false, "unreadable", None),
    };
    let state = daemon::read_state(&settings.state_dir);
    // The daemon rewrites its state at least once a minute while it runs.
    let fresh = fs::metadata(daemon::state_file(&settings.state_dir))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age < Duration::from_secs(150));
    let running = fresh && state.as_ref().is_some_and(|state| state.running);
    if signed_in
        && state
            .as_ref()
            .is_some_and(|state| state.token == "rejected")
    {
        token = "rejected";
    }
    let hosted = settings.target(signed_in) == Target::Hosted;

    let mut inbox: Vec<(u8, u64, Value)> = Vec::new();
    let mut environments = Vec::new();
    for environment in &settings.environments {
        let alive = health::server_alive(environment);
        let mut counts = serde_json::Map::new();
        let mut sessions = store.list(&environment.name);
        sessions.retain(|session| session.status != Status::Ended);
        for session in &sessions {
            let (why, rank) = attention(session);
            *counts.entry(why).or_insert(json!(0)) =
                json!(counts.get(why).and_then(Value::as_u64).unwrap_or(0) + 1);
            // Waiting sessions by how long they have waited; the rest by how
            // recently anything happened.
            let since = session.needs_since_ms.unwrap_or(session.last_event_ms);
            let order = match session.status {
                Status::NeedsInput => since,
                _ => u64::MAX - since,
            };
            inbox.push((
                rank,
                order,
                json!({
                    "kind": "session",
                    "attention": why,
                    "since": at(since),
                    "environment": environment.name,
                    "session_id": session.session_id,
                    "status": session.status.as_str(),
                    "needs_kind": session.needs_kind,
                    "error_kind": session.error_kind,
                    "repos": session.repos,
                    "branch": session.branch,
                    "workspace": session.workspace,
                    "started_at": at(session.started_ms),
                    "last_event_at": at(session.last_event_ms),
                }),
            ));
        }
        if !alive {
            inbox.push((
                2,
                0,
                json!({
                    "kind": "environment",
                    "attention": "down",
                    "since": Value::Null,
                    "environment": environment.name,
                }),
            ));
        }
        environments.push(json!({
            "name": environment.name,
            "root": environment.root,
            "agent": environment.agent,
            "server_alive": alive,
            "sessions": counts,
        }));
    }
    inbox.sort_by_key(|item| (item.0, item.1));

    let machine = health::sample(&settings.environments);
    json!({
        "generated_at": rfc3339(now),
        "machine": {
            "name": login.as_ref().and_then(|l| l.machine_name.clone()).or_else(|| settings::machine_name(settings)),
            "disk_used_pct": machine.disk_used_pct,
            "load_1m": machine.load_1m,
            "uptime_s": machine.uptime_s,
            "memory_pressure_pct": machine.memory_pressure_pct,
        },
        "watch": {
            "target": if hosted { "hosted" } else { "none" },
            "signed_in": signed_in,
            "token": token,
            "api_url": login.as_ref().map(|l| l.api_url.clone()),
            "account_name": login.as_ref().and_then(|l| l.account_name.clone()),
            "last_heartbeat_at": state.as_ref().and_then(|s| s.heartbeat.last_ok_at.clone()),
            "heartbeat_error": state.as_ref().and_then(|s| s.heartbeat.last_error.clone()),
            "pending_events": state.as_ref().map_or(0, |s| s.events.pending),
            "rejected_events": state.as_ref().map_or(0, |s| s.events.rejected),
            "events_error": state.as_ref().and_then(|s| s.events.last_error.clone()),
        },
        "daemon": {
            "running": running,
            "pid": state.as_ref().filter(|_| running).map(|s| s.pid),
            "version": state.as_ref().map(|s| s.version.clone()),
            "updated_at": state.as_ref().map(|s| s.updated_at.clone()),
        },
        "inbox": inbox.into_iter().map(|(_, _, item)| item).collect::<Vec<_>>(),
        "environments": environments,
        "housekeeping": housekeeping::read_ledger(&settings.state_dir),
        "now_ms": now_ms,
    })
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3_600),
    }
}

/// Seconds between an RFC 3339 timestamp this program wrote and `now_ms`.
fn since(stamp: &str, now_ms: u64) -> Option<u64> {
    let digits = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    // Civil date to days since the epoch (the inverse of `rfc3339`).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let then = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some((now_ms / 1000).saturating_sub(then.max(0) as u64))
}

/// The view as text, for a person at the box.
pub fn render(view: &Value) -> String {
    let now_ms = view["now_ms"].as_u64().unwrap_or(0);
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let mut out = String::new();

    let machine = &view["machine"];
    let mut line = format!(
        "{} · disk {}% · load {} · up {}",
        machine["name"].as_str().unwrap_or("this box"),
        machine["disk_used_pct"],
        machine["load_1m"],
        ago(machine["uptime_s"].as_u64().unwrap_or(0)),
    );
    if let Some(pressure) = machine["memory_pressure_pct"].as_f64() {
        line.push_str(&format!(" · memory pressure {pressure}%"));
    }
    out.push_str(&line);
    out.push('\n');

    let watch = &view["watch"];
    out.push_str(&match (watch["target"].as_str(), watch["token"].as_str()) {
        (_, Some("rejected")) => {
            "Watch: the token is no longer accepted. Run nucleate login.".to_string()
        }
        (_, Some("insecure")) => {
            "Watch: the token file is readable by others, so it is not used. Make it 600."
                .to_string()
        }
        (Some("hosted"), _) => {
            let mut line = format!("Watch: reporting to {}", text(&watch["api_url"]));
            if let Some(account) = watch["account_name"].as_str() {
                line.push_str(&format!(" for {account}"));
            }
            match watch["last_heartbeat_at"]
                .as_str()
                .and_then(|stamp| since(stamp, now_ms))
            {
                Some(seconds) => line.push_str(&format!(" · last heartbeat {} ago", ago(seconds))),
                None => line.push_str(" · no heartbeat accepted yet"),
            }
            if let Some(error) = watch["heartbeat_error"]
                .as_str()
                .or(watch["events_error"].as_str())
            {
                line.push_str(&format!(" · {error}"));
            }
            let pending = watch["pending_events"].as_u64().unwrap_or(0);
            if pending > 0 {
                line.push_str(&format!(" · {pending} events waiting to send"));
            }
            line
        }
        (_, Some("ok")) => {
            "Watch: signed in, but target is \"none\". Nothing leaves this box.".to_string()
        }
        _ => "Watch: not signed in. Nothing leaves this box.".to_string(),
    });
    out.push('\n');
    out.push_str(match view["daemon"]["running"].as_bool() {
        Some(true) => "Daemon: running\n",
        _ => "Daemon: not running. Sessions below are as the daemon last saw them.\n",
    });

    let headings = [
        ("needs_input", "Needs you"),
        ("failed", "Failed"),
        ("down", "Down"),
        ("working", "Working"),
        ("idle", "Idle"),
    ];
    let inbox = view["inbox"].as_array().cloned().unwrap_or_default();
    for (attention, heading) in headings {
        let items: Vec<&Value> = inbox
            .iter()
            .filter(|item| item["attention"] == attention)
            .collect();
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{heading}\n"));
        for item in items {
            if item["kind"] == "environment" {
                out.push_str(&format!(
                    "  {}  agent server is not running\n",
                    text(&item["environment"])
                ));
                continue;
            }
            let id: String = text(&item["session_id"]).chars().take(8).collect();
            let detail = item["needs_kind"]
                .as_str()
                .or(item["error_kind"].as_str())
                .unwrap_or_default();
            let waited = item["since"]
                .as_str()
                .and_then(|stamp| since(stamp, now_ms))
                .map(ago)
                .unwrap_or_default();
            let repos: Vec<String> = item["repos"]
                .as_array()
                .map(|repos| repos.iter().map(text).collect())
                .unwrap_or_default();
            let mut line = format!("  {}  {id}", text(&item["environment"]));
            for part in [
                detail.to_string(),
                waited,
                repos.join(", "),
                text(&item["branch"]),
            ] {
                if !part.is_empty() {
                    line.push_str(&format!("  {part}"));
                }
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    if inbox.is_empty() {
        out.push_str("\nNo sessions.\n");
    }

    let environments = view["environments"].as_array().cloned().unwrap_or_default();
    if environments.is_empty() {
        out.push_str("\nNo environments are configured. nucleated init writes a first config.\n");
    } else {
        out.push_str("\nEnvironments\n");
        for environment in environments {
            out.push_str(&format!(
                "  {}  agent server {}  {}\n",
                text(&environment["name"]),
                if environment["server_alive"] == true {
                    "alive"
                } else {
                    "not running"
                },
                text(&environment["root"]),
            ));
        }
    }
    if let Some(ledger) = view["housekeeping"]
        .as_object()
        .filter(|ledger| !ledger.is_empty())
    {
        out.push_str("\nHousekeeping\n");
        for (key, last) in ledger {
            let when = last["at_s"]
                .as_u64()
                .map(|at| ago((now_ms / 1000).saturating_sub(at)))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {key}  {when} ago  {}\n",
                text(&last["summary"])
            ));
        }
    }
    out
}

/// `nucleated check`: is this box set up to do what its config says?
pub fn check(settings: &Settings) -> (Value, bool) {
    let mut checks = Vec::new();
    let mut add = |name: String, ok: bool, detail: String| {
        checks.push(json!({ "name": name, "ok": ok, "detail": detail }));
    };
    add(
        "config".to_string(),
        true,
        match settings.exists {
            true => settings.path.display().to_string(),
            false => format!(
                "{} does not exist; running with no environments",
                settings.path.display()
            ),
        },
    );
    let stored = credentials::read(&settings.token_file);
    let explicit_hosted = settings.target == Some(Target::Hosted);
    let credential = match stored {
        Ok(Stored::Present(credential)) => {
            add(
                "token".to_string(),
                true,
                "present, readable only by its owner".to_string(),
            );
            Some(credential)
        }
        Ok(Stored::Missing) => {
            add(
                "token".to_string(),
                !explicit_hosted,
                "not signed in; nothing is sent. nucleate login signs this box in".to_string(),
            );
            None
        }
        Ok(Stored::Insecure { mode }) => {
            add(
                "token".to_string(),
                false,
                format!(
                    "{} has mode {mode:o}; it must be 600",
                    settings.token_file.display()
                ),
            );
            None
        }
        Err(error) => {
            add("token".to_string(), false, format!("{error:#}"));
            None
        }
    };
    let mut reports = Vec::new();
    for environment in &settings.environments {
        let nucleus = environment.root.join(".repos").is_dir();
        add(
            format!("{}: nucleus", environment.name),
            nucleus,
            match nucleus {
                true => environment.root.display().to_string(),
                false => format!(
                    "{} is not a nucleus; nucleate init --root creates one",
                    environment.root.display()
                ),
            },
        );
        let alive = health::server_alive(environment);
        let (known, how) = match &environment.check {
            Some(_) => (true, "check command".to_string()),
            None => (
                std::process::Command::new("systemctl")
                    .args(["--user", "cat", &environment.unit])
                    .output()
                    .is_ok_and(|out| out.status.success()),
                format!("unit {}", environment.unit),
            ),
        };
        add(
            format!("{}: agent server", environment.name),
            known,
            match (known, alive) {
                (false, _) => format!("{how} does not exist"),
                (true, true) => format!("{how}: alive"),
                (true, false) => format!("{how}: not running"),
            },
        );
        reports.push(EnvironmentReport {
            env_id: environment.name.clone(),
            server_alive: alive,
        });
    }
    if let Some(credential) = credential.filter(|_| settings.target(true) == Target::Hosted) {
        let api_url = credential
            .login
            .map(|login| login.api_url)
            .unwrap_or_else(|| settings.login_api_url(None));
        let sent = Client::new(&api_url, Timings::default().http_timeout)
            .map_err(|error| format!("{error:#}"))
            .and_then(|client| {
                let heartbeat = Heartbeat {
                    machine: health::sample(&settings.environments),
                    environments: reports,
                    daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                    client_ts: rfc3339(SystemTime::now()),
                };
                client
                    .heartbeat(&credential.token, &heartbeat)
                    .map_err(|failure| failure.to_string())
            });
        add(
            "heartbeat".to_string(),
            sent.is_ok(),
            match sent {
                Ok(_) => format!("accepted by {api_url}"),
                Err(why) => why,
            },
        );
    }
    let ok = checks.iter().all(|check| check["ok"] == true);
    (json!({ "ok": ok, "checks": checks }), ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_naturally() {
        assert_eq!(ago(12), "12s");
        assert_eq!(ago(720), "12m");
        assert_eq!(ago(3_720), "1h 2m");
        assert_eq!(ago(864_000 + 7_200), "10d 2h");
    }

    #[test]
    fn a_timestamp_reads_back_as_an_age() {
        let stamp = at(1_795_089_603_120);
        assert_eq!(since(&stamp, 1_795_089_603_120 + 90_000), Some(90));
        assert_eq!(
            since(&at(951_782_400_000), 951_782_400_000 + 86_400_000),
            Some(86_400)
        );
        assert_eq!(since("not a time", 0), None);
    }
}
