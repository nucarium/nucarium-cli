//! `nucleated status` and `nucleated check`: what Watch would show, read from
//! the box itself, for a box with no account or no network.

use crate::config_sync;
use crate::credentials::{self, Stored};
use crate::daemon::{self, Timings};
use crate::driver;
use crate::envelope::{rfc3339, unix_ms};
use crate::housekeeping;
use crate::notifier;
use crate::reporter;
use crate::sessions::{Session, Status, Store};
use crate::settings::{self, Settings, Target};
use crate::watch::{Client, Heartbeat, Taken};
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
    // What a heartbeat would say now. Built where the daemon builds its own,
    // so what is shown here is what Watch is told.
    let (beat, setup_problems) = daemon::sample(settings, now);

    let synced = config_sync::read_status(settings);
    // The repos the last count found the daemon would not run git in.
    let last_count = reporter::read_counts(settings);

    let mut inbox: Vec<(u8, u64, Value)> = Vec::new();
    let mut environments = Vec::new();
    for (environment, report) in settings.environments.iter().zip(&beat.environments) {
        let alive = report.server_alive;
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
                    "limit_resets_at": session.limit_resets_at,
                    "repos": session.repos,
                    "branch": session.branch,
                    "workspace": session.workspace,
                    "started_at": at(session.started_ms),
                    "last_event_at": at(session.last_event_ms),
                    "outcome": session.outcome.as_ref().map(|outcome| json!({
                        "reported_at": at(outcome.reported_ms),
                        "commits": outcome.commits,
                        "uncommitted_repos": outcome.uncommitted_repos,
                        "unpushed_commits": outcome.unpushed_commits,
                        "pull_requests": outcome.pull_requests,
                    })),
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
        // What the driver thread last found, and where the config sync
        // stands: fixed words, ids and times, read from the box's own files.
        let watched = environment.driver.as_ref().map(|configured| {
            let memory = driver::read_memory(&settings.state_dir, &environment.name)
                .filter(|memory| memory.driver == configured.name);
            json!({
                "name": configured.name,
                "poll_s": configured.poll.map(|every| every.as_secs()),
                "condition": memory.as_ref().map(|memory| memory.condition.as_str()),
                "since": memory.as_ref().map(|memory| at(memory.since_ms)),
                "looked_at": memory.as_ref().map(|memory| at(memory.looked_at_ms)),
                "last_good_at": memory.as_ref().and_then(|memory| memory.good_at_ms).map(at),
                "uncounted_worktrees": memory.as_ref().map(|memory| memory.uncounted_worktrees),
                "link_ref": report.driver_ref,
                "link_template": environment
                    .driver_adapter()
                    .and_then(|adapter| adapter.link_template()),
            })
        });
        let refused = last_count
            .as_ref()
            .and_then(|counted| counted.nucleus(&environment.name))
            .map(|nucleus| nucleus.refused.clone())
            .unwrap_or_default();
        let config = config_sync::report(synced.as_ref(), &environment.name, now);
        let config = (config != config_sync::Report::default()).then(|| {
            json!({
                "commit": config.commit,
                "modified": config.modified,
                "state": config.state.map(config_sync::State::as_str),
                "detail": config.detail,
            })
        });
        // What the box recipe set the environment up with, as a heartbeat
        // carries it, and fixed words for what was left out and why.
        let problems: Vec<&str> = setup_problems
            .iter()
            .filter(|(name, _)| *name == environment.name)
            .map(|(_, problem)| problem.as_str())
            .collect();
        environments.push(json!({
            "name": environment.name,
            "root": environment.root,
            "agent": environment.agent,
            "driver": watched,
            "config": config,
            "setup": report.setup,
            "setup_problems": problems,
            "usage": report.usage,
            "agent_version": report.agent_version,
            "login_expires_at": report.login_expires_at,
            "server_alive": alive,
            "server_restarts": report.server_restarts,
            "unsaved": report.unsaved,
            "last_snapshot_at": report.last_snapshot_at,
            "sessions": counts,
            "git_refused": refused,
        }));
    }
    inbox.sort_by_key(|item| (item.0, item.1));

    let machine = &beat.machine;
    let told = notifier::read_state(&settings.state_dir).unwrap_or_default();
    json!({
        "generated_at": rfc3339(now),
        "machine": {
            "name": login.as_ref().and_then(|l| l.machine_name.clone()).or_else(|| settings::machine_name(settings)),
            "disk_used_pct": machine.disk_used_pct,
            "load_1m": machine.load_1m,
            "uptime_s": machine.uptime_s,
            "memory_pressure_pct": machine.memory_pressure_pct,
            "on_battery": machine.on_battery,
            "reboot_required": machine.reboot_required,
            "uncommitted_repos": machine.uncommitted_repos,
            "unpushed_commits": machine.unpushed_commits,
            "last_snapshot_at": machine.last_snapshot_at,
            "paused_until": beat.paused_until,
        },
        "notifier": {
            "on": settings.notify.is_some(),
            "after_s": settings.notify.as_ref().map(|notify| notify.after.as_secs()),
            "watch_unreachable_since": told.down_since_ms.map(at),
            "said_so": told.announced,
            "sessions_told": told.told.len(),
        },
        "watch": {
            "target": if hosted { "hosted" } else { "none" },
            "signed_in": signed_in,
            "token": token,
            "api_url": login.as_ref().map(|l| l.api_url.clone()),
            "account_name": login.as_ref().and_then(|l| l.account_name.clone()),
            "last_heartbeat_at": state.as_ref().and_then(|s| s.heartbeat.last_ok_at.clone()),
            "heartbeat_error": state.as_ref().and_then(|s| s.heartbeat.last_error.clone()),
            // What the box knows and this Watch has not said it takes.
            "withheld": state.as_ref().map(|s| s.heartbeat.withheld.clone()).unwrap_or_default(),
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

/// A timestamp this program wrote, as a person reads one: `2026-11-20 03:10 UTC`.
fn clock(stamp: &str) -> String {
    match (stamp.get(0..10), stamp.get(11..16)) {
        (Some(date), Some(time)) => format!("{date} {time} UTC"),
        _ => stamp.to_string(),
    }
}

/// `3 commits`, `1 pull request told`: a count and its noun, and a word after.
fn counted(n: u64, noun: &str, after: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{plural} {after}").trim_end().to_string()
}

/// How much is unsaved, in words, or nothing when both counts are zero or
/// not known.
fn unsaved_words(uncommitted: &Value, unpushed: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(n) = uncommitted.as_u64().filter(|n| *n > 0) {
        parts.push(counted(n, "repo", "with uncommitted changes"));
    }
    if let Some(n) = unpushed.as_u64().filter(|n| *n > 0) {
        parts.push(counted(n, "commit", "not pushed"));
    }
    parts.join(" · ")
}

/// What a session left behind, as its last outcome reported it.
fn left_behind(outcome: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(n) = outcome["commits"].as_u64() {
        parts.push(counted(n, "commit", ""));
    }
    if let Some(pulls) = outcome["pull_requests"]
        .as_array()
        .filter(|p| !p.is_empty())
    {
        parts.push(counted(pulls.len() as u64, "pull request", ""));
    }
    let unsaved = unsaved_words(&outcome["uncommitted_repos"], &outcome["unpushed_commits"]);
    if !unsaved.is_empty() {
        parts.push(unsaved);
    }
    parts.join(" · ")
}

/// How an environment's driver was last found: its name, and, when the last
/// look found nothing, the fixed word for why and since when.
fn driver_words(driver: &Value) -> String {
    let Some(name) = driver["name"].as_str() else {
        return String::new();
    };
    let found = match (driver["poll_s"].is_null(), driver["condition"].as_str()) {
        (true, _) => ": not asked, driver_poll is off".to_string(),
        (false, None) => ": not asked yet".to_string(),
        (false, Some("ok")) => String::new(),
        (false, Some(condition)) => match driver["since"].as_str() {
            Some(stamp) => format!(": {condition} since {}", clock(stamp)),
            None => format!(": {condition}"),
        },
    };
    // Why no unsaved count is shown for it, when that is why.
    let left_out = match driver["uncounted_worktrees"].as_u64() {
        None | Some(0) => String::new(),
        Some(1) => " · 1 worktree outside its folders, unsaved work not counted".to_string(),
        Some(count) => {
            format!(" · {count} worktrees outside its folders, unsaved work not counted")
        }
    };
    format!("  driver {name}{found}{left_out}")
}

/// Where an environment's config stands: the short commit, whether a synced
/// file was changed by hand, and the sync's state when it is not "ok".
fn config_words(config: &Value) -> String {
    if !config.is_object() {
        return String::new();
    }
    let mut words = match config["commit"].as_str() {
        Some(commit) => format!("  config {}", commit.chars().take(8).collect::<String>()),
        None => "  config not applied yet".to_string(),
    };
    if config["modified"] == true {
        words.push_str(", changed by hand");
    }
    match (config["state"].as_str(), config["detail"].as_str()) {
        (None | Some("ok"), _) => {}
        (Some(state), Some(detail)) => words.push_str(&format!(" ({state}: {detail})")),
        (Some(state), None) => words.push_str(&format!(" ({state})")),
    }
    words
}

/// What the box recipe set an environment up with: its network, its counts,
/// and how its setup script last ran; and, when something in its files was
/// left out, the fixed words for why. Nothing when the recipe wrote nothing.
fn setup_words(setup: &Value, problems: &Value) -> String {
    let problems: Vec<&str> = problems
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|problem| *problem != "missing")
        .collect();
    if !setup.is_object() {
        return match problems.is_empty() {
            true => String::new(),
            false => format!("  setup not reported ({})", problems.join(", ")),
        };
    }
    let count = |key: &str| setup[key].as_u64().unwrap_or(0);
    let network = match setup["network"].as_str() {
        Some("limited") => format!(
            "network limited, {}",
            counted(count("allowed_hosts"), "more host", "")
        ),
        Some(other) => format!("network {other}"),
        None => String::new(),
    };
    let mut words = format!(
        "  setup {network}, {}, {}, {}",
        counted(
            setup["repos"].as_array().map_or(0, |r| r.len() as u64),
            "repo",
            ""
        ),
        counted(count("variables"), "variable", ""),
        counted(count("secrets"), "secret", ""),
    );
    let script = &setup["script"];
    let at = |key: &str| script[key].as_str().map(clock).unwrap_or_default();
    match script["state"].as_str() {
        Some("running") => words.push_str(&format!(", script running since {}", at("started_at"))),
        Some("ok") => words.push_str(&format!(", script ran ok at {}", at("finished_at"))),
        Some("failed") => match script["exit_code"].as_u64() {
            Some(code) => words.push_str(&format!(
                ", script failed with exit {code} at {}",
                at("finished_at")
            )),
            None => words.push_str(&format!(", script failed at {}", at("finished_at"))),
        },
        _ => {}
    }
    if !problems.is_empty() {
        words.push_str(&format!(" ({})", problems.join(", ")));
    }
    words
}

/// The repos the daemon would not run git in, by name and why.
fn refused_words(refused: &Value) -> String {
    let named: Vec<String> = refused
        .as_array()
        .into_iter()
        .flatten()
        .map(|repo| {
            format!(
                "{} ({})",
                repo["repo"].as_str().unwrap_or_default(),
                repo["reason"].as_str().unwrap_or_default()
            )
        })
        .collect();
    if named.is_empty() {
        return String::new();
    }
    format!("  git refused: {}", named.join(", "))
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
    if machine["on_battery"] == true {
        line.push_str(" · on battery");
    }
    if machine["reboot_required"] == true {
        line.push_str(" · reboot required");
    }
    out.push_str(&line);
    out.push('\n');
    if let Some(until) = machine["paused_until"].as_str() {
        out.push_str(&format!(
            "Paused for maintenance until {}. Watch holds its alerts for this box going dark. \
             nucleated resume ends it.\n",
            clock(until)
        ));
    }

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
            let withheld: Vec<&str> = watch["withheld"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if !withheld.is_empty() {
                line.push_str(&format!(
                    " · not sent, Watch does not take it yet: {}",
                    withheld.join(", ")
                ));
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
    let notifier = &view["notifier"];
    if notifier["on"] == true {
        let mut line = "Notifier: on".to_string();
        if let Some(since) = notifier["watch_unreachable_since"].as_str() {
            line.push_str(&format!(" · Watch unreachable since {}", clock(since)));
            line.push_str(match notifier["said_so"].as_bool() {
                Some(true) => " · said so",
                _ => " · not said yet",
            });
            let told = notifier["sessions_told"].as_u64().unwrap_or(0);
            if told > 0 {
                line.push_str(&format!(" · {}", counted(told, "waiting session", "told")));
            }
        }
        out.push_str(&line);
        out.push('\n');
    }

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
            let resets = item["limit_resets_at"]
                .as_str()
                .map(|stamp| format!("limit resets {}", clock(stamp)))
                .unwrap_or_default();
            let mut line = format!("  {}  {id}", text(&item["environment"]));
            for part in [
                detail.to_string(),
                resets,
                waited,
                repos.join(", "),
                text(&item["branch"]),
                left_behind(&item["outcome"]),
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
            let mut line = format!(
                "  {}  agent server {}  {}",
                text(&environment["name"]),
                if environment["server_alive"] == true {
                    "alive"
                } else {
                    "not running"
                },
                text(&environment["root"]),
            );
            if let Some(version) = environment["agent_version"].as_str() {
                line.push_str(&format!("  agent {version}"));
            }
            if let Some(stamp) = environment["login_expires_at"].as_str() {
                line.push_str(&format!("  login expires {}", clock(stamp)));
            }
            if let Some(restarts) = environment["server_restarts"].as_u64() {
                line.push_str(&format!("  {}", counted(restarts, "restart", "")));
            }
            let unsaved = &environment["unsaved"];
            if unsaved.is_object() {
                let held =
                    unsaved_words(&unsaved["uncommitted_repos"], &unsaved["unpushed_commits"]);
                line.push_str(&format!(
                    "  {}",
                    if held.is_empty() {
                        "nothing unsaved".to_string()
                    } else {
                        held
                    }
                ));
                if let Some(seconds) = unsaved["counted_at"]
                    .as_str()
                    .and_then(|stamp| since(stamp, now_ms))
                {
                    line.push_str(&format!(" (counted {} ago)", ago(seconds)));
                }
            }
            if let Some(seconds) = environment["last_snapshot_at"]
                .as_str()
                .and_then(|stamp| since(stamp, now_ms))
            {
                line.push_str(&format!("  last snapshot {} ago", ago(seconds)));
            }
            line.push_str(&driver_words(&environment["driver"]));
            line.push_str(&config_words(&environment["config"]));
            line.push_str(&setup_words(
                &environment["setup"],
                &environment["setup_problems"],
            ));
            line.push_str(&refused_words(&environment["git_refused"]));
            out.push_str(&line);
            out.push('\n');
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
    // The same heartbeat the daemon sends: a thinner one would tell Watch
    // that what it left out is no longer so.
    let beat = daemon::heartbeat(settings, SystemTime::now());
    for (environment, report) in settings.environments.iter().zip(&beat.environments) {
        if environment.driver.is_some() {
            // A driver keeps its agents where it likes. The folder has to
            // be there; it does not have to be a nucleus.
            let folder = environment.root.is_dir();
            add(
                format!("{}: folder", environment.name),
                folder,
                match folder {
                    true => environment.root.display().to_string(),
                    false => format!("{} is not a folder", environment.root.display()),
                },
            );
        } else {
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
        }
        let alive = report.server_alive;
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
    }
    if let Some(ups) = settings.ups.as_deref() {
        add(
            "ups".to_string(),
            beat.machine.on_battery.is_some(),
            match beat.machine.on_battery {
                Some(true) => format!("{ups}: on battery"),
                Some(false) => format!("{ups}: on mains"),
                None => format!("upsc gave no status for {ups}; is NUT running?"),
            },
        );
    }
    if let Some(notify) = &settings.notify {
        let usable = notifier::usable(notify);
        add(
            "notifier".to_string(),
            usable.is_ok(),
            match usable {
                Ok(()) => format!(
                    "on: posts when Watch has been unreachable for {}",
                    ago(notify.after.as_secs())
                ),
                Err(why) => why.to_string(),
            },
        );
    }
    if let Some(credential) = credential.filter(|_| settings.target(true) == Target::Hosted) {
        let api_url = credential
            .login
            .map(|login| login.api_url)
            .unwrap_or_else(|| settings.login_api_url(None));
        // Sent as the daemon sends it: first what every Watch takes, and
        // then, in full, whatever more this Watch answers that it takes.
        let sent = Client::new(&api_url, Timings::default().http_timeout)
            .map_err(|error| format!("{error:#}"))
            .and_then(|client| {
                let send = |body: &Heartbeat| {
                    client
                        .heartbeat(&credential.token, body)
                        .map_err(|failure| failure.to_string())
                };
                let plain = beat.only(&Taken::default());
                let taken = Taken::from_names(&send(&plain)?.accepts);
                let full = beat.only(&taken);
                if full != plain {
                    send(&full)?;
                }
                Ok(beat
                    .ahead()
                    .difference(&full.ahead())
                    .copied()
                    .collect::<Vec<_>>())
            });
        add(
            "heartbeat".to_string(),
            sent.is_ok(),
            match sent {
                Ok(withheld) if withheld.is_empty() => format!("accepted by {api_url}"),
                Ok(withheld) => format!(
                    "accepted by {api_url}; it does not take {} yet, so that stays on this box",
                    withheld.join(", ")
                ),
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

    #[test]
    fn a_failure_on_a_usage_limit_shows_when_the_limit_resets() {
        let view = json!({
            "now_ms": 1_795_089_603_120u64,
            "machine": { "name": "agentbox", "disk_used_pct": 41.5, "load_1m": 0.6, "uptime_s": 60 },
            "watch": { "target": "none" },
            "daemon": { "running": true },
            "inbox": [{
                "kind": "session",
                "attention": "failed",
                "since": at(1_795_089_600_000),
                "environment": "client-a",
                "session_id": "7f3c2a1e-58b4",
                "status": "failed",
                "error_kind": "rate_limit",
                "limit_resets_at": "2026-11-20T03:10:00.000Z",
                "repos": ["your-org/web"],
                "branch": "agent/checkout-fix",
            }],
            "environments": [{
                "name": "client-a",
                "root": "/home/agent/client-a",
                "server_alive": true,
                "agent_version": "2.1.0",
                "login_expires_at": "2026-12-01T09:00:00.000Z",
            }],
        });
        let text = render(&view);
        assert!(
            text.contains(
                "client-a  7f3c2a1e  rate_limit  limit resets 2026-11-20 03:10 UTC  3s  your-org/web  agent/checkout-fix"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "client-a  agent server alive  /home/agent/client-a  agent 2.1.0  login expires 2026-12-01 09:00 UTC"
            ),
            "{text}"
        );
        // With neither known, the lines are as they were.
        assert!(!text.contains("Paused") && !text.contains("Notifier"));
        let mut bare = view.clone();
        bare["inbox"][0]["limit_resets_at"] = Value::Null;
        bare["environments"][0]["agent_version"] = Value::Null;
        bare["environments"][0]["login_expires_at"] = Value::Null;
        let text = render(&bare);
        assert!(text.contains("client-a  7f3c2a1e  rate_limit  3s  your-org/web"));
        assert!(text.contains("client-a  agent server alive  /home/agent/client-a\n"));
    }

    #[test]
    fn an_environments_setup_is_shown_and_so_is_its_being_held_back() {
        let view = |setup: Value, problems: Value| {
            json!({
                "now_ms": 1_795_089_603_120u64,
                "machine": { "name": "agentbox", "disk_used_pct": 41.5, "load_1m": 0.6, "uptime_s": 60 },
                "watch": {
                    "target": "hosted", "api_url": "https://api.nucarium.com",
                    "token": "ok", "withheld": ["setup"],
                },
                "daemon": { "running": true },
                "inbox": [],
                "environments": [{
                    "name": "client-a",
                    "root": "/home/agent/client-a",
                    "server_alive": true,
                    "setup": setup,
                    "setup_problems": problems,
                }],
            })
        };
        let setup = |script: Value| {
            json!({
                "network": "limited", "allowed_hosts": 3,
                "repos": ["your-org/web", "your-org/api"], "variables": 2, "secrets": 0,
                "script": script,
            })
        };
        let text = render(&view(
            setup(json!({
                "state": "failed", "started_at": "2026-11-22T10:00:00.000Z",
                "finished_at": "2026-11-22T10:00:42.000Z", "exit_code": 3,
            })),
            json!(["repo_left_out"]),
        ));
        assert!(
            text.contains(
                "/home/agent/client-a  setup network limited, 3 more hosts, 2 repos, 2 variables, \
                 0 secrets, script failed with exit 3 at 2026-11-22 10:00 UTC (repo_left_out)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("not sent, Watch does not take it yet: setup"),
            "{text}"
        );
        let running = render(&view(
            setup(json!({ "state": "running", "started_at": "2026-11-22T10:00:00.000Z" })),
            json!([]),
        ));
        assert!(
            running.contains("0 secrets, script running since 2026-11-22 10:00 UTC\n"),
            "{running}"
        );
        let ok = render(&view(
            setup(json!({
                "state": "ok", "started_at": "2026-11-22T10:00:00.000Z",
                "finished_at": "2026-11-22T10:00:42.000Z",
            })),
            json!([]),
        ));
        assert!(
            ok.contains("script ran ok at 2026-11-22 10:00 UTC\n"),
            "{ok}"
        );
        // A file that could not be used says why; a box with no recipe says
        // nothing about setup at all.
        let broken = render(&view(Value::Null, json!(["malformed"])));
        assert!(
            broken.contains("/home/agent/client-a  setup not reported (malformed)\n"),
            "{broken}"
        );
        let none = render(&view(Value::Null, json!(["missing"])));
        assert!(none.contains("/home/agent/client-a\n"), "{none}");
    }

    #[test]
    fn what_the_box_reports_about_itself_is_shown_in_words() {
        let now_ms = 1_795_089_603_120u64;
        let view = json!({
            "now_ms": now_ms,
            "machine": {
                "name": "agentbox", "disk_used_pct": 41.5, "load_1m": 0.6, "uptime_s": 60,
                "on_battery": true, "reboot_required": true,
                "paused_until": "2026-11-20T03:10:00.000Z",
            },
            "watch": { "target": "none" },
            "daemon": { "running": true },
            "notifier": {
                "on": true, "watch_unreachable_since": "2026-11-19T11:40:00.000Z",
                "said_so": true, "sessions_told": 1,
            },
            "inbox": [{
                "kind": "session",
                "attention": "idle",
                "since": at(now_ms - 3_000),
                "environment": "client-a",
                "session_id": "7f3c2a1e-58b4",
                "status": "idle",
                "repos": ["your-org/web"],
                "branch": "agent/checkout-fix",
                "outcome": {
                    "commits": 3, "uncommitted_repos": 1, "unpushed_commits": 0,
                    "pull_requests": ["https://github.com/your-org/web/pull/412"],
                },
            }],
            "environments": [{
                "name": "client-a",
                "root": "/home/agent/client-a",
                "server_alive": true,
                "server_restarts": 4,
                "unsaved": {
                    "uncommitted_repos": 2, "unpushed_commits": 1,
                    "counted_at": at(now_ms - 240_000),
                },
                "last_snapshot_at": at(now_ms - 2_400_000),
            }],
        });
        let text = render(&view);
        assert!(
            text.contains("up 1m · on battery · reboot required\n"),
            "{text}"
        );
        assert!(
            text.contains("Paused for maintenance until 2026-11-20 03:10 UTC."),
            "{text}"
        );
        assert!(
            text.contains(
                "Notifier: on · Watch unreachable since 2026-11-19 11:40 UTC · said so · 1 waiting session told\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "agent/checkout-fix  3 commits · 1 pull request · 1 repo with uncommitted changes\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "/home/agent/client-a  4 restarts  2 repos with uncommitted changes · 1 commit not pushed (counted 4m ago)  last snapshot 40m ago\n"
            ),
            "{text}"
        );
        // Counted, and nothing found.
        let mut clean = view.clone();
        clean["environments"][0]["unsaved"]["uncommitted_repos"] = json!(0);
        clean["environments"][0]["unsaved"]["unpushed_commits"] = json!(0);
        assert!(render(&clean).contains("4 restarts  nothing unsaved (counted 4m ago)"));
    }
}
