//! `nucleated run`: the heartbeat and the forwarder.
//!
//! One loop, ticking twice a second. Each tick it notices a changed config,
//! token or maintenance pause, sends a heartbeat when one is due, applies new
//! spool files to the local session state, and sends what is worth sending.
//! Once a minute it also ends the sessions whose agent process has gone
//! without a `SessionEnd`.
//! Housekeeping and the reporter run beside it in threads of their own, so a
//! long fetch or a slow `git status` never delays a heartbeat.
//!
//! Nothing here can make a session wait: the hooks only write files, and this
//! loop only reads them. And nothing here acts on an agent: the daemon
//! reports and tidies, and starts, stops or pauses nothing.

use crate::credentials::{self, Stored, Token};
use crate::envelope::{rfc3339, unix_ms, Draft, Envelope, EventType};
use crate::health;
use crate::notifier::{Notifier, Waiting};
use crate::process::{self, Identity};
use crate::reporter::{self, Counts};
use crate::sessions::{self, Status, Store};
use crate::settings::{self, Environment, Settings, Target};
use crate::spool::{Record, Spool};
use crate::watch::{
    ApiError, Client, EnvironmentReport, Heartbeat, MachineHealth, Taken, UnsavedReport,
};
use crate::{agent, config_sync, driver, housekeeping, paths, pause, setup, usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The numbers the loop runs on. Tests shorten them; the daemon uses the defaults.
#[derive(Debug, Clone)]
pub struct Timings {
    pub tick: Duration,
    /// Routine events wait this long for company before they are sent.
    pub batch_window: Duration,
    /// ...or until this many are waiting.
    pub batch_ready: usize,
    /// The most events in one request.
    pub batch_max: usize,
    /// A failed send is retried after this long, doubling to `backoff_max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Spread retries by up to a fifth, so boxes do not retry in step.
    pub jitter: bool,
    pub http_timeout: Duration,
    /// After Watch refuses a heartbeat carrying a field it had said it
    /// takes, no such field is sent for this long.
    pub ahead_hold: Duration,
    /// How often the daemon looks for sessions whose agent process has gone.
    /// A session is ended once two looks in a row have not found it.
    pub process_check: Duration,
}

impl Default for Timings {
    fn default() -> Timings {
        Timings {
            tick: Duration::from_millis(500),
            batch_window: Duration::from_secs(10),
            batch_ready: 50,
            batch_max: 100,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
            jitter: true,
            http_timeout: Duration::from_secs(15),
            ahead_hold: Duration::from_secs(3_600),
            process_check: Duration::from_secs(60),
        }
    }
}

struct Pending {
    envelope: Envelope,
    at_ms: u64,
    seen: Instant,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatState {
    pub last_ok_at: Option<String>,
    pub last_attempt_at: Option<String>,
    /// A short word such as `unreachable`. Never text from the network.
    pub last_error: Option<String>,
    pub interval_s: u64,
    /// The fields the box knows and did not send, because this Watch has not
    /// said it takes them: names from [`crate::watch::AHEAD`].
    #[serde(default)]
    pub withheld: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventsState {
    pub pending: usize,
    pub rejected: usize,
    pub sent: u64,
    pub last_sent_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentState {
    pub name: String,
    pub server_alive: bool,
    pub checked_at: String,
}

/// What the daemon last knew, written to `daemon.json` for `nucleated status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub pid: u32,
    pub version: String,
    pub started_at: String,
    pub updated_at: String,
    pub running: bool,
    /// `hosted` or `none`.
    pub target: String,
    pub api_url: Option<String>,
    /// `ok`, `missing`, `insecure`, `unreadable` or `rejected`.
    pub token: String,
    pub heartbeat: HeartbeatState,
    pub events: EventsState,
    pub environments: Vec<EnvironmentState>,
    pub machine: Option<MachineHealth>,
}

pub fn state_file(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("daemon.json")
}

pub fn read_state(state_dir: &Path) -> Option<State> {
    serde_json::from_str(&fs::read_to_string(state_file(state_dir)).ok()?).ok()
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

fn log(line: &str) {
    eprintln!("nucleated: {line}");
}

/// A wait before a retry, spread by up to a fifth so boxes do not retry in step.
pub(crate) fn jittered(wait: Duration, jitter: bool) -> Duration {
    if !jitter {
        return wait;
    }
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    wait + wait.mul_f64(f64::from(byte[0]) / 255.0 * 0.2)
}

fn at_ms(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// What a heartbeat says about one environment: whether its agent server is
/// alive and how often its supervisor has restarted it, what the reporter
/// last counted in its nucleus, whatever its agent's adapter can read about
/// the login and the version, the driver it is watched through, and where
/// its config stands. Where a thing is not known, an adapter that cannot
/// read it or a count not yet made, the heartbeat says nothing.
///
/// Nothing here asks a driver or runs git: what the driver thread and the
/// config sync last wrote down is read from two small files.
fn environment_report(
    settings: &Settings,
    environment: &Environment,
    counts: Option<&Counts>,
    synced: Option<&config_sync::Status>,
    now: SystemTime,
) -> EnvironmentReport {
    // An environment watched through a driver is not one login, so there is
    // no one version and no one login expiry to report for it.
    let adapter = match environment.driver {
        Some(_) => None,
        None => agent::by_name(&environment.agent),
    };
    let nucleus = counts.and_then(|counts| counts.nucleus(&environment.name));
    let counted_at = counts.map(|counts| at_ms(counts.counted_at_ms));
    let unsaved = nucleus
        .and_then(|nucleus| nucleus.unsaved)
        .zip(counted_at)
        .and_then(|(counted, at)| {
            UnsavedReport::new(counted.uncommitted_repos, counted.unpushed_commits, at, now)
        });
    let driver_ref = environment
        .driver
        .as_ref()
        .and_then(|_| driver::read_memory(&settings.state_dir, &environment.name))
        .filter(|memory| Some(&memory.driver) == environment.driver.as_ref().map(|d| &d.name))
        .and_then(|memory| memory.link_ref);
    EnvironmentReport::new(
        &environment.name,
        health::server_alive(environment),
        adapter.and_then(|adapter| adapter.version(environment)),
        adapter.and_then(|adapter| adapter.login_expires_at(environment.config_dir.as_deref())),
        now,
    )
    .with_restarts(health::server_restarts(environment))
    .with_unsaved(unsaved)
    .with_snapshot(
        nucleus
            .and_then(|nucleus| nucleus.last_snapshot_ms)
            .map(at_ms),
        now,
    )
    .with_driver(
        environment
            .driver
            .as_ref()
            .map(|driver| driver.name.as_str()),
        driver_ref,
    )
    .with_config(&config_sync::report(synced, &environment.name, now))
}

/// The whole box's unsaved work, each nucleus counted once, and the time
/// since which every nucleus has had a good snapshot. Each is nothing unless
/// it is known for every environment: a total that leaves a nucleus out
/// would read as less than there is.
fn machine_totals(settings: &Settings, counts: &Counts) -> (Option<(u64, u64)>, Option<u64>) {
    let nuclei: Option<Vec<_>> = settings
        .environments
        .iter()
        .map(|environment| counts.nucleus(&environment.name))
        .collect();
    let Some(mut nuclei) = nuclei.filter(|nuclei| !nuclei.is_empty()) else {
        return (None, None);
    };
    // Environments that share a nucleus found the same entry.
    nuclei.sort_by_key(|nucleus| &nucleus.environments);
    nuclei.dedup_by_key(|nucleus| &nucleus.environments);
    let unsaved = nuclei.iter().map(|nucleus| nucleus.unsaved).try_fold(
        (0u64, 0u64),
        |(uncommitted, unpushed), counted| {
            counted.map(|counted| {
                (
                    uncommitted + counted.uncommitted_repos,
                    unpushed + counted.unpushed_commits,
                )
            })
        },
    );
    // A driver's worktrees are never snapshotted, so they neither have a
    // time nor hold back the box's.
    let snapshot = Some(&nuclei)
        .filter(|nuclei| nuclei.iter().any(|nucleus| !nucleus.driver))
        .and_then(|nuclei| {
            nuclei
                .iter()
                .filter(|nucleus| !nucleus.driver)
                .map(|nucleus| nucleus.last_snapshot_ms)
                .try_fold(u64::MAX, |oldest, at| at.map(|at| oldest.min(at)))
        });
    (unsaved, snapshot)
}

/// Everything a heartbeat says, sampled now. Each probe only asks, with a
/// time limit, and one that gets no answer leaves its field out. The
/// unsaved counts are read from the file the reporter writes: a heartbeat
/// never runs git.
///
/// `nucleated check` and `nucleated status` build theirs here too. A heartbeat
/// is the whole truth about the box, so a thinner one would tell Watch that
/// what it left out is no longer so.
pub fn heartbeat(settings: &Settings, now: SystemTime) -> Heartbeat {
    sample(settings, now).0
}

/// The heartbeat, and why anything about an environment's setup was left
/// out of it, by environment: for the daemon's log and `nucleated status`.
pub fn sample(settings: &Settings, now: SystemTime) -> (Heartbeat, Vec<(String, setup::Problem)>) {
    let counts = reporter::read_counts(settings);
    let (unsaved, snapshot) = counts
        .as_ref()
        .map_or((None, None), |counts| machine_totals(settings, counts));
    let machine = health::sample(&settings.environments, settings.ups.as_deref())
        .with_unsaved(unsaved)
        .with_snapshot(snapshot.map(at_ms), now);
    let synced = config_sync::read_status(settings);
    let usage = usage::read_readings(&settings.state_dir);
    let mut problems = Vec::new();
    let environments = settings
        .environments
        .iter()
        .map(|environment| {
            // Two small files the box recipe writes, read afresh each time.
            let setup = setup::report(&settings.setup_dir, &environment.name, now);
            problems.extend(
                setup
                    .problems
                    .iter()
                    .map(|problem| (environment.name.clone(), *problem)),
            );
            environment_report(settings, environment, counts.as_ref(), synced.as_ref(), now)
                .with_setup(setup.report)
                .with_usage(usage::report(usage.as_ref(), environment, now))
        })
        .collect();
    let beat = Heartbeat::new(machine, environments, now)
        .with_pause(pause::until(&settings.state_dir, now), now);
    (beat, problems)
}

pub struct Daemon {
    settings: Settings,
    settings_stamp: Option<SystemTime>,
    timings: Timings,
    spool: Spool,
    sessions: Store,

    token: Option<Token>,
    token_word: &'static str,
    token_stamp: Option<SystemTime>,
    token_checked: bool,
    api_url: Option<String>,
    client: Option<Client>,
    /// Watch answered 401. Nothing is sent until the token changes or a
    /// heartbeat is accepted again.
    token_rejected: bool,

    next_heartbeat: Option<Instant>,
    heartbeat_failures: u32,
    server_interval: Option<Duration>,
    /// The environments an accepted heartbeat has named. Watch refuses an
    /// event for any other.
    named: BTreeSet<String>,
    /// The fields ahead of the contract that Watch, in its last answer to a
    /// heartbeat, said it takes. None until it says so, and none again when
    /// the token changes: nothing Watch would refuse is sent on a guess.
    taken: Taken,
    /// Watch refused a heartbeat that carried such a field. Until this time
    /// none is sent, whatever Watch says it takes.
    ahead_held_until: Option<Instant>,
    heartbeat: HeartbeatState,
    environments: Vec<EnvironmentState>,
    machine: Option<MachineHealth>,
    /// What the last heartbeat could not say about an environment's setup,
    /// and why. Each is logged when it first appears, and not at every beat.
    setup_problems: BTreeSet<(String, setup::Problem)>,

    pending: BTreeMap<String, Pending>,
    backoff: Duration,
    retry_at: Option<Instant>,
    events: EventsState,

    started_at: String,
    written: Option<String>,
    last_prune: Option<Instant>,

    /// The pause file as last seen. Pausing or resuming changes it, and the
    /// next heartbeat then goes at once.
    pause_stamp: Option<SystemTime>,
    notifier: Notifier,
    /// The name Watch knows this box by, for what the box posts itself.
    machine_name: Option<String>,

    /// Where agent processes are looked for; nothing where they cannot be.
    proc_root: Option<PathBuf>,
    last_process_check: Option<Instant>,
    /// The sessions whose agent process the last look did not find, with
    /// the process it looked for.
    unseen: BTreeMap<(String, String), Identity>,
}

impl Daemon {
    pub fn new(settings: Settings, timings: Timings) -> Daemon {
        Daemon {
            settings_stamp: modified(&settings.path),
            spool: Spool::new(&settings.spool_dir),
            sessions: Store::new(&settings.state_dir),
            pause_stamp: modified(&pause::file(&settings.state_dir)),
            notifier: Notifier::new(&settings.state_dir),
            machine_name: None,
            proc_root: process::proc_root(),
            last_process_check: None,
            unseen: BTreeMap::new(),
            heartbeat: HeartbeatState {
                interval_s: settings.heartbeat_interval.as_secs(),
                ..HeartbeatState::default()
            },
            backoff: timings.backoff_min,
            settings,
            timings,
            token: None,
            token_word: "missing",
            token_stamp: None,
            token_checked: false,
            api_url: None,
            client: None,
            token_rejected: false,
            next_heartbeat: None,
            heartbeat_failures: 0,
            server_interval: None,
            named: BTreeSet::new(),
            taken: Taken::default(),
            ahead_held_until: None,
            environments: Vec::new(),
            machine: None,
            setup_problems: BTreeSet::new(),
            pending: BTreeMap::new(),
            retry_at: None,
            events: EventsState::default(),
            started_at: rfc3339(SystemTime::now()),
            written: None,
            last_prune: None,
        }
    }

    /// Look for agent processes in `proc` rather than the system's `/proc`.
    pub fn with_proc(mut self, proc: &Path) -> Daemon {
        self.proc_root = Some(proc.to_path_buf());
        self
    }

    fn hosted(&self) -> bool {
        self.settings.target(self.token.is_some()) == Target::Hosted
    }

    /// Pick up a changed config file. A file that no longer parses is ignored
    /// until it does: a typo must not stop a running daemon.
    fn reload_settings(&mut self) {
        let stamp = modified(&self.settings.path);
        if stamp == self.settings_stamp {
            return;
        }
        self.settings_stamp = stamp;
        match settings::load_from(&self.settings.path) {
            Ok(settings) => {
                log("config changed; reloaded");
                self.spool = Spool::new(&settings.spool_dir);
                self.sessions = Store::new(&settings.state_dir);
                if settings.state_dir != self.settings.state_dir {
                    self.pause_stamp = modified(&pause::file(&settings.state_dir));
                    self.notifier = Notifier::new(&settings.state_dir);
                }
                self.settings = settings;
                self.token_checked = false;
                self.next_heartbeat = None;
            }
            Err(error) => log(&format!("config ignored until it is fixed: {error:#}")),
        }
    }

    /// Pick up a new, removed or changed token: logging in needs no restart.
    fn reload_token(&mut self) {
        let stamp = modified(&self.settings.token_file);
        if self.token_checked && stamp == self.token_stamp {
            return;
        }
        self.token_checked = true;
        self.token_stamp = stamp;
        let (token, word, login) = match credentials::read(&self.settings.token_file) {
            Ok(Stored::Present(credential)) => (Some(credential.token), "ok", credential.login),
            Ok(Stored::Missing) => (None, "missing", None),
            Ok(Stored::Insecure { mode }) => {
                log(&format!(
                    "{} is readable by others (mode {mode:o}); not using it",
                    self.settings.token_file.display()
                ));
                (None, "insecure", None)
            }
            Err(_) => (None, "unreadable", None),
        };
        self.machine_name = login.as_ref().and_then(|login| login.machine_name.clone());
        let api_url = token.as_ref().map(|_| {
            login
                .map(|login| login.api_url)
                .unwrap_or_else(|| self.settings.login_api_url(None))
        });
        self.client = api_url
            .as_deref()
            .and_then(|url| Client::new(url, self.timings.http_timeout).ok());
        if token.is_some() && self.client.is_none() {
            log("the stored API address is not usable; not sending");
        }
        if token != self.token {
            log(match &token {
                Some(_) => "signed in; reporting to Watch",
                None => "not signed in; sending nothing",
            });
        }
        self.token = token.filter(|_| self.client.is_some());
        self.token_word = word;
        self.api_url = api_url;
        self.token_rejected = false;
        self.named.clear();
        self.taken = Taken::default();
        self.ahead_held_until = None;
        self.next_heartbeat = None;
        self.retry_at = None;
        self.backoff = self.timings.backoff_min;
    }

    fn interval(&self) -> Duration {
        self.settings
            .heartbeat_interval
            .max(self.server_interval.unwrap_or_default())
    }

    /// A pause set or ended on the box is told to Watch at once, not at the
    /// next beat: the person who typed it is about to pull a plug.
    fn notice_pause(&mut self) {
        let stamp = modified(&pause::file(&self.settings.state_dir));
        if stamp != self.pause_stamp {
            self.pause_stamp = stamp;
            self.next_heartbeat = None;
        }
    }

    /// Log what keeps an environment's setup out of the heartbeat, once: when
    /// it first appears, and again only if it goes away and comes back. The
    /// line is fixed words and the environment's name: no path, and nothing
    /// from the file.
    fn note_setup(&mut self, problems: Vec<(String, setup::Problem)>) {
        let problems: BTreeSet<_> = problems.into_iter().collect();
        for (environment, problem) in problems.difference(&self.setup_problems) {
            log(&format!("environment {environment}: {}", problem.says()));
        }
        self.setup_problems = problems;
    }

    /// The fields ahead of the contract that may go in a heartbeat now: what
    /// Watch last said it takes, and nothing while a refusal is being sat out.
    fn taken_now(&self, now: Instant) -> Taken {
        match self.ahead_held_until {
            Some(until) if now < until => Taken::default(),
            _ => self.taken.clone(),
        }
    }

    /// Sample the machine and its agent servers and, when signed in, tell Watch.
    ///
    /// What is sampled is everything the box knows. What is sent is that,
    /// less any field ahead of the contract that Watch has not said it
    /// takes: Watch refuses a heartbeat for one field it does not name, and
    /// a box must never look dark for having more to say.
    fn beat(&mut self, now: Instant) {
        let wall = SystemTime::now();
        let stamp = rfc3339(wall);
        let (known, problems) = sample(&self.settings, wall);
        self.note_setup(problems);
        self.environments = known
            .environments
            .iter()
            .map(|report| EnvironmentState {
                name: report.env_id.clone(),
                server_alive: report.server_alive,
                checked_at: stamp.clone(),
            })
            .collect();
        self.machine = Some(known.machine.clone());
        self.heartbeat.withheld.clear();
        let mut wait = self.interval();
        if let (true, Some(client), Some(token)) = (self.hosted(), &self.client, &self.token) {
            self.heartbeat.last_attempt_at = Some(stamp.clone());
            let mut body = known.only(&self.taken_now(now));
            let mut answer = client.heartbeat(token, &body);
            if matches!(answer, Err(ApiError::Refused { .. })) && !body.ahead().is_empty() {
                // Watch said it takes these and then refused them. Say at
                // once what every Watch takes, and leave the rest for later.
                log("Watch refused a heartbeat with a newer field in it; sending without them");
                self.ahead_held_until = Some(now + self.timings.ahead_hold);
                body = known.only(&Taken::default());
                answer = client.heartbeat(token, &body);
            }
            let sent = body.ahead();
            self.heartbeat.withheld = known
                .ahead()
                .difference(&sent)
                .map(|field| field.to_string())
                .collect();
            match answer {
                Ok(ack) => {
                    if self.heartbeat.last_error.is_some() || self.heartbeat.last_ok_at.is_none() {
                        log("heartbeat accepted");
                    }
                    self.server_interval = ack.heartbeat_interval_s.map(Duration::from_secs);
                    self.heartbeat.last_ok_at = Some(stamp);
                    self.heartbeat.last_error = None;
                    self.heartbeat_failures = 0;
                    self.named = body.environments.into_iter().map(|e| e.env_id).collect();
                    // Watch is reachable and takes the token: send what is waiting.
                    self.token_rejected = false;
                    self.retry_at = None;
                    self.backoff = self.timings.backoff_min;
                    if self.settings.notify.is_some() {
                        self.notifier.heartbeat_accepted(now, wall);
                    }
                    wait = self.interval();
                    // Watch has just said it takes more than it was sent:
                    // say the rest now, not a whole interval from now.
                    self.taken = Taken::from_names(&ack.accepts);
                    let next = known.only(&self.taken_now(now)).ahead();
                    if next.difference(&sent).next().is_some() {
                        wait = Duration::ZERO;
                    }
                }
                Err(failure) => {
                    if self.heartbeat.last_error.as_deref() != Some(failure.kind()) {
                        log(&format!("heartbeat not accepted: {failure}"));
                    }
                    self.heartbeat.last_error = Some(failure.kind().to_string());
                    self.token_rejected |= failure == ApiError::Unauthorized;
                    // A refused token and a request to slow down are both
                    // answers: Watch is there. Anything else is not hearing
                    // from it.
                    let answered = matches!(
                        failure,
                        ApiError::Unauthorized | ApiError::RateLimited { .. }
                    );
                    if self.settings.notify.is_some() {
                        self.notifier.heartbeat_failed(!answered, now, wall);
                    }
                    // Try again sooner than the next beat, but ever less often.
                    let sooner =
                        Duration::from_secs(5).saturating_mul(1 << self.heartbeat_failures.min(10));
                    self.heartbeat_failures += 1;
                    wait = match failure {
                        ApiError::RateLimited { retry_after } => {
                            retry_after.unwrap_or(wait).max(Duration::from_secs(5))
                        }
                        ApiError::Unauthorized => wait,
                        _ => wait.min(sooner),
                    };
                }
            }
        }
        self.heartbeat.interval_s = self.interval().as_secs();
        self.next_heartbeat = Some(now + wait);
    }

    /// Apply spool files not seen before to the local session state, and keep
    /// the ones worth sending.
    fn take_in(&mut self, now: Instant) {
        let hosted = self.hosted();
        for id in self.spool.pending() {
            if self.pending.contains_key(&id) {
                continue;
            }
            let Some(record) = self.spool.read(&id) else {
                // Not a record this program wrote.
                self.spool.reject(&id);
                continue;
            };
            let previous = self
                .sessions
                .load(&record.event.env_id, &record.event.session_id);
            let Some(applied) = sessions::apply(previous, &record) else {
                // An outcome for a session the box does not know.
                self.spool.remove(&id);
                continue;
            };
            if applied.changed {
                let _ = self.sessions.save(&applied.session);
                // A session in a nucleus has stopped or ended: ask the
                // reporter what it left behind. The reporter runs the git;
                // this loop only leaves the note.
                let finished = matches!(
                    record.event.kind,
                    EventType::Stop | EventType::StopFailure | EventType::SessionEnd
                );
                if finished && applied.session.workspace.is_some() {
                    reporter::ask(
                        &self.settings.state_dir,
                        &applied.session.env,
                        &applied.session.session_id,
                        SystemTime::now(),
                    );
                }
            }
            if applied.forward && hosted {
                self.pending.insert(
                    id,
                    Pending {
                        envelope: record.event,
                        at_ms: record.at_ms,
                        seen: now,
                    },
                );
            } else {
                // With no target, or nothing new to tell Watch, the event has
                // done its work once the local state has it.
                self.spool.remove(&id);
            }
        }
        if !hosted && !self.pending.is_empty() {
            for id in std::mem::take(&mut self.pending).into_keys() {
                self.spool.remove(&id);
            }
        }
    }

    /// End the sessions whose agent process has gone without a
    /// `SessionEnd`: killed, or stopped with its container. A session is
    /// ended once two looks in a row, `process_check` apart, have not found
    /// its process. The end is written to the spool as a hook writes one, so
    /// it is applied and sent like any other. A session whose hooks recorded
    /// no process is left alone. Returns true when an end was written.
    fn end_vanished(&mut self, now: Instant) -> bool {
        let Some(proc_root) = &self.proc_root else {
            return false;
        };
        if self
            .last_process_check
            .is_some_and(|last| now.saturating_duration_since(last) < self.timings.process_check)
        {
            return false;
        }
        self.last_process_check = Some(now);
        let live: Vec<(sessions::Session, Identity)> = self
            .settings
            .environments
            .iter()
            .flat_map(|environment| self.sessions.list(&environment.name))
            .filter(|session| session.status != Status::Ended)
            .filter_map(|session| session.process.map(|process| (session, process)))
            .collect();
        let wanted = live.iter().map(|(_, process)| *process).collect();
        // A look that cannot tell is no look: it neither ends a session nor
        // counts towards ending one.
        let Some(found) = process::running(proc_root, &wanted) else {
            return false;
        };
        let spool = self.spool.clone();
        let mut unseen = BTreeMap::new();
        let mut wrote = false;
        for (session, process) in live {
            if found.contains(&process) {
                continue;
            }
            let key = (session.env.clone(), session.session_id.clone());
            if self.unseen.get(&key) != Some(&process) {
                unseen.insert(key, process);
                continue;
            }
            // A spool nobody is emptying is not allowed to fill the disk.
            // The next look tries again.
            if spool.holds_at_least(self.settings.spool_max_events.saturating_mul(2)) {
                unseen.insert(key, process);
                continue;
            }
            // After the session's last event, whatever the clock says, so it
            // is never taken for one older than that.
            let at = SystemTime::now().max(at_ms(session.last_event_ms + 1));
            let draft = Draft {
                env_id: session.env.clone(),
                session_id: session.session_id.clone(),
                bridge_session_id: session.bridge_session_id.clone(),
                repos: session.repos.clone(),
                branch: session.branch.clone(),
                ..Draft::default()
            };
            let Some(event) = Envelope::new(EventType::SessionEnd, draft, at) else {
                continue;
            };
            let record = Record {
                event,
                at_ms: unix_ms(at),
                workspace: session.workspace.clone(),
                process: None,
            };
            if spool.write(&record).is_ok() {
                log(&format!(
                    "environment {}: a session's agent process has gone; ended it",
                    session.env
                ));
                wrote = true;
            } else {
                unseen.insert(key, process);
            }
        }
        self.unseen = unseen;
        wrote
    }

    fn drop_event(&mut self, id: &str) {
        self.pending.remove(id);
        self.spool.remove(id);
    }

    /// Keep the spool within its limits: nothing older than `max_age`, and no
    /// more than `max_events`, dropping routine events before "needs input".
    fn enforce_limits(&mut self) {
        let now_ms = unix_ms(SystemTime::now());
        let max_age_ms = self.settings.spool_max_age.as_millis() as u64;
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.at_ms) > max_age_ms)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.drop_event(id);
        }
        let over = self
            .pending
            .len()
            .saturating_sub(self.settings.spool_max_events);
        if over > 0 {
            let routine = self
                .pending
                .iter()
                .filter(|(_, p)| !p.envelope.kind.needs_input());
            let urgent = self
                .pending
                .iter()
                .filter(|(_, p)| p.envelope.kind.needs_input());
            let doomed: Vec<String> = routine
                .chain(urgent)
                .take(over)
                .map(|(id, _)| id.clone())
                .collect();
            log(&format!(
                "spool over its limit; dropping the {} oldest events",
                doomed.len()
            ));
            for id in &doomed {
                self.drop_event(id);
            }
        }
    }

    fn next_backoff(&mut self) -> Duration {
        let wait = self.backoff;
        self.backoff = (self.backoff * 2).min(self.timings.backoff_max);
        jittered(wait, self.timings.jitter)
    }

    /// The sessions waiting on their owner that Watch has not heard about:
    /// those with a "needs input" event still unsent, and still waiting.
    fn unheard_waits(&self) -> Vec<Waiting> {
        let mut seen = BTreeSet::new();
        self.pending
            .values()
            .map(|pending| &pending.envelope)
            .filter(|event| event.kind.needs_input())
            .filter(|event| seen.insert((&event.env_id, &event.session_id)))
            .filter_map(|event| {
                let session = self.sessions.load(&event.env_id, &event.session_id)?;
                (session.status == Status::NeedsInput).then_some(Waiting {
                    env: session.env,
                    session_id: session.session_id,
                    since_ms: session.needs_since_ms?,
                })
            })
            .collect()
    }

    /// Let the box speak for itself while Watch cannot be reached. Off
    /// unless the config gives it an address, and silent on a box that does
    /// not report to Watch: such a box has no Watch to lose.
    fn notify(&mut self, now: Instant) {
        let hosted = self.hosted();
        let target = self.settings.notify.as_ref().filter(|_| hosted);
        // No outage and nothing owed: every tick but a few, and it costs nothing.
        if target.is_some() && self.notifier.is_idle() {
            return;
        }
        let waiting = match target.is_some() && self.notifier.tells_waits() {
            true => self.unheard_waits(),
            false => Vec::new(),
        };
        let machine = target
            .and(self.machine_name.clone())
            .or_else(|| target.and_then(|_| settings::machine_name(&self.settings)))
            .unwrap_or_else(|| "This box".to_string());
        self.notifier.tick(
            target,
            &machine,
            &waiting,
            &self.timings,
            now,
            SystemTime::now(),
        );
    }

    /// Send one batch. `Ok` means every event in it is dealt with, delivered
    /// or set aside; `Err` means stop and try again later.
    fn send(&mut self, ids: &[String], now: Instant) -> Result<(), ()> {
        let (Some(client), Some(token)) = (&self.client, &self.token) else {
            return Err(());
        };
        let envelopes: Vec<&Envelope> = ids
            .iter()
            .filter_map(|id| self.pending.get(id).map(|p| &p.envelope))
            .collect();
        match client.events(token, &envelopes) {
            Ok(ack) => {
                let refused: BTreeSet<&str> =
                    ack.rejected.iter().map(|r| r.event_id.as_str()).collect();
                for id in ids {
                    if refused.contains(id.as_str()) {
                        self.spool.reject(id);
                    } else {
                        self.spool.remove(id);
                    }
                    self.pending.remove(id);
                }
                if self.events.last_error.is_some() {
                    log("events are being accepted again");
                }
                self.events.sent += (ids.len() - refused.len()) as u64;
                self.events.last_sent_at = Some(rfc3339(SystemTime::now()));
                self.events.last_error = None;
                self.backoff = self.timings.backoff_min;
                Ok(())
            }
            // Watch will not take this request however often it is sent. Find
            // the event it objects to by halving, and set that one aside.
            Err(ApiError::Refused { .. }) => {
                if let [only] = ids {
                    log("Watch refused an event; set aside in spool/rejected");
                    self.spool.reject(only);
                    self.pending.remove(only);
                    return Ok(());
                }
                let (first, second) = ids.split_at(ids.len() / 2);
                self.send(first, now)?;
                self.send(second, now)
            }
            Err(failure) => {
                if self.events.last_error.as_deref() != Some(failure.kind()) {
                    log(&format!("events not sent, will retry: {failure}"));
                }
                self.events.last_error = Some(failure.kind().to_string());
                match failure {
                    ApiError::Unauthorized => self.token_rejected = true,
                    ApiError::RateLimited { retry_after } => {
                        let wait = retry_after.unwrap_or_else(|| self.next_backoff());
                        self.retry_at = Some(now + wait);
                    }
                    _ => self.retry_at = Some(now + self.next_backoff()),
                }
                Err(())
            }
        }
    }

    /// Send what is waiting, if it is time: at once when anything "needs
    /// input", otherwise when a batch has gathered.
    fn flush(&mut self, now: Instant) {
        if !self.hosted() || self.token_rejected || self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        loop {
            let sendable: Vec<(&String, &Pending)> = self
                .pending
                .iter()
                .filter(|(_, p)| self.named.contains(&p.envelope.env_id))
                .collect();
            let ready = sendable.iter().any(|(_, p)| p.envelope.kind.needs_input())
                || sendable.len() >= self.timings.batch_ready
                || sendable.iter().any(|(_, p)| {
                    now.saturating_duration_since(p.seen) >= self.timings.batch_window
                });
            if !ready {
                return;
            }
            let batch: Vec<String> = sendable
                .into_iter()
                .take(self.timings.batch_max)
                .map(|(id, _)| id.clone())
                .collect();
            if self.send(&batch, now).is_err() {
                return;
            }
        }
    }

    fn prune(&mut self, now: Instant) {
        if self
            .last_prune
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(600))
        {
            return;
        }
        self.last_prune = Some(now);
        let wall = SystemTime::now();
        self.spool.prune(self.settings.spool_max_age, wall);
        let max_age_ms = self.settings.spool_max_age.as_millis() as u64;
        for env in &self.settings.environments {
            self.sessions.prune(&env.name, max_age_ms, unix_ms(wall));
        }
    }

    /// One turn of the loop.
    pub fn tick(&mut self, now: Instant) {
        self.reload_settings();
        self.reload_token();
        self.notice_pause();
        if self.next_heartbeat.is_none_or(|due| now >= due) {
            self.beat(now);
        }
        self.take_in(now);
        // After the spool is taken in, so an end a hook wrote just before its
        // agent went is the one applied.
        if self.end_vanished(now) {
            self.take_in(now);
        }
        self.enforce_limits();
        self.flush(now);
        self.notify(now);
        self.prune(now);
    }

    pub fn state(&self, running: bool) -> State {
        State {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: self.started_at.clone(),
            updated_at: rfc3339(SystemTime::now()),
            running,
            target: if self.hosted() { "hosted" } else { "none" }.to_string(),
            api_url: self.api_url.clone(),
            token: if self.token_rejected {
                "rejected"
            } else {
                self.token_word
            }
            .to_string(),
            heartbeat: self.heartbeat.clone(),
            events: EventsState {
                pending: self.pending.len(),
                rejected: self.spool.rejected().len(),
                ..self.events.clone()
            },
            environments: self.environments.clone(),
            machine: self.machine.clone(),
        }
    }

    /// Write `daemon.json` when anything but its timestamp has changed, and
    /// at least once a minute so a reader can tell the daemon is alive.
    pub fn write_state(&mut self, running: bool) {
        let state = self.state(running);
        let mut comparable = state.clone();
        comparable.updated_at = state.updated_at.chars().take(16).collect();
        let key = serde_json::to_string(&comparable).unwrap_or_default();
        if self.written.as_deref() == Some(&key) {
            return;
        }
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            if paths::write_private(&state_file(&self.settings.state_dir), text.as_bytes()).is_ok()
            {
                self.written = Some(key);
            }
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }
}

/// `nucleated run`. Returns when `stop` is set.
pub fn run(stop: Arc<AtomicBool>) -> Result<()> {
    let settings = settings::load()?;
    paths::create_private_dir(&settings.state_dir)?;
    log(&format!(
        "started, version {}, {} environment(s)",
        env!("CARGO_PKG_VERSION"),
        settings.environments.len()
    ));
    let keeper = {
        let stop = stop.clone();
        let path = settings.path.clone();
        std::thread::spawn(move || housekeeping::run_schedule(&path, &stop))
    };
    let reporter = {
        let stop = stop.clone();
        let path = settings.path.clone();
        std::thread::spawn(move || reporter::run_schedule(&path, &stop))
    };
    // Drivers are asked, and the owner's config repository is fetched, on
    // threads of their own too: neither a driver that does not answer nor a
    // git host that does not is ever waited on by a heartbeat.
    let drivers = {
        let stop = stop.clone();
        let path = settings.path.clone();
        std::thread::spawn(move || driver::run_schedule(&path, &stop))
    };
    let config = {
        let stop = stop.clone();
        let path = settings.path.clone();
        std::thread::spawn(move || config_sync::run_schedule(&path, &stop))
    };
    // Plan usage is asked of the agent vendor on a thread of its own as well.
    let plan = {
        let stop = stop.clone();
        let path = settings.path.clone();
        std::thread::spawn(move || usage::run_schedule(&path, &stop))
    };
    let timings = Timings::default();
    let tick = timings.tick;
    let mut daemon = Daemon::new(settings, timings);
    while !stop.load(Ordering::SeqCst) {
        daemon.tick(Instant::now());
        daemon.write_state(true);
        std::thread::sleep(tick);
    }
    daemon.write_state(false);
    log("stopped");
    // Housekeeping may be in the middle of a fetch, and the reporter of a
    // count; each stops at its next boundary, and nothing either does is
    // left half-written.
    drop(keeper);
    drop(reporter);
    drop(drivers);
    drop(config);
    drop(plan);
    Ok(())
}
