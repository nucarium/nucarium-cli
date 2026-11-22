//! The Watch API, as the box uses it: sign in, revoke, heartbeat, events.
//!
//! Everything the box sends goes through this file.

use crate::config_sync;
use crate::credentials::Token;
use crate::envelope::{
    rfc3339, valid_commit_id, valid_env_key, valid_repo, valid_session_id, valid_version, within,
    Envelope,
};
use crate::sessions::ACTIVITY_BEAT;
use crate::setup::{Network, ScriptState};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::time::{Duration, SystemTime};

pub const CLIENT_ID: &str = "nucleate";
pub const SCOPE: &str = "ingest";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MAX_BODY: u64 = 1024 * 1024;

/// Why a request did not succeed, sorted by what the caller should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No answer: no network, no route, a timeout, a TLS failure. Try again later.
    Unreachable(String),
    /// Watch does not accept the token. Trying again will not help.
    Unauthorized,
    /// Watch asked for a pause.
    RateLimited { retry_after: Option<Duration> },
    /// Watch understood the request and refused it. Do not send it again as it is.
    Refused {
        status: u16,
        code: Option<String>,
        detail: Option<String>,
    },
    /// Watch failed. Try again later.
    Server { status: u16 },
    /// An answer that is not the API's.
    Unexpected(String),
}

impl ApiError {
    /// True when the same request may succeed later.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ApiError::Unreachable(_)
                | ApiError::RateLimited { .. }
                | ApiError::Server { .. }
                | ApiError::Unexpected(_)
        )
    }

    /// A short word for logs and status, with nothing from the server in it.
    pub fn kind(&self) -> &'static str {
        match self {
            ApiError::Unreachable(_) => "unreachable",
            ApiError::Unauthorized => "unauthorized",
            ApiError::RateLimited { .. } => "rate_limited",
            ApiError::Refused { .. } => "refused",
            ApiError::Server { .. } => "server_error",
            ApiError::Unexpected(_) => "unexpected_answer",
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unreachable(why) => write!(f, "Watch could not be reached ({why})"),
            ApiError::Unauthorized => write!(f, "Watch does not accept this token"),
            ApiError::RateLimited { .. } => write!(f, "Watch asked this box to slow down"),
            ApiError::Refused {
                status,
                code,
                detail,
            } => {
                write!(f, "Watch refused the request ({status}")?;
                if let Some(code) = code {
                    write!(f, " {}", printable(code))?;
                }
                write!(f, ")")?;
                if let Some(detail) = detail {
                    write!(f, ": {}", printable(detail))?;
                }
                Ok(())
            }
            ApiError::Server { status } => write!(f, "Watch failed ({status})"),
            ApiError::Unexpected(what) => write!(f, "an unexpected answer from Watch ({what})"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Text from the network, made safe to print on a terminal.
pub fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(300).collect()
}

/// The scheme and the host of an address, checked: `https` anywhere, `http`
/// only for this machine, so nothing secret is sent in the clear across a
/// network, and no name and password inside the address.
fn check_origin(url: &str) -> Result<()> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("{url:?} is not an http(s) address"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        bail!("{url:?} is not a plain address");
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1");
    match scheme {
        "https" => Ok(()),
        "http" if local => Ok(()),
        "http" => bail!("{url:?} is not https; plain http is only for an address on this machine"),
        _ => bail!("{url:?} is not an http(s) address"),
    }
}

/// Check an API address and trim it. `https` anywhere; `http` only for this
/// machine, so a token is never sent in the clear across a network.
pub fn normalize_api_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.contains(['?', '#']) {
        bail!("{url:?} is not a plain API address");
    }
    check_origin(url)?;
    Ok(url.to_string())
}

/// Check the address of a webhook the box posts to. The same rule as for the
/// API, except that a webhook's address may carry a query.
pub fn normalize_webhook_url(url: &str) -> Result<String> {
    let url = url.trim();
    if url.contains('#') || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("{url:?} is not a plain address");
    }
    check_origin(url)?;
    Ok(url.to_string())
}

/// The answer to a device authorization request (RFC 8628 §3.2).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub interval: Option<u64>,
}

/// What signing in grants.
#[derive(Debug, Clone)]
pub struct Grant {
    pub token: Token,
    pub scope: Option<String>,
    pub machine_id: Option<String>,
    pub machine_name: Option<String>,
    pub account_name: Option<String>,
}

/// One answer from the token endpoint while waiting for approval.
#[derive(Debug, Clone)]
pub enum TokenPoll {
    Granted(Grant),
    Pending,
    SlowDown,
    Denied,
    Expired,
}

/// The most Watch takes in a heartbeat's counts.
pub const MAX_UNCOMMITTED_REPOS: u64 = 10_000;
pub const MAX_UNPUSHED_COMMITS: u64 = 1_000_000;
pub const MAX_SERVER_RESTARTS: u64 = 2_147_483_647;
/// The most Watch takes in an environment's setup.
pub const MAX_ALLOWED_HOSTS: u64 = 10_000;
pub const MAX_VARIABLES: u64 = 1_000;
pub const MAX_SECRETS: u64 = 1_000;
pub const MAX_SETUP_REPOS: usize = 32;
/// The longest maintenance pause Watch keeps.
pub const MAX_PAUSE: Duration = Duration::from_secs(86_400);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MachineHealth {
    pub disk_used_pct: f64,
    pub load_1m: f64,
    pub uptime_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_pressure_pct: Option<f64>,
    /// Whether the box runs from its UPS battery. Nothing on a box with no
    /// UPS, or when the UPS could not be asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot_required: Option<bool>,
    /// Unsaved work across the whole box, each nucleus counted once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncommitted_repos: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpushed_commits: Option<u64>,
    /// Every nucleus has had a good snapshot since this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_snapshot_at: Option<String>,
}

impl MachineHealth {
    /// Add the box's unsaved work: counts, and nothing when either is more
    /// than Watch takes.
    pub fn with_unsaved(mut self, counts: Option<(u64, u64)>) -> MachineHealth {
        let counts = counts.filter(|(uncommitted, unpushed)| {
            *uncommitted <= MAX_UNCOMMITTED_REPOS && *unpushed <= MAX_UNPUSHED_COMMITS
        });
        self.uncommitted_repos = counts.map(|(uncommitted, _)| uncommitted);
        self.unpushed_commits = counts.map(|(_, unpushed)| unpushed);
        self
    }

    pub fn with_snapshot(mut self, at: Option<SystemTime>, now: SystemTime) -> MachineHealth {
        self.last_snapshot_at = at.map(|at| rfc3339(at.min(now)));
        self
    }
}

/// A login expiry is sent only when it lies this far around now: Watch keeps
/// nothing outside the same window.
const LOGIN_EXPIRY_BEHIND: Duration = Duration::from_secs(30 * 86_400);
const LOGIN_EXPIRY_AHEAD: Duration = Duration::from_secs(400 * 86_400);

/// How much of an environment's work exists only on the box: two counts and
/// the time they were counted.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UnsavedReport {
    pub uncommitted_repos: u64,
    pub unpushed_commits: u64,
    pub counted_at: String,
}

impl UnsavedReport {
    /// Nothing when a count is more than Watch takes. A time ahead of now,
    /// from a clock that has since moved back, is sent as now.
    pub fn new(
        uncommitted_repos: u64,
        unpushed_commits: u64,
        counted_at: SystemTime,
        now: SystemTime,
    ) -> Option<UnsavedReport> {
        (uncommitted_repos <= MAX_UNCOMMITTED_REPOS && unpushed_commits <= MAX_UNPUSHED_COMMITS)
            .then(|| UnsavedReport {
                uncommitted_repos,
                unpushed_commits,
                counted_at: rfc3339(counted_at.min(now)),
            })
    }
}

/// What an environment is set up with on the box, as the box recipe wrote it
/// down: a fixed word for its network, three counts, its repositories'
/// names, and the setup script's last run. Never a variable's name or value,
/// the script, its hash or its output.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SetupReport {
    pub network: Network,
    /// With `limited`, how many hosts are allowed beyond the default list.
    pub allowed_hosts: u64,
    pub repos: Vec<String>,
    pub variables: u64,
    pub secrets: u64,
    /// Nothing when there is no script, or it has not run yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptReport>,
}

impl SetupReport {
    /// Nothing when a count is more than Watch takes, or there are more
    /// repositories than it takes. A repository's name that is not an
    /// `owner/name` or a `name` is left out, and only it.
    pub fn new(
        network: Network,
        allowed_hosts: u64,
        repos: &[String],
        variables: u64,
        secrets: u64,
        script: Option<ScriptReport>,
    ) -> Option<SetupReport> {
        (allowed_hosts <= MAX_ALLOWED_HOSTS
            && variables <= MAX_VARIABLES
            && secrets <= MAX_SECRETS
            && repos.len() <= MAX_SETUP_REPOS)
            .then(|| SetupReport {
                network,
                allowed_hosts,
                repos: repos
                    .iter()
                    .filter(|repo| valid_repo(repo))
                    .cloned()
                    .collect(),
                variables,
                secrets,
                script,
            })
    }
}

/// The setup script's last run: a fixed word, when it started, when it
/// ended, and for a failed run how it exited.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScriptReport {
    pub state: ScriptState,
    pub started_at: String,
    /// Nothing while it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Only for a failed run, and only when it is an exit status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u8>,
}

impl ScriptReport {
    /// A run as Watch takes one. A run that has ended needs its end, and one
    /// that ended before it started is no run: nothing is made up for
    /// either. A time ahead of now, from a clock that has since moved back,
    /// is sent as now.
    pub fn new(
        state: ScriptState,
        started_at: SystemTime,
        finished_at: Option<SystemTime>,
        exit_code: Option<i64>,
        now: SystemTime,
    ) -> Option<ScriptReport> {
        let started_at = started_at.min(now);
        let finished_at = match state {
            ScriptState::Running => None,
            ScriptState::Ok | ScriptState::Failed => Some(finished_at?.min(now)),
        };
        if finished_at.is_some_and(|finished| finished < started_at) {
            return None;
        }
        Some(ScriptReport {
            state,
            started_at: rfc3339(started_at),
            finished_at: finished_at.map(rfc3339),
            exit_code: exit_code
                .filter(|_| state == ScriptState::Failed)
                .and_then(|code| u8::try_from(code).ok()),
        })
    }
}

/// The fields of a heartbeat's environment that are ahead of the contract
/// every Watch holds. Watch refuses a heartbeat that carries a field it does
/// not name, and to its owner a refused heartbeat is a machine gone dark, so
/// none of these is sent until Watch has said it takes it: in `accepts`, in
/// its answer to a heartbeat. Until then each stays on the box, where
/// `nucleated status` shows it.
pub const AHEAD: [&str; 6] = [
    "driver",
    "driver_ref",
    "config_commit",
    "config_modified",
    "config_state",
    "setup",
];

/// Which of the [`AHEAD`] fields a Watch has said it takes. None, until it
/// says so.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taken(BTreeSet<&'static str>);

impl Taken {
    /// What Watch named. A name that is not one of [`AHEAD`] is a field this
    /// program does not have, and is passed over.
    pub fn from_names(names: &[String]) -> Taken {
        Taken(
            AHEAD
                .into_iter()
                .filter(|field| names.iter().any(|name| name == field))
                .collect(),
        )
    }

    pub fn takes(&self, field: &str) -> bool {
        self.0.contains(field)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EnvironmentReport {
    pub env_id: String,
    pub server_alive: bool,
    /// The supervisor's running count of restarts of the agent server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_restarts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_expires_at: Option<String>,
    /// When the snapshot job last finished for this environment's nucleus
    /// with nothing failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_snapshot_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsaved: Option<UnsavedReport>,
    /// The driver the environment is watched through: its config name, one
    /// of a fixed few. It tells Watch which link template opens a session
    /// there, and that its sessions come from a state source that may report
    /// no tool activity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// The driver's own id for this installation, for its link template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_ref: Option<String>,
    /// The commit the environment's config folder was last applied from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_commit: Option<String>,
    /// Whether a synced file has been changed by hand since that commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_modified: Option<bool>,
    /// How the sync stands: one of [`config_sync::State`]'s fixed words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_state: Option<String>,
    /// What the environment is set up with on the box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<SetupReport>,
}

impl EnvironmentReport {
    /// One environment's part of a heartbeat. A version that is not a short
    /// version string, or an expiry outside what Watch keeps, is left out.
    pub fn new(
        env_id: &str,
        server_alive: bool,
        agent_version: Option<String>,
        login_expires_at: Option<SystemTime>,
        now: SystemTime,
    ) -> EnvironmentReport {
        EnvironmentReport {
            env_id: env_id.to_string(),
            server_alive,
            server_restarts: None,
            agent_version: agent_version.filter(|version| valid_version(version)),
            login_expires_at: login_expires_at
                .filter(|at| within(*at, now, LOGIN_EXPIRY_BEHIND, LOGIN_EXPIRY_AHEAD))
                .map(rfc3339),
            last_snapshot_at: None,
            unsaved: None,
            driver: None,
            driver_ref: None,
            config_commit: None,
            config_modified: None,
            config_state: None,
            setup: None,
        }
    }

    /// Name the driver an environment is watched through, and the driver's
    /// own id for the installation when it has one. A name that is not a
    /// short lower-case word is left out, and so is an id that is not an id;
    /// an id never goes without the name it belongs to.
    pub fn with_driver(mut self, name: Option<&str>, reference: Option<String>) -> Self {
        self.driver = name
            .filter(|name| name.len() <= 32 && valid_env_key(name))
            .map(str::to_string);
        self.driver_ref = reference.filter(|id| self.driver.is_some() && valid_session_id(id));
        self
    }

    /// Say where an environment's config stands. A commit that is not a
    /// commit id is left out, and the hand-change flag with it: there is no
    /// room here for a branch name, a subject or a sentence.
    pub fn with_config(mut self, config: &config_sync::Report) -> Self {
        self.config_commit = config.commit.clone().filter(|id| valid_commit_id(id));
        self.config_modified = config.modified.filter(|_| self.config_commit.is_some());
        self.config_state = config.state.map(|state| state.as_str().to_string());
        self
    }

    /// Say what the environment is set up with, as the box recipe wrote it
    /// down and [`SetupReport::new`] held it to its shape.
    pub fn with_setup(mut self, setup: Option<SetupReport>) -> Self {
        self.setup = setup;
        self
    }

    pub fn with_restarts(mut self, count: Option<u64>) -> EnvironmentReport {
        self.server_restarts = count.filter(|count| *count <= MAX_SERVER_RESTARTS);
        self
    }

    pub fn with_unsaved(mut self, unsaved: Option<UnsavedReport>) -> EnvironmentReport {
        self.unsaved = unsaved;
        self
    }

    pub fn with_snapshot(mut self, at: Option<SystemTime>, now: SystemTime) -> EnvironmentReport {
        self.last_snapshot_at = at.map(|at| rfc3339(at.min(now)));
        self
    }

    /// The report as a Watch that takes `taken` would accept it: every field
    /// of the contract, and of the [`AHEAD`] fields only those. A field that
    /// only means something beside one that is left out is left out too.
    pub fn only(&self, taken: &Taken) -> EnvironmentReport {
        let mut report = self.clone();
        if !taken.takes("driver") {
            report.driver = None;
        }
        if !taken.takes("driver_ref") || report.driver.is_none() {
            report.driver_ref = None;
        }
        if !taken.takes("config_commit") {
            report.config_commit = None;
        }
        if !taken.takes("config_modified") || report.config_commit.is_none() {
            report.config_modified = None;
        }
        if !taken.takes("config_state") {
            report.config_state = None;
        }
        if !taken.takes("setup") {
            report.setup = None;
        }
        report
    }

    /// The [`AHEAD`] fields this report carries.
    fn ahead(&self) -> impl Iterator<Item = &'static str> {
        [
            self.driver.is_some(),
            self.driver_ref.is_some(),
            self.config_commit.is_some(),
            self.config_modified.is_some(),
            self.config_state.is_some(),
            self.setup.is_some(),
        ]
        .into_iter()
        .zip(AHEAD)
        .filter_map(|(carried, field)| carried.then_some(field))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Heartbeat {
    pub machine: MachineHealth,
    pub environments: Vec<EnvironmentReport>,
    pub daemon_version: String,
    /// A promise: while a working session's tools run, at least one
    /// `activity` event for it is sent in this many seconds.
    pub activity_beat_s: u64,
    /// A maintenance pause announced on the box: until then Watch puts off
    /// the alerts for this machine going dark.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_until: Option<String>,
    pub client_ts: String,
}

impl Heartbeat {
    /// Every heartbeat is built here, so each one makes the same promise. One
    /// that left `activity_beat_s` out would tell Watch the promise was gone.
    pub fn new(
        machine: MachineHealth,
        environments: Vec<EnvironmentReport>,
        now: SystemTime,
    ) -> Heartbeat {
        Heartbeat {
            machine,
            environments,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            activity_beat_s: ACTIVITY_BEAT.as_secs(),
            paused_until: None,
            client_ts: rfc3339(now),
        }
    }

    /// Say the box is paused for maintenance until `until`. A time that has
    /// passed is no pause, and one more than a day ahead is sent as a day:
    /// Watch keeps no more.
    pub fn with_pause(mut self, until: Option<SystemTime>, now: SystemTime) -> Heartbeat {
        self.paused_until = until
            .filter(|until| *until > now)
            .map(|until| rfc3339(until.min(now + MAX_PAUSE)));
        self
    }

    /// The heartbeat as a Watch that takes `taken` would accept it. With
    /// nothing taken, that is the heartbeat every Watch accepts.
    pub fn only(&self, taken: &Taken) -> Heartbeat {
        Heartbeat {
            environments: self
                .environments
                .iter()
                .map(|report| report.only(taken))
                .collect(),
            ..self.clone()
        }
    }

    /// The [`AHEAD`] fields this heartbeat carries, for any environment.
    pub fn ahead(&self) -> BTreeSet<&'static str> {
        self.environments
            .iter()
            .flat_map(EnvironmentReport::ahead)
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatAck {
    #[serde(default)]
    pub heartbeat_interval_s: Option<u64>,
    /// The [`AHEAD`] fields this Watch takes, by name. A Watch that says
    /// nothing takes none of them.
    #[serde(default, deserialize_with = "names")]
    pub accepts: Vec<String>,
}

/// A list of names, read without ever failing the answer it is part of: what
/// is not a list is no names, and what is not a string in it is passed over.
fn names<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_string))
        .collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct RejectedEvent {
    pub event_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventsAck {
    #[serde(default)]
    pub accepted: u64,
    #[serde(default)]
    pub duplicates: u64,
    #[serde(default)]
    pub rejected: Vec<RejectedEvent>,
}

/// Whether Watch still accepts a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStatus {
    Valid,
    Rejected,
    Unknown(ApiError),
}

struct Answer {
    status: u16,
    retry_after: Option<Duration>,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// Sort a non-2xx answer written as RFC 7807 problem details.
    fn problem(&self) -> ApiError {
        match self.status {
            401 => ApiError::Unauthorized,
            408 => ApiError::Unreachable("the request timed out at Watch".to_string()),
            429 => ApiError::RateLimited {
                retry_after: self.retry_after,
            },
            400..=499 => {
                let body = self.json();
                ApiError::Refused {
                    status: self.status,
                    code: body["code"].as_str().map(str::to_string),
                    detail: body["detail"]
                        .as_str()
                        .or_else(|| body["title"].as_str())
                        .map(str::to_string),
                }
            }
            500..=599 => ApiError::Server {
                status: self.status,
            },
            other => ApiError::Unexpected(format!("status {other}")),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, ApiError> {
        serde_json::from_str(&self.body).map_err(|_| {
            ApiError::Unexpected(format!(
                "status {} with a body that is not the API's",
                self.status
            ))
        })
    }
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
}

impl Client {
    /// A client for the API at `base_url`. No request takes longer than `timeout`.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Client> {
        let base = normalize_api_url(base_url)?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            // A redirect could carry the token to another host. The API does
            // not redirect, so one is treated as an answer that is not the API's.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .user_agent(concat!("nucleate/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Client { agent, base })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn answer(
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Answer, ApiError> {
        let mut response = result.map_err(transport)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(transport)?;
        Ok(Answer {
            status,
            retry_after,
            body,
        })
    }

    fn form(&self, path: &str, fields: &[(&str, &str)]) -> Result<Answer, ApiError> {
        Self::answer(
            self.agent
                .post(format!("{}{path}", self.base))
                .header("Accept", "application/json")
                .send_form(fields.iter().copied()),
        )
    }

    fn ingest(&self, path: &str, token: &Token, body: &impl Serialize) -> Result<Answer, ApiError> {
        let answer = Self::answer(
            self.agent
                .post(format!("{}{path}", self.base))
                .header("Authorization", format!("Bearer {}", token.expose()))
                .header("Accept", "application/json")
                .send_json(body),
        )?;
        match answer.status {
            200..=299 => Ok(answer),
            _ => Err(answer.problem()),
        }
    }

    /// Ask for a code the owner can approve (RFC 8628 §3.1).
    pub fn device_authorization(
        &self,
        machine_name: Option<&str>,
    ) -> Result<DeviceAuthorization, ApiError> {
        let mut fields = vec![("client_id", CLIENT_ID), ("scope", SCOPE)];
        if let Some(name) = machine_name {
            fields.push(("machine_name", name));
        }
        let answer = self.form("/v1/oauth/device_authorization", &fields)?;
        match answer.status {
            200..=299 => answer.parse(),
            _ => Err(oauth_error(&answer)),
        }
    }

    /// Ask once whether the code has been approved (RFC 8628 §3.4).
    pub fn token(&self, device_code: &str) -> Result<TokenPoll, ApiError> {
        let answer = self.form(
            "/v1/oauth/token",
            &[
                ("grant_type", DEVICE_GRANT),
                ("device_code", device_code),
                ("client_id", CLIENT_ID),
            ],
        )?;
        let body = answer.json();
        if (200..=299).contains(&answer.status) {
            let text = |key: &str| body[key].as_str().map(str::to_string);
            let token = body["access_token"]
                .as_str()
                .and_then(|raw| Token::parse(raw).ok())
                .ok_or_else(|| ApiError::Unexpected("a grant with no usable token".to_string()))?;
            return Ok(TokenPoll::Granted(Grant {
                token,
                scope: text("scope"),
                machine_id: text("machine_id"),
                machine_name: text("machine_name"),
                account_name: text("account_name"),
            }));
        }
        match body["error"].as_str() {
            Some("authorization_pending") => Ok(TokenPoll::Pending),
            Some("slow_down") => Ok(TokenPoll::SlowDown),
            Some("access_denied") => Ok(TokenPoll::Denied),
            Some("expired_token") => Ok(TokenPoll::Expired),
            _ => Err(oauth_error(&answer)),
        }
    }

    /// Revoke a token (RFC 7009). Watch answers 200 whether or not the token
    /// was still live.
    pub fn revoke(&self, token: &Token) -> Result<(), ApiError> {
        let answer = self.form(
            "/v1/oauth/revoke",
            &[
                ("token", token.expose()),
                ("token_type_hint", "access_token"),
                ("client_id", CLIENT_ID),
            ],
        )?;
        match answer.status {
            200..=299 => Ok(()),
            _ => Err(oauth_error(&answer)),
        }
    }

    pub fn heartbeat(
        &self,
        token: &Token,
        heartbeat: &Heartbeat,
    ) -> Result<HeartbeatAck, ApiError> {
        self.ingest("/v1/heartbeats:ingest", token, heartbeat)?
            .parse()
    }

    /// Send a batch of at most 100 envelopes.
    pub fn events(&self, token: &Token, events: &[&Envelope]) -> Result<EventsAck, ApiError> {
        self.ingest("/v1/events:ingest", token, &json!({ "events": events }))?
            .parse()
    }

    /// Ask whether Watch still accepts a token, without reporting anything:
    /// an empty batch of events is answered with a validation error when the
    /// token is good, and with 401 when it is not.
    pub fn token_status(&self, token: &Token) -> TokenStatus {
        let events: [&Envelope; 0] = [];
        match self.events(token, &events) {
            Ok(_) | Err(ApiError::Refused { .. }) | Err(ApiError::RateLimited { .. }) => {
                TokenStatus::Valid
            }
            Err(ApiError::Unauthorized) => TokenStatus::Rejected,
            Err(other) => TokenStatus::Unknown(other),
        }
    }
}

fn transport(error: ureq::Error) -> ApiError {
    match error {
        ureq::Error::BadUri(_) | ureq::Error::Http(_) | ureq::Error::Protocol(_) => {
            ApiError::Unexpected(error.to_string())
        }
        ureq::Error::BodyExceedsLimit(_) => ApiError::Unexpected("an oversized answer".to_string()),
        other => ApiError::Unreachable(other.to_string()),
    }
}

/// Sort a non-2xx answer from an OAuth endpoint, which carries
/// `{ "error", "error_description" }` rather than problem details.
fn oauth_error(answer: &Answer) -> ApiError {
    let body = answer.json();
    match answer.status {
        429 => ApiError::RateLimited {
            retry_after: answer.retry_after,
        },
        400..=499 => ApiError::Refused {
            status: answer.status,
            code: body["error"]
                .as_str()
                .or_else(|| body["code"].as_str())
                .map(str::to_string),
            detail: body["error_description"]
                .as_str()
                .or_else(|| body["detail"].as_str())
                .map(str::to_string),
        },
        _ => answer.problem(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_anywhere_and_http_only_on_this_machine() {
        assert_eq!(
            normalize_api_url("https://api.nucarium.com/").unwrap(),
            "https://api.nucarium.com"
        );
        assert!(normalize_api_url("http://localhost:8787").is_ok());
        assert!(normalize_api_url("http://127.0.0.1:8787").is_ok());
        assert!(normalize_api_url("http://[::1]:8787").is_ok());
        assert!(normalize_api_url("http://api.nucarium.com").is_err());
        assert!(normalize_api_url("http://localhost.example.com").is_err());
        assert!(normalize_api_url("http://localhost@example.com").is_err());
        assert!(normalize_api_url("ftp://localhost").is_err());
        assert!(normalize_api_url("api.nucarium.com").is_err());
        assert!(normalize_api_url("https://api.nucarium.com/?x=1").is_err());
    }

    #[test]
    fn a_webhook_address_may_carry_a_query_and_nothing_secret_in_the_clear() {
        assert_eq!(
            normalize_webhook_url(" https://ntfy.sh/your-private-topic ").unwrap(),
            "https://ntfy.sh/your-private-topic"
        );
        assert!(normalize_webhook_url("https://hooks.example.test/x?key=1").is_ok());
        assert!(normalize_webhook_url("http://localhost:8080/topic").is_ok());
        assert!(normalize_webhook_url("http://ntfy.example.test/topic").is_err());
        assert!(normalize_webhook_url("https://user:pass@ntfy.example.test/topic").is_err());
        assert!(normalize_webhook_url("https://ntfy.sh/a topic").is_err());
        assert!(normalize_webhook_url("https://ntfy.sh/topic#x").is_err());
        assert!(normalize_webhook_url("ntfy.sh/topic").is_err());
    }

    fn at(ms: u64) -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn a_heartbeat_promises_the_activity_beat_and_names_only_what_it_knows() {
        let now = at(1_795_089_600_000);
        let machine = MachineHealth {
            disk_used_pct: 41.5,
            load_1m: 0.62,
            uptime_s: 864_000,
            ..MachineHealth::default()
        };
        let report = EnvironmentReport::new("client-a", true, None, None, now);
        let body = serde_json::to_value(Heartbeat::new(machine, vec![report], now)).unwrap();
        assert_eq!(body["activity_beat_s"], 300);
        assert_eq!(body["client_ts"], "2026-11-19T12:00:00.000Z");
        assert_eq!(
            body["environments"],
            json!([{ "env_id": "client-a", "server_alive": true }])
        );
        // With nothing more known, the machine says its three numbers and
        // the heartbeat names no pause.
        assert_eq!(
            body["machine"],
            json!({ "disk_used_pct": 41.5, "load_1m": 0.62, "uptime_s": 864_000 })
        );
        assert!(body.get("paused_until").is_none());
    }

    #[test]
    fn what_the_box_counted_and_probed_goes_out_as_numbers_times_and_yes_or_no() {
        let now = at(1_795_089_600_000);
        let minutes = |n: u64| Duration::from_secs(60 * n);
        let machine = MachineHealth {
            disk_used_pct: 41.5,
            load_1m: 0.62,
            uptime_s: 864_000,
            on_battery: Some(false),
            reboot_required: Some(true),
            ..MachineHealth::default()
        }
        .with_unsaved(Some((2, 5)))
        .with_snapshot(Some(now - minutes(50)), now);
        let report = EnvironmentReport::new("client-a", true, None, None, now)
            .with_restarts(Some(4))
            .with_unsaved(UnsavedReport::new(2, 5, now - minutes(8), now))
            .with_snapshot(Some(now - minutes(50)), now);
        let body = serde_json::to_value(
            Heartbeat::new(machine, vec![report], now).with_pause(Some(now + minutes(120)), now),
        )
        .unwrap();
        assert_eq!(
            body["machine"],
            json!({
                "disk_used_pct": 41.5, "load_1m": 0.62, "uptime_s": 864_000,
                "on_battery": false, "reboot_required": true,
                "uncommitted_repos": 2, "unpushed_commits": 5,
                "last_snapshot_at": "2026-11-19T11:10:00.000Z",
            })
        );
        assert_eq!(
            body["environments"],
            json!([{
                "env_id": "client-a", "server_alive": true, "server_restarts": 4,
                "last_snapshot_at": "2026-11-19T11:10:00.000Z",
                "unsaved": {
                    "uncommitted_repos": 2, "unpushed_commits": 5,
                    "counted_at": "2026-11-19T11:52:00.000Z",
                },
            }])
        );
        assert_eq!(body["paused_until"], "2026-11-19T14:00:00.000Z");
    }

    #[test]
    fn a_value_outside_what_watch_takes_is_left_out_of_a_heartbeat() {
        let now = at(1_795_089_600_000);
        let day = Duration::from_secs(86_400);
        assert_eq!(UnsavedReport::new(10_001, 0, now, now), None);
        assert_eq!(UnsavedReport::new(0, 1_000_001, now, now), None);
        assert!(UnsavedReport::new(10_000, 1_000_000, now, now).is_some());
        let machine = MachineHealth::default().with_unsaved(Some((10_001, 0)));
        assert_eq!(machine.uncommitted_repos, None);
        assert_eq!(machine.unpushed_commits, None);
        let report = EnvironmentReport::new("client-a", true, None, None, now);
        assert_eq!(
            report
                .clone()
                .with_restarts(Some(2_147_483_648))
                .server_restarts,
            None
        );
        // A time said to be past is never sent as one ahead of the box's clock.
        let counted = UnsavedReport::new(0, 0, now + day, now).unwrap();
        assert_eq!(counted.counted_at, "2026-11-19T12:00:00.000Z");
        assert_eq!(
            report
                .with_snapshot(Some(now + day), now)
                .last_snapshot_at
                .as_deref(),
            Some("2026-11-19T12:00:00.000Z")
        );
        // A pause that has passed is no pause; one too long is a day.
        let beat = |until| {
            Heartbeat::new(MachineHealth::default(), Vec::new(), now)
                .with_pause(Some(until), now)
                .paused_until
        };
        assert_eq!(beat(now), None);
        assert_eq!(beat(now - day), None);
        assert_eq!(
            beat(now + 3 * day).as_deref(),
            Some("2026-11-20T12:00:00.000Z")
        );
    }

    #[test]
    fn an_environment_reports_a_version_and_a_login_expiry_when_they_fit() {
        let now = at(1_795_089_600_000);
        let day = Duration::from_secs(86_400);
        let report = |version: &str, expires: SystemTime| {
            serde_json::to_value(EnvironmentReport::new(
                "client-a",
                true,
                Some(version.to_string()),
                Some(expires),
                now,
            ))
            .unwrap()
        };
        let known = report("2.1.0", now + day);
        assert_eq!(known["agent_version"], "2.1.0");
        assert_eq!(known["login_expires_at"], "2026-11-20T12:00:00.000Z");
        // Words where a version belongs, and times Watch would not keep.
        assert!(report("2.1.0 (Claude Code)", now)
            .get("agent_version")
            .is_none());
        assert!(report("", now).get("agent_version").is_none());
        let kept = |expires| report("2.1.0", expires).get("login_expires_at").is_some();
        assert!(kept(now - 30 * day));
        assert!(!kept(now - 31 * day));
        assert!(kept(now + 400 * day));
        assert!(!kept(now + 401 * day));
    }

    #[test]
    fn a_driver_and_a_config_commit_go_out_as_a_name_an_id_and_fixed_words() {
        let now = at(1_795_089_600_000);
        let report = || EnvironmentReport::new("t3", true, None, None, now);
        let commit = "a1".repeat(20);
        let known = report()
            .with_driver(Some("t3code"), Some("env_01HZX".into()))
            .with_config(&config_sync::Report {
                commit: Some(commit.clone()),
                modified: Some(true),
                state: Some(config_sync::State::Rejected),
                detail: Some("unverified".into()),
            });
        assert_eq!(
            serde_json::to_value(known).unwrap(),
            json!({
                "env_id": "t3", "server_alive": true,
                "driver": "t3code", "driver_ref": "env_01HZX",
                "config_commit": commit, "config_modified": true, "config_state": "rejected",
            })
        );
        // Words where a name, an id or a commit belongs are left out, and
        // what only means something beside them goes too.
        let wordy = report()
            .with_driver(Some("T3 Code, the desktop app"), Some("env_01HZX".into()))
            .with_config(&config_sync::Report {
                commit: Some("the commit that fixed the login page".into()),
                modified: Some(true),
                state: None,
                detail: Some("unverified".into()),
            });
        assert_eq!(
            serde_json::to_value(wordy).unwrap(),
            json!({ "env_id": "t3", "server_alive": true })
        );
        let no_ref = report().with_driver(Some("orca"), Some("an id with words in it".into()));
        assert_eq!(no_ref.driver.as_deref(), Some("orca"));
        assert_eq!(no_ref.driver_ref, None);
        // A sync that has applied nothing yet has a state and no commit.
        let waiting = report().with_config(&config_sync::Report {
            state: Some(config_sync::State::Pending),
            ..config_sync::Report::default()
        });
        assert_eq!(waiting.config_commit, None);
        assert_eq!(waiting.config_state.as_deref(), Some("pending"));
    }

    fn setup(script: Option<ScriptReport>) -> Option<SetupReport> {
        SetupReport::new(
            Network::Full,
            0,
            &["your-org/web".to_string(), "your-org/api".to_string()],
            2,
            0,
            script,
        )
    }

    #[test]
    fn an_environments_setup_goes_out_as_fixed_words_counts_names_and_times() {
        let now = at(1_795_089_600_000);
        let minutes = |n: u64| Duration::from_secs(60 * n);
        let failed = ScriptReport::new(
            ScriptState::Failed,
            now - minutes(10),
            Some(now - minutes(9)),
            Some(3),
            now,
        );
        let report =
            EnvironmentReport::new("client-a", true, None, None, now).with_setup(setup(failed));
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({
                "env_id": "client-a", "server_alive": true,
                "setup": {
                    "network": "full", "allowed_hosts": 0,
                    "repos": ["your-org/web", "your-org/api"],
                    "variables": 2, "secrets": 0,
                    "script": {
                        "state": "failed",
                        "started_at": "2026-11-19T11:50:00.000Z",
                        "finished_at": "2026-11-19T11:51:00.000Z",
                        "exit_code": 3,
                    },
                },
            })
        );
        // With no script run, no script; a run that has ended says no exit
        // unless it failed; one still running says when it started.
        let bare = serde_json::to_value(setup(None)).unwrap();
        assert!(bare.get("script").is_none(), "{bare}");
        let ok = ScriptReport::new(ScriptState::Ok, now - minutes(2), Some(now), Some(0), now);
        assert_eq!(
            serde_json::to_value(ok).unwrap(),
            json!({
                "state": "ok",
                "started_at": "2026-11-19T11:58:00.000Z",
                "finished_at": "2026-11-19T12:00:00.000Z",
            })
        );
        let running = ScriptReport::new(
            ScriptState::Running,
            now - minutes(2),
            Some(now),
            Some(1),
            now,
        );
        assert_eq!(
            serde_json::to_value(running).unwrap(),
            json!({ "state": "running", "started_at": "2026-11-19T11:58:00.000Z" })
        );
    }

    #[test]
    fn a_setup_watch_would_not_take_is_not_sent_and_what_it_cannot_vouch_for_is_left_out() {
        let now = at(1_795_089_600_000);
        let hour = Duration::from_secs(3_600);
        let repos = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let made = |hosts, variables, secrets, names: &[&str]| {
            SetupReport::new(
                Network::Limited,
                hosts,
                &repos(names),
                variables,
                secrets,
                None,
            )
        };
        assert!(made(10_000, 1_000, 1_000, &["web"; 32]).is_some());
        assert_eq!(made(10_001, 0, 0, &[]), None);
        assert_eq!(made(0, 1_001, 0, &[]), None);
        assert_eq!(made(0, 0, 1_001, &[]), None);
        assert_eq!(made(0, 0, 0, &["web"; 33]), None);
        assert_eq!(
            made(0, 0, 0, &["your-org/web", "a/b/c", "a sentence", "api"])
                .unwrap()
                .repos,
            ["your-org/web", "api"]
        );
        // A run's times are never ahead of the clock, and it never ends
        // before it starts or without an end.
        let ahead = ScriptReport::new(ScriptState::Ok, now - hour, Some(now + hour), None, now);
        assert_eq!(
            ahead.unwrap().finished_at.as_deref(),
            Some("2026-11-19T12:00:00.000Z")
        );
        let started = ScriptReport::new(ScriptState::Running, now + hour, None, None, now);
        assert_eq!(started.unwrap().started_at, "2026-11-19T12:00:00.000Z");
        assert_eq!(
            ScriptReport::new(ScriptState::Ok, now, Some(now - hour), None, now),
            None
        );
        assert_eq!(
            ScriptReport::new(ScriptState::Failed, now - hour, None, Some(1), now),
            None
        );
        // An exit status is a byte.
        for code in [-1, 256] {
            let failed =
                ScriptReport::new(ScriptState::Failed, now - hour, Some(now), Some(code), now);
            assert_eq!(failed.unwrap().exit_code, None);
        }
        let killed = ScriptReport::new(ScriptState::Failed, now - hour, Some(now), Some(137), now);
        assert_eq!(killed.unwrap().exit_code, Some(137));
    }

    #[test]
    fn a_field_ahead_of_the_contract_goes_only_to_a_watch_that_said_it_takes_it() {
        let now = at(1_795_089_600_000);
        let commit = "a1".repeat(20);
        let report = EnvironmentReport::new("t3", true, None, None, now)
            .with_restarts(Some(4))
            .with_driver(Some("t3code"), Some("env_01HZX".into()))
            .with_config(&config_sync::Report {
                commit: Some(commit.clone()),
                modified: Some(false),
                state: Some(config_sync::State::Ok),
                detail: None,
            })
            .with_setup(setup(None));
        let said = Heartbeat::new(MachineHealth::default(), vec![report], now);
        assert_eq!(said.ahead().into_iter().collect::<Vec<_>>(), {
            let mut all = AHEAD.to_vec();
            all.sort_unstable();
            all
        });
        let sent = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
            serde_json::to_value(said.only(&Taken::from_names(&names))).unwrap()["environments"][0]
                .clone()
        };
        // A Watch that has said nothing is sent what every Watch takes.
        assert_eq!(
            sent(&[]),
            json!({ "env_id": "t3", "server_alive": true, "server_restarts": 4 })
        );
        assert!(said.only(&Taken::default()).ahead().is_empty());
        assert_eq!(
            said.only(&Taken::from_names(&AHEAD.map(String::from))),
            said
        );
        // One that takes some is sent those, and a field that only means
        // something beside one it does not take stays behind with it.
        assert_eq!(
            sent(&["driver", "config_state"]),
            json!({
                "env_id": "t3", "server_alive": true, "server_restarts": 4,
                "driver": "t3code", "config_state": "ok",
            })
        );
        assert_eq!(
            sent(&["driver_ref", "config_modified"]),
            json!({ "env_id": "t3", "server_alive": true, "server_restarts": 4 })
        );
        // The setup goes whole to a Watch that takes it, and not at all to
        // one that does not.
        assert_eq!(
            sent(&["setup"])["setup"],
            json!({
                "network": "full", "allowed_hosts": 0,
                "repos": ["your-org/web", "your-org/api"], "variables": 2, "secrets": 0,
            })
        );
        assert!(sent(&["driver", "config_state"]).get("setup").is_none());
        // A name this program has no field for unlocks nothing.
        assert_eq!(sent(&["server_restarts", "prompt"]), sent(&[]));
    }

    #[test]
    fn an_answer_to_a_heartbeat_names_what_watch_takes_or_takes_nothing_new() {
        let ack = |body: Value| serde_json::from_value::<HeartbeatAck>(body).unwrap();
        // Watch as it is today names nothing.
        let today = ack(json!({ "received_at": "x", "heartbeat_interval_s": 60 }));
        assert_eq!(today.heartbeat_interval_s, Some(60));
        assert_eq!(Taken::from_names(&today.accepts), Taken::default());
        let later = ack(json!({ "heartbeat_interval_s": 60, "accepts": ["driver", "driver_ref"] }));
        let taken = Taken::from_names(&later.accepts);
        assert!(taken.takes("driver") && taken.takes("driver_ref"));
        assert!(!taken.takes("config_state"));
        // An answer that is otherwise good is never lost to this one field.
        for odd in [json!("driver"), json!({ "driver": true }), json!([1, null])] {
            let odd = ack(json!({ "heartbeat_interval_s": 300, "accepts": odd }));
            assert_eq!(odd.heartbeat_interval_s, Some(300));
            assert_eq!(Taken::from_names(&odd.accepts), Taken::default());
        }
    }

    #[test]
    fn text_from_the_network_cannot_drive_the_terminal() {
        assert_eq!(printable("ok\u{1b}[2Jgone\n"), "ok[2Jgone");
    }
}
