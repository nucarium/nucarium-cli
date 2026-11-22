//! The daemon's settings: `~/.config/nucleated/config.toml`.
//!
//! Every key is optional. With no file the daemon has no environments and no
//! target, and still runs.

use crate::driver::{self, Driver};
use crate::envelope::{valid_branch, valid_env_key};
use crate::{agent, git, paths};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_API_URL: &str = "https://api.nucarium.com";
pub const API_URL_ENV: &str = "NUCARIUM_API_URL";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Hosted,
    None,
}

/// One agent server on the box: one agent login over one nucleus, or one
/// driver and the agents it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub name: String,
    /// The nucleus, or, for a driver that owns its worktrees, the folder the
    /// driver keeps them in.
    pub root: PathBuf,
    pub unit: String,
    pub check: Option<String>,
    /// The agent whose hooks this environment takes when a hook names no
    /// agent of its own. One of [`agent::NAMES`].
    pub agent: String,
    /// Set when the environment is watched through a driver. Its sessions
    /// then come from the driver's own state, as well as from any hook that
    /// lands here.
    pub driver: Option<DriverSettings>,
    pub config_dir: Option<PathBuf>,
    pub github_token_file: Option<PathBuf>,
    pub branch_prefix: Option<String>,
}

/// How an environment's driver is reached: the `driver` key and the
/// `driver_*` keys beside it. Each driver reads the few it names in
/// [`Driver::keys`], and the config is refused if it sets any other.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverSettings {
    /// One of [`driver::NAMES`].
    pub name: String,
    /// How often the driver is asked for its agents' state. `None` is "off":
    /// the environment is still reported, and nothing is asked.
    pub poll: Option<Duration>,
    /// `driver_home`: the driver's own data folder.
    pub home: Option<PathBuf>,
    /// `driver_url`: the driver's server, on this machine and nowhere else.
    pub url: Option<String>,
    /// `driver_socket`: the driver's local socket.
    pub socket: Option<PathBuf>,
    /// `driver_session`: which of the driver's named sessions to ask.
    pub session: Option<String>,
    /// `driver_token_file`: a read-only token the owner issued, mode 600.
    pub token_file: Option<PathBuf>,
    /// `driver_cli`: the driver's own command-line program.
    pub cli: Option<PathBuf>,
    /// `driver_dirs`: more folders the driver keeps worktrees in.
    pub dirs: Vec<PathBuf>,
}

impl Environment {
    /// The adapter for the driver this environment is watched through.
    pub fn driver_adapter(&self) -> Option<&'static dyn Driver> {
        driver::by_name(&self.driver.as_ref()?.name)
    }

    /// True when a driver, and not `nucleate`, owns the worktrees under this
    /// environment's root. Housekeeping then leaves every one of them alone.
    pub fn driver_owns_worktrees(&self) -> bool {
        self.driver_adapter()
            .is_some_and(|adapter| adapter.owns_worktrees())
    }
}

/// What a hand change to a synced file means when a new commit arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDrift {
    /// The new commit replaces it, after one copy is kept on the box.
    Overwrite,
    /// The environment is left alone until the hand change is undone.
    Keep,
}

/// `[config_sync]`: the owner's own repository of agent config, pulled by
/// the box. Absent means off: nothing runs and nothing is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSync {
    /// The git address. An argument to git on the box and nothing else: it
    /// is never logged, never written to the state folder and never sent.
    pub repository: String,
    /// The one pinned branch.
    pub branch: String,
    /// How often the branch is fetched. `None` is "off".
    pub interval: Option<Duration>,
    pub require_signed: bool,
    /// An OpenSSH allowed-signers file the owner keeps outside the synced
    /// repository.
    pub allowed_signers: Option<PathBuf>,
    pub on_drift: OnDrift,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Housekeeping {
    /// `None` is "off".
    pub refresh: Option<Duration>,
    pub cleanup: Option<Duration>,
    pub snapshot: Option<Duration>,
    pub cleanup_min_age: Duration,
}

/// Where the box speaks for itself while it cannot reach Watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notify {
    /// The owner's own webhook, shaped as ntfy takes one.
    pub url: String,
    pub token_file: Option<PathBuf>,
    /// How long Watch must have been unreachable before the box says so.
    pub after: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The file these came from, whether or not it exists.
    pub path: PathBuf,
    pub exists: bool,
    pub machine: Option<String>,
    /// As written. Use [`Settings::target`] for the one in force.
    pub target: Option<Target>,
    pub watch_url: Option<String>,
    pub token_file: PathBuf,
    pub heartbeat_interval: Duration,
    pub environments: Vec<Environment>,
    pub housekeeping: Housekeeping,
    pub state_dir: PathBuf,
    /// Where the daemon keeps what may hold content and so may not sit in
    /// the state folder: the clone of the config repository, and the one
    /// copy of what a sync replaced.
    pub cache_dir: PathBuf,
    /// Where the box recipe writes what each environment is set up with, and
    /// how its setup script last ran. Read, never written.
    pub setup_dir: PathBuf,
    pub spool_dir: PathBuf,
    pub spool_max_age: Duration,
    pub spool_max_events: usize,
    /// The UPS as NUT names it, such as `ups@localhost`. `None` on a box
    /// with no UPS.
    pub ups: Option<String>,
    /// How often unsaved work is counted. `None` is "off".
    pub report_unsaved: Option<Duration>,
    /// `None` is off, which is the default.
    pub notify: Option<Notify>,
    /// `None` is off, which is the default.
    pub config_sync: Option<ConfigSync>,
    /// What happens to a repository whose git config or pointers the daemon
    /// will not run git in: refused, the default, or only reported.
    pub git_vetting: git::Vetting,
}

// Every table refuses a key it does not know. A key that is misspelled is
// then an error that names it, and never a setting quietly left at its
// default: for `allowed_signers` that default is "check no signature".
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    machine: Option<String>,
    target: Option<String>,
    state_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    setup_dir: Option<PathBuf>,
    #[serde(default)]
    watch: WatchFile,
    #[serde(default)]
    heartbeat: HeartbeatFile,
    #[serde(default)]
    environment: Vec<EnvironmentFile>,
    #[serde(default)]
    housekeeping: HousekeepingFile,
    #[serde(default)]
    spool: SpoolFile,
    #[serde(default)]
    ups: UpsFile,
    #[serde(default)]
    report: ReportFile,
    #[serde(default)]
    notify: NotifyFile,
    config_sync: Option<ConfigSyncFile>,
    #[serde(default)]
    git: GitFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitFile {
    vetting: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSyncFile {
    repository: Option<String>,
    branch: Option<String>,
    interval: Option<String>,
    require_signed: Option<bool>,
    allowed_signers: Option<PathBuf>,
    on_drift: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpsFile {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportFile {
    unsaved: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyFile {
    url: Option<String>,
    token_file: Option<PathBuf>,
    after: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchFile {
    url: Option<String>,
    token_file: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatFile {
    interval: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentFile {
    name: String,
    root: PathBuf,
    unit: Option<String>,
    check: Option<String>,
    agent: Option<String>,
    driver: Option<String>,
    driver_poll: Option<String>,
    driver_home: Option<PathBuf>,
    driver_url: Option<String>,
    driver_socket: Option<PathBuf>,
    driver_session: Option<String>,
    driver_token_file: Option<PathBuf>,
    driver_cli: Option<PathBuf>,
    #[serde(default)]
    driver_dirs: Vec<PathBuf>,
    config_dir: Option<PathBuf>,
    github_token_file: Option<PathBuf>,
    branch_prefix: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HousekeepingFile {
    refresh: Option<String>,
    cleanup: Option<String>,
    snapshot: Option<String>,
    cleanup_min_age: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpoolFile {
    dir: Option<PathBuf>,
    max_age: Option<String>,
    max_events: Option<usize>,
}

/// `"90s"`, `"10m"`, `"1h"`, `"7d"`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| anyhow!("{text:?} needs a unit: s, m, h or d"))?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| anyhow!("{text:?} is not a duration like \"60s\" or \"7d\""))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => bail!("{text:?} needs a unit: s, m, h or d"),
    };
    Ok(Duration::from_secs(number * seconds))
}

fn parse_schedule(key: &str, text: Option<&str>, default: Duration) -> Result<Option<Duration>> {
    Ok(match text {
        None => Some(default),
        Some("off") => None,
        Some("hourly") => Some(Duration::from_secs(3_600)),
        Some("daily") => Some(Duration::from_secs(86_400)),
        Some("weekly") => Some(Duration::from_secs(604_800)),
        Some(other) => {
            bail!("{key} is {other:?}; it takes \"hourly\", \"daily\", \"weekly\" or \"off\"")
        }
    })
}

/// The config file: `NUCLEATED_CONFIG`, or `config.toml` in the config folder.
pub fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("NUCLEATED_CONFIG").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(paths::daemon_config_dir()?.join("config.toml"))
}

pub fn load() -> Result<Settings> {
    load_from(&config_path()?)
}

pub fn load_from(path: &Path) -> Result<Settings> {
    let exists = path.is_file();
    let file: File = if exists {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    } else {
        File::default()
    };
    from_file(path, exists, file).with_context(|| format!("in {}", path.display()))
}

/// `"a", "b" and "c"`, for an error that names what would have been taken.
fn listed(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An environment's `driver` and the `driver_*` keys beside it, held to what
/// that driver takes.
fn driver_settings(entry: &EnvironmentFile) -> Result<Option<DriverSettings>> {
    let set: Vec<&str> = [
        ("driver_poll", entry.driver_poll.is_some()),
        ("driver_home", entry.driver_home.is_some()),
        ("driver_url", entry.driver_url.is_some()),
        ("driver_socket", entry.driver_socket.is_some()),
        ("driver_session", entry.driver_session.is_some()),
        ("driver_token_file", entry.driver_token_file.is_some()),
        ("driver_cli", entry.driver_cli.is_some()),
        ("driver_dirs", !entry.driver_dirs.is_empty()),
    ]
    .into_iter()
    .filter_map(|(key, set)| set.then_some(key))
    .collect();
    let Some(name) = entry.driver.as_deref() else {
        if let Some(key) = set.first() {
            bail!(
                "environment {:?} sets {key} and names no driver",
                entry.name
            );
        }
        return Ok(None);
    };
    let Some(adapter) = driver::by_name(name) else {
        bail!(
            "environment {:?} has driver {name:?}; the drivers are {}",
            entry.name,
            listed(&driver::NAMES)
        );
    };
    if let Some(key) = set
        .iter()
        .find(|key| **key != "driver_poll" && !adapter.keys().contains(*key))
    {
        bail!(
            "environment {:?} sets {key}, which driver {name:?} does not read",
            entry.name
        );
    }
    let poll = match entry.driver_poll.as_deref() {
        None => Some(adapter.default_poll().max(adapter.min_poll())),
        Some("off") => None,
        Some(text) => {
            let every = parse_duration(text)
                .with_context(|| format!("environment {:?}, driver_poll", entry.name))?;
            if every < adapter.min_poll() {
                bail!(
                    "environment {:?} has driver_poll {text:?}; driver {name:?} is asked no \
                     more often than every {} seconds",
                    entry.name,
                    adapter.min_poll().as_secs()
                );
            }
            Some(every)
        }
    };
    let url = match entry.driver_url.as_deref() {
        None => None,
        Some(url) => Some(driver::loopback_origin(url).ok_or_else(|| {
            anyhow!(
                "environment {:?} has driver_url {url:?}; it takes an address on this machine \
                 and nothing more, such as \"http://127.0.0.1:3773\"",
                entry.name
            )
        })?),
    };
    if let Some(session) = entry.driver_session.as_deref() {
        let plain = (1..=64).contains(&session.len())
            && session
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
            && !session.starts_with(['.', '-']);
        if !plain {
            bail!(
                "environment {:?} has driver_session {session:?}; it takes a plain name",
                entry.name
            );
        }
    }
    // The program is run by name, with the driver's own fixed arguments. It
    // must not read as an option.
    if let Some(cli) = entry.driver_cli.as_deref() {
        let text = cli.to_string_lossy();
        if text.trim().is_empty() || text.starts_with('-') {
            bail!(
                "environment {:?} has driver_cli {text:?}; it takes a program's name or path",
                entry.name
            );
        }
    }
    Ok(Some(DriverSettings {
        name: name.to_string(),
        poll,
        home: entry.driver_home.clone(),
        url,
        socket: entry.driver_socket.clone(),
        session: entry.driver_session.clone(),
        token_file: entry.driver_token_file.clone(),
        cli: entry.driver_cli.clone(),
        dirs: entry.driver_dirs.clone(),
    }))
}

/// A driver that owns its worktrees keeps them in folders no nucleus and no
/// other driver shares: housekeeping works by folder, and must never find
/// itself inside one of them.
fn keep_drivers_folders_apart(environments: &[Environment]) -> Result<()> {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let folders = |environment: &Environment| -> Vec<PathBuf> {
        let more = environment
            .driver
            .iter()
            .filter(|_| environment.driver_owns_worktrees())
            .flat_map(|driver| driver.dirs.iter());
        std::iter::once(&environment.root)
            .chain(more)
            .map(|path| resolve(path))
            .collect()
    };
    for owner in environments.iter().filter(|e| e.driver_owns_worktrees()) {
        for other in environments.iter().filter(|e| e.name != owner.name) {
            for mine in folders(owner) {
                for theirs in folders(other) {
                    if mine.starts_with(&theirs) || theirs.starts_with(&mine) {
                        bail!(
                            "environment {:?} keeps its driver's own worktrees in {}, and \
                             environment {:?} uses {}; neither may be, hold or sit inside the \
                             other",
                            owner.name,
                            mine.display(),
                            other.name,
                            theirs.display()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn config_sync(file: ConfigSyncFile) -> Result<ConfigSync> {
    // Both go to git as arguments. Neither may read as an option.
    let argument = |key: &str, value: Option<String>| -> Result<String> {
        let value = value
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        if value.is_empty() {
            bail!("config_sync.{key} is not set");
        }
        if value.starts_with('-') || value.chars().any(char::is_control) {
            bail!("config_sync.{key} is not one git would take");
        }
        Ok(value)
    };
    let repository = argument("repository", file.repository)?;
    let branch = argument("branch", file.branch)?;
    if !valid_branch(&branch) || branch.contains("..") {
        bail!("config_sync.branch is {branch:?}; it takes one branch's name");
    }
    let require_signed = file
        .require_signed
        .unwrap_or(file.allowed_signers.is_some());
    if require_signed && file.allowed_signers.is_none() {
        bail!(
            "config_sync.require_signed is set and config_sync.allowed_signers is not; a \
             signature is checked against the signers you name"
        );
    }
    Ok(ConfigSync {
        repository,
        branch,
        interval: parse_schedule(
            "config_sync.interval",
            file.interval.as_deref(),
            Duration::from_secs(3_600),
        )?,
        require_signed,
        allowed_signers: file.allowed_signers,
        on_drift: match file.on_drift.as_deref() {
            None | Some("overwrite") => OnDrift::Overwrite,
            Some("keep") => OnDrift::Keep,
            Some(other) => {
                bail!("config_sync.on_drift is {other:?}; it takes \"overwrite\" or \"keep\"")
            }
        },
    })
}

fn from_file(path: &Path, exists: bool, file: File) -> Result<Settings> {
    let target = match file.target.as_deref() {
        None => None,
        Some("hosted") => Some(Target::Hosted),
        Some("none") => Some(Target::None),
        Some(other) => bail!("target is {other:?}; it takes \"hosted\" or \"none\""),
    };
    let mut environments = Vec::new();
    for entry in file.environment {
        if !valid_env_key(&entry.name) {
            bail!(
                "environment name {:?} must be lower-case letters, digits, '.', '_' or '-', \
                 starting with a letter or digit, at most 64 characters",
                entry.name
            );
        }
        if environments
            .iter()
            .any(|known: &Environment| known.name == entry.name)
        {
            bail!("two environments are named {:?}", entry.name);
        }
        let agent = entry
            .agent
            .clone()
            .unwrap_or_else(|| agent::DEFAULT.to_string());
        if agent::by_name(&agent).is_none() {
            bail!(
                "environment {:?} has agent {agent:?}; the agents are {}",
                entry.name,
                listed(&agent::NAMES)
            );
        }
        let driver = driver_settings(&entry)?;
        let adapter = driver
            .as_ref()
            .and_then(|driver| driver::by_name(&driver.name));
        if driver.is_some() && entry.config_dir.is_some() {
            bail!(
                "environment {:?} sets config_dir, which is one login's config folder; an \
                 environment watched through a driver is not one login",
                entry.name
            );
        }
        let check = entry.check.filter(|c| !c.trim().is_empty());
        let unit = match (
            entry.unit,
            adapter.and_then(|adapter| adapter.default_unit()),
        ) {
            (Some(unit), _) => unit,
            (None, Some(unit)) => unit.to_string(),
            // What a third party calls its service is not this program's to
            // guess: a wrong name would make a healthy environment look down.
            (None, None) if driver.is_some() && check.is_none() => bail!(
                "environment {:?} needs `unit` or `check`: its driver documents no unit name of \
                 its own",
                entry.name
            ),
            (None, None) => format!("agent-server@{}.service", entry.name),
        };
        let environment = Environment {
            unit,
            name: entry.name,
            root: entry.root,
            check,
            agent,
            driver,
            config_dir: entry.config_dir,
            github_token_file: entry.github_token_file,
            branch_prefix: entry.branch_prefix,
        };
        if let Some(adapter) = adapter {
            adapter.check_config(&environment).map_err(|why| {
                anyhow!(
                    "environment {:?}, driver {:?}: {why}",
                    environment.name,
                    adapter.name()
                )
            })?;
        }
        environments.push(environment);
    }
    keep_drivers_folders_apart(&environments)?;
    if environments.len() > 40 {
        bail!(
            "{} environments; Watch takes at most 40 from one machine",
            environments.len()
        );
    }
    let duration = |key: &str, text: Option<&String>, default: u64| -> Result<Duration> {
        match text {
            None => Ok(Duration::from_secs(default)),
            Some(text) => parse_duration(text).with_context(|| key.to_string()),
        }
    };
    let state_dir = match file.state_dir {
        Some(dir) => dir,
        None => paths::state_dir()?,
    };
    let cache_dir = match file.cache_dir {
        Some(dir) => dir,
        None => paths::cache_dir()?,
    };
    if cache_dir.starts_with(&state_dir) {
        bail!(
            "cache_dir is inside state_dir; the state folder holds no content, and the cache may"
        );
    }
    let config_sync = file.config_sync.map(config_sync).transpose()?;
    let token_file = match file.watch.token_file {
        Some(path) => path,
        None => paths::daemon_config_dir()?.join("token"),
    };
    let heartbeat_interval = duration("heartbeat.interval", file.heartbeat.interval.as_ref(), 60)?;
    if heartbeat_interval < Duration::from_secs(10) {
        bail!("heartbeat.interval is under ten seconds");
    }
    let ups = file
        .ups
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    // The name is handed to `upsc` as one argument. It must not read as an option.
    if let Some(name) = ups.as_deref() {
        let plain = name.len() <= 255
            && !name.starts_with('-')
            && name.bytes().all(|c| c.is_ascii_graphic());
        if !plain {
            bail!("ups.name is {name:?}; it takes a NUT name such as \"ups@localhost\"");
        }
    }
    let report_unsaved = match file.report.unsaved.as_deref() {
        None => Some(Duration::from_secs(600)),
        Some("off") => None,
        Some(text) => {
            let every = parse_duration(text).context("report.unsaved")?;
            if every < Duration::from_secs(60) {
                bail!("report.unsaved is under a minute; it takes \"1m\" or longer, or \"off\"");
            }
            Some(every)
        }
    };
    let git_vetting = match file.git.vetting.as_deref() {
        None | Some("enforce") => git::Vetting::Enforce,
        Some("report") => git::Vetting::Report,
        Some(other) => bail!("git.vetting is {other:?}; it takes \"enforce\" or \"report\""),
    };
    let notify = match file.notify.url.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(url) => {
            let after = duration("notify.after", file.notify.after.as_ref(), 600)?;
            if after < Duration::from_secs(120) {
                bail!("notify.after is under two minutes");
            }
            Some(Notify {
                url: crate::watch::normalize_webhook_url(url).context("notify.url")?,
                token_file: file.notify.token_file,
                after,
            })
        }
    };
    Ok(Settings {
        path: path.to_path_buf(),
        exists,
        machine: file
            .machine
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty()),
        target,
        watch_url: file.watch.url,
        token_file,
        heartbeat_interval,
        environments,
        housekeeping: Housekeeping {
            refresh: parse_schedule(
                "housekeeping.refresh",
                file.housekeeping.refresh.as_deref(),
                Duration::from_secs(3_600),
            )?,
            cleanup: parse_schedule(
                "housekeeping.cleanup",
                file.housekeeping.cleanup.as_deref(),
                Duration::from_secs(604_800),
            )?,
            snapshot: parse_schedule(
                "housekeeping.snapshot",
                file.housekeeping.snapshot.as_deref(),
                Duration::from_secs(3_600),
            )?,
            cleanup_min_age: duration(
                "housekeeping.cleanup_min_age",
                file.housekeeping.cleanup_min_age.as_ref(),
                604_800,
            )?,
        },
        spool_dir: file.spool.dir.unwrap_or_else(|| state_dir.join("spool")),
        spool_max_age: duration("spool.max_age", file.spool.max_age.as_ref(), 604_800)?,
        spool_max_events: file.spool.max_events.unwrap_or(10_000).max(1),
        state_dir,
        cache_dir,
        setup_dir: file
            .setup_dir
            .unwrap_or_else(|| PathBuf::from(crate::setup::DEFAULT_DIR)),
        ups,
        report_unsaved,
        notify,
        config_sync,
        git_vetting,
    })
}

impl Settings {
    /// The target in force: what the file says, else hosted once signed in.
    pub fn target(&self, signed_in: bool) -> Target {
        match self.target {
            Some(Target::None) => Target::None,
            _ if signed_in => Target::Hosted,
            _ => Target::None,
        }
    }

    pub fn environment(&self, name: &str) -> Option<&Environment> {
        self.environments.iter().find(|env| env.name == name)
    }

    /// The API to sign in to: the flag, the environment variable, the config
    /// file, then hosted Watch.
    pub fn login_api_url(&self, flag: Option<&str>) -> String {
        flag.map(str::to_string)
            .or_else(|| env::var(API_URL_ENV).ok().filter(|v| !v.trim().is_empty()))
            .or_else(|| self.watch_url.clone())
            .unwrap_or_else(|| DEFAULT_API_URL.to_string())
    }
}

/// This box's name: the config's `machine`, else the hostname.
pub fn machine_name(settings: &Settings) -> Option<String> {
    settings.machine.clone().or_else(hostname)
}

pub fn hostname() -> Option<String> {
    let from_file = fs::read_to_string("/proc/sys/kernel/hostname").ok();
    let name = from_file.or_else(|| {
        let out = std::process::Command::new("hostname").output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    })?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Settings> {
        from_file(Path::new("config.toml"), true, toml::from_str(text)?)
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert!(parse_duration("7").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("7w").is_err());
    }

    #[test]
    fn an_empty_file_is_a_daemon_with_nothing_to_watch() {
        let settings = parse("").unwrap();
        assert!(settings.environments.is_empty());
        assert_eq!(settings.target(false), Target::None);
        assert_eq!(settings.target(true), Target::Hosted);
        assert_eq!(settings.heartbeat_interval, Duration::from_secs(60));
        assert_eq!(
            settings.housekeeping.cleanup,
            Some(Duration::from_secs(604_800))
        );
        assert_eq!(settings.spool_max_events, 10_000);
    }

    #[test]
    fn target_none_keeps_a_signed_in_box_quiet() {
        let settings = parse("target = \"none\"").unwrap();
        assert_eq!(settings.target(true), Target::None);
        assert!(parse("target = \"self-hosted\"").is_err());
    }

    #[test]
    fn environments_default_their_unit_and_agent() {
        let settings = parse(
            "[[environment]]\nname = \"client-a\"\nroot = \"/home/agent/client-a\"\n\
             [[environment]]\nname = \"ops\"\nroot = \"/home/agent/ops\"\nunit = \"ops.service\"\n\
             check = \"true\"\n[housekeeping]\ncleanup = \"off\"\n",
        )
        .unwrap();
        assert_eq!(
            settings.environments[0].unit,
            "agent-server@client-a.service"
        );
        assert_eq!(settings.environments[0].agent, "claude-code");
        assert_eq!(settings.environments[1].unit, "ops.service");
        assert_eq!(settings.environments[1].check.as_deref(), Some("true"));
        assert_eq!(settings.housekeeping.cleanup, None);
    }

    #[test]
    fn the_ups_the_report_and_the_notifier_are_off_or_default_until_set() {
        let settings = parse("").unwrap();
        assert_eq!(settings.ups, None);
        assert_eq!(settings.report_unsaved, Some(Duration::from_secs(600)));
        assert_eq!(settings.notify, None);

        let settings = parse(
            "[ups]\nname = \"ups@localhost\"\n[report]\nunsaved = \"1m\"\n\
             [notify]\nurl = \"https://ntfy.sh/your-private-topic\"\ntoken_file = \"/run/t\"\n",
        )
        .unwrap();
        assert_eq!(settings.ups.as_deref(), Some("ups@localhost"));
        assert_eq!(settings.report_unsaved, Some(Duration::from_secs(60)));
        assert_eq!(
            settings.notify,
            Some(Notify {
                url: "https://ntfy.sh/your-private-topic".into(),
                token_file: Some(PathBuf::from("/run/t")),
                after: Duration::from_secs(600),
            })
        );
        assert_eq!(
            parse("[report]\nunsaved = \"off\"\n")
                .unwrap()
                .report_unsaved,
            None
        );
        // An address left empty is a notifier left off.
        assert_eq!(parse("[notify]\nurl = \"\"\n").unwrap().notify, None);
    }

    #[test]
    fn settings_that_would_cost_too_much_or_leak_are_refused() {
        assert!(parse("[report]\nunsaved = \"30s\"\n").is_err());
        assert!(parse("[report]\nunsaved = \"sometimes\"\n").is_err());
        assert!(parse("[ups]\nname = \"--help\"\n").is_err());
        assert!(parse("[ups]\nname = \"ups one\"\n").is_err());
        let notify = |rest: &str| parse(&format!("[notify]\n{rest}\n"));
        assert!(notify("url = \"http://ntfy.example.test/topic\"").is_err());
        assert!(notify("url = \"https://user:pass@ntfy.example.test/topic\"").is_err());
        assert!(notify("url = \"ntfy.sh/topic\"").is_err());
        assert!(notify("url = \"https://ntfy.sh/topic\"\nafter = \"1m\"").is_err());
        assert!(
            notify("url = \"http://127.0.0.1:8080/topic?priority=high\"\nafter = \"2m\"").is_ok()
        );
    }

    #[test]
    fn an_environment_runs_an_agent_by_name_and_may_be_watched_through_a_driver() {
        let env = |rest: &str| format!("[[environment]]\nname = \"box\"\nroot = \"/x\"\n{rest}\n");
        for name in agent::NAMES {
            let settings = parse(&env(&format!("agent = \"{name}\""))).unwrap();
            assert_eq!(settings.environments[0].agent, name);
            assert_eq!(settings.environments[0].driver, None);
        }
        let refused = parse(&env("agent = \"another\"")).unwrap_err().to_string();
        for name in agent::NAMES {
            assert!(refused.contains(name), "{refused}");
        }
        // A driver with a unit of its own that its documentation names.
        let t3 = parse(&env("driver = \"t3code\"")).unwrap();
        let environment = &t3.environments[0];
        let adapter = environment.driver_adapter().unwrap();
        assert_eq!(Some(environment.unit.as_str()), adapter.default_unit());
        assert_eq!(environment.agent, agent::DEFAULT);
        let driver = environment.driver.as_ref().unwrap();
        assert_eq!(driver.name, "t3code");
        assert_eq!(driver.poll, Some(adapter.default_poll()));
        assert!(!environment.driver_owns_worktrees());
        assert!(parse(&env("driver = \"orca\"")).unwrap().environments[0].driver_owns_worktrees());
        // The config's own unit stands over the driver's.
        let own = parse(&env("driver = \"t3code\"\nunit = \"t3.service\"")).unwrap();
        assert_eq!(own.environments[0].unit, "t3.service");
        // A driver that documents no unit is not given a guessed one.
        assert!(parse(&env("driver = \"herdr-core\"")).is_err());
        assert!(parse(&env("driver = \"herdr-core\"\ncheck = \"true\"")).is_ok());
        assert!(parse(&env("driver = \"herdr-tui\"\nunit = \"herdr.service\"")).is_ok());
        let unknown = parse(&env("driver = \"another\"")).unwrap_err().to_string();
        for name in driver::NAMES {
            assert!(unknown.contains(name), "{unknown}");
        }
        // One login's config folder is not a driver's.
        assert!(parse(&env(
            "driver = \"t3code\"\nconfig_dir = \"/home/agent/.claude\""
        ))
        .is_err());
    }

    #[test]
    fn a_driver_takes_only_the_keys_it_reads_and_is_asked_no_more_often_than_it_allows() {
        let env = |rest: &str| {
            parse(&format!(
                "[[environment]]\nname = \"box\"\nroot = \"/x\"\ncheck = \"true\"\n{rest}\n"
            ))
        };
        let driver = |rest: &str| env(rest).map(|s| s.environments[0].driver.clone().unwrap());
        let t3 = driver(
            "driver = \"t3code\"\ndriver_home = \"/home/agent/.t3\"\n\
             driver_url = \"http://127.0.0.1:3773/\"\ndriver_token_file = \"/run/t3.token\"\n\
             driver_poll = \"30s\"",
        )
        .unwrap();
        assert_eq!(t3.home, Some(PathBuf::from("/home/agent/.t3")));
        assert_eq!(t3.url.as_deref(), Some("http://127.0.0.1:3773"));
        assert_eq!(t3.token_file, Some(PathBuf::from("/run/t3.token")));
        assert_eq!(t3.poll, Some(Duration::from_secs(30)));
        let herdr = driver(
            "driver = \"herdr-core\"\ndriver_socket = \"/run/user/1001/herdr-core/default.sock\"\n\
             driver_session = \"default\"\ndriver_poll = \"1h\"",
        )
        .unwrap();
        assert_eq!(herdr.session.as_deref(), Some("default"));
        assert_eq!(herdr.poll, Some(Duration::from_secs(3_600)));
        let orca = driver(
            "driver = \"orca\"\ndriver_cli = \"/opt/orca-server/bin/orca-ide\"\n\
             driver_dirs = [\"/srv/more\"]\ndriver_poll = \"off\"",
        )
        .unwrap();
        assert_eq!(
            orca.cli,
            Some(PathBuf::from("/opt/orca-server/bin/orca-ide"))
        );
        assert_eq!(orca.dirs, [PathBuf::from("/srv/more")]);
        assert_eq!(orca.poll, None);

        // A key another driver reads, or one with no driver to read it.
        assert!(env("driver = \"t3code\"\ndriver_cli = \"t3\"").is_err());
        assert!(env("driver = \"orca\"\ndriver_url = \"http://127.0.0.1:6768\"").is_err());
        assert!(env("driver_poll = \"15s\"").is_err());
        assert!(env("driver_home = \"/home/agent/.t3\"").is_err());
        // Asked too often, or somewhere that is not this machine.
        for name in driver::NAMES {
            let often = format!("driver = \"{name}\"\ndriver_poll = \"0s\"");
            assert!(env(&often).is_err(), "{name} may be asked without pause");
        }
        assert!(env("driver = \"t3code\"\ndriver_poll = \"often\"").is_err());
        assert!(env("driver = \"t3code\"\ndriver_url = \"https://app.t3.codes\"").is_err());
        assert!(env("driver = \"t3code\"\ndriver_url = \"http://192.168.1.20:3773\"").is_err());
        // A name or a program that would read as something else.
        assert!(env("driver = \"herdr-tui\"\ndriver_session = \"../other\"").is_err());
        assert!(env("driver = \"herdr-tui\"\ndriver_session = \"my session\"").is_err());
        assert!(env("driver = \"orca\"\ndriver_cli = \"--help\"").is_err());
    }

    #[test]
    fn a_driver_that_owns_its_worktrees_shares_its_folders_with_no_one() {
        let two = |first: &str, second: &str| {
            parse(&format!(
                "[[environment]]\nname = \"orca\"\ndriver = \"orca\"\n{first}\n\
                 [[environment]]\nname = \"client-a\"\n{second}\n"
            ))
        };
        let nucleus = "root = \"/home/agent/client-a\"";
        assert!(two("root = \"/home/agent/orca/workspaces\"", nucleus).is_ok());
        // The same folder, one inside the other, and the other way about.
        assert!(two("root = \"/home/agent/client-a\"", nucleus).is_err());
        assert!(two("root = \"/home/agent/client-a/.worktrees\"", nucleus).is_err());
        assert!(two("root = \"/home/agent\"", nucleus).is_err());
        // The driver's other folders are held to the same rule.
        let more =
            "root = \"/home/agent/orca/workspaces\"\ndriver_dirs = [\"/home/agent/client-a/x\"]";
        assert!(two(more, nucleus).is_err());
        // A folder whose name only begins the same way is another folder.
        assert!(two("root = \"/home/agent/client\"", nucleus).is_ok());
        // A driver that owns nothing may sit over a nucleus.
        assert!(parse(
            "[[environment]]\nname = \"t3\"\ndriver = \"t3code\"\nroot = \"/home/agent/client-a\"\n\
             [[environment]]\nname = \"client-a\"\nroot = \"/home/agent/client-a\"\n"
        )
        .is_ok());
    }

    #[test]
    fn config_sync_is_off_until_a_repository_and_a_branch_are_named() {
        assert_eq!(parse("").unwrap().config_sync, None);
        let sync = |rest: &str| parse(&format!("[config_sync]\n{rest}\n"));
        let named = "repository = \"git@github.com:your-org/agent-config.git\"\nbranch = \"main\"";
        let plain = sync(named).unwrap().config_sync.unwrap();
        assert_eq!(
            plain,
            ConfigSync {
                repository: "git@github.com:your-org/agent-config.git".into(),
                branch: "main".into(),
                interval: Some(Duration::from_secs(3_600)),
                require_signed: false,
                allowed_signers: None,
                on_drift: OnDrift::Overwrite,
            }
        );
        let signed = sync(&format!(
            "{named}\ninterval = \"daily\"\nallowed_signers = \"/home/agent/.config/nucleated/signers\"\n\
             on_drift = \"keep\""
        ))
        .unwrap()
        .config_sync
        .unwrap();
        assert_eq!(signed.interval, Some(Duration::from_secs(86_400)));
        // Naming the signers is asking for the signature.
        assert!(signed.require_signed);
        assert_eq!(signed.on_drift, OnDrift::Keep);
        assert_eq!(
            sync(&format!("{named}\ninterval = \"off\""))
                .unwrap()
                .config_sync
                .unwrap()
                .interval,
            None
        );

        assert!(sync("").is_err());
        assert!(sync("repository = \"git@github.com:your-org/agent-config.git\"").is_err());
        assert!(sync("branch = \"main\"").is_err());
        assert!(sync(&format!("{named}\nrequire_signed = true")).is_err());
        assert!(sync(&format!("{named}\ninterval = \"sometimes\"")).is_err());
        assert!(sync(&format!("{named}\non_drift = \"merge\"")).is_err());
        // Both are handed to git. Neither may read as an option or a range.
        assert!(sync("repository = \"--upload-pack=sh\"\nbranch = \"main\"").is_err());
        assert!(sync("repository = \"/srv/config.git\"\nbranch = \"--all\"").is_err());
        assert!(sync("repository = \"/srv/config.git\"\nbranch = \"main..other\"").is_err());
        assert!(sync("repository = \"/srv/config.git\"\nbranch = \"two words\"").is_err());
    }

    #[test]
    fn a_key_no_table_knows_is_refused_by_name_and_never_quietly_dropped() {
        let refused = |text: &str, key: &str| {
            let error = match parse(text) {
                Ok(_) => panic!("{key} was taken, or dropped without a word"),
                Err(error) => format!("{error:#}"),
            };
            assert!(error.contains(key), "{key} is not named in: {error}");
        };
        let named = "repository = \"git@github.com:your-org/agent-config.git\"\nbranch = \"main\"";
        // Hyphens where the keys have underscores. Dropped, these would
        // leave signature checking off for an owner who had asked for it.
        refused(
            &format!("[config_sync]\n{named}\nallowed-signers = \"/home/agent/signers\"\n"),
            "allowed-signers",
        );
        refused(
            &format!("[config_sync]\n{named}\nrequire-signed = true\n"),
            "require-signed",
        );
        // A driver key that is nearly one: dropped, the folders it names
        // would be neither counted nor kept from housekeeping.
        let env = "[[environment]]\nname = \"box\"\nroot = \"/x\"\n";
        refused(
            &format!("{env}driver = \"orca\"\ndriver_dir = [\"/srv/more\"]\n"),
            "driver_dir",
        );
        refused(
            &format!("{env}config-dir = \"/home/agent/.claude\"\n"),
            "config-dir",
        );
        // And every other table, and the top of the file.
        refused("machine_name = \"agentbox\"\n", "machine_name");
        refused("[config-sync]\nbranch = \"main\"\n", "config-sync");
        refused("[watch]\napi_url = \"http://localhost:8787\"\n", "api_url");
        refused("[heartbeat]\nevery = \"60s\"\n", "every");
        refused("[housekeeping]\nsnapshots = \"hourly\"\n", "snapshots");
        refused("[spool]\nmax-events = 10\n", "max-events");
        refused("[ups]\nups = \"ups@localhost\"\n", "ups");
        refused("[report]\nunsaved_every = \"10m\"\n", "unsaved_every");
        refused("[notify]\nwebhook = \"https://ntfy.sh/topic\"\n", "webhook");
        // Every key the tables do know still loads, spelled as documented.
        let whole = parse(&format!(
            "machine = \"agentbox\"\ntarget = \"none\"\nstate_dir = \"/s\"\ncache_dir = \"/c\"\n\
             setup_dir = \"/e\"\n\
             [watch]\nurl = \"http://localhost:8787\"\ntoken_file = \"/t\"\n\
             [heartbeat]\ninterval = \"60s\"\n\
             {env}unit = \"u.service\"\ncheck = \"true\"\nagent = \"claude-code\"\n\
             config_dir = \"/home/agent/.claude\"\ngithub_token_file = \"/g\"\n\
             branch_prefix = \"workspace\"\n\
             [[environment]]\nname = \"orca\"\nroot = \"/o\"\ndriver = \"orca\"\n\
             driver_poll = \"off\"\ndriver_cli = \"orca-ide\"\ndriver_home = \"/h\"\n\
             driver_dirs = [\"/srv/more\"]\n\
             [[environment]]\nname = \"t3\"\nroot = \"/t3\"\ndriver = \"t3code\"\n\
             driver_url = \"http://127.0.0.1:3773\"\ndriver_token_file = \"/k\"\n\
             [[environment]]\nname = \"herdr\"\nroot = \"/hd\"\ndriver = \"herdr-core\"\n\
             check = \"true\"\ndriver_socket = \"/run/h.sock\"\ndriver_session = \"default\"\n\
             [housekeeping]\nrefresh = \"hourly\"\ncleanup = \"weekly\"\nsnapshot = \"hourly\"\n\
             cleanup_min_age = \"7d\"\n\
             [report]\nunsaved = \"10m\"\n[ups]\nname = \"ups@localhost\"\n\
             [notify]\nurl = \"https://ntfy.sh/topic\"\ntoken_file = \"/n\"\nafter = \"10m\"\n\
             [spool]\ndir = \"/sp\"\nmax_age = \"7d\"\nmax_events = 10000\n\
             [config_sync]\n{named}\ninterval = \"hourly\"\nrequire_signed = true\n\
             allowed_signers = \"/home/agent/signers\"\non_drift = \"keep\"\n"
        ))
        .unwrap();
        assert_eq!(whole.environments.len(), 4);
        assert!(whole.config_sync.unwrap().require_signed);
        assert_eq!(whole.setup_dir, PathBuf::from("/e"));
        refused("setup-dir = \"/e\"\n", "setup-dir");
    }

    #[test]
    fn the_setup_files_are_read_from_where_the_box_recipe_writes_them() {
        assert_eq!(
            parse("").unwrap().setup_dir,
            PathBuf::from("/var/lib/nucarium/environments")
        );
        assert_eq!(
            parse("setup_dir = \"/srv/environments\"\n")
                .unwrap()
                .setup_dir,
            PathBuf::from("/srv/environments")
        );
    }

    #[test]
    fn the_cache_is_kept_out_of_the_state_folder() {
        let settings =
            parse("state_dir = \"/var/state\"\ncache_dir = \"/var/cache/nucleated\"\n").unwrap();
        assert_eq!(settings.cache_dir, PathBuf::from("/var/cache/nucleated"));
        assert!(parse("state_dir = \"/var/state\"\ncache_dir = \"/var/state/cache\"\n").is_err());
        assert!(parse("state_dir = \"/var/state\"\ncache_dir = \"/var/state\"\n").is_err());
    }

    #[test]
    fn environment_names_must_be_ones_watch_accepts() {
        let env = |name: &str| format!("[[environment]]\nname = \"{name}\"\nroot = \"/x\"\n");
        assert!(parse(&env("Client A")).is_err());
        assert!(parse(&(env("ops") + &env("ops"))).is_err());
        assert!(parse(&(env("ops") + "agent = \"another\"\n")).is_err());
        assert!(parse("[housekeeping]\nrefresh = \"sometimes\"\n").is_err());
    }
}
