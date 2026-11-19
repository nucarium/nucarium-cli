//! The daemon's settings: `~/.config/nucleated/config.toml`.
//!
//! Every key is optional. With no file the daemon has no environments and no
//! target, and still runs.

use crate::envelope::valid_env_key;
use crate::paths;
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

/// One agent server on the box: one agent login over one nucleus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub name: String,
    pub root: PathBuf,
    pub unit: String,
    pub check: Option<String>,
    pub agent: String,
    pub config_dir: Option<PathBuf>,
    pub github_token_file: Option<PathBuf>,
    pub branch_prefix: Option<String>,
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
}

#[derive(Debug, Default, Deserialize)]
struct File {
    machine: Option<String>,
    target: Option<String>,
    state_dir: Option<PathBuf>,
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
}

#[derive(Debug, Default, Deserialize)]
struct UpsFile {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ReportFile {
    unsaved: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct NotifyFile {
    url: Option<String>,
    token_file: Option<PathBuf>,
    after: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WatchFile {
    url: Option<String>,
    token_file: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct HeartbeatFile {
    interval: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EnvironmentFile {
    name: String,
    root: PathBuf,
    unit: Option<String>,
    check: Option<String>,
    agent: Option<String>,
    config_dir: Option<PathBuf>,
    github_token_file: Option<PathBuf>,
    branch_prefix: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HousekeepingFile {
    refresh: Option<String>,
    cleanup: Option<String>,
    snapshot: Option<String>,
    cleanup_min_age: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
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
        Some(other) => bail!(
            "housekeeping.{key} is {other:?}; it takes \"hourly\", \"daily\", \"weekly\" or \"off\""
        ),
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
        let agent = entry.agent.unwrap_or_else(|| "claude-code".to_string());
        if crate::agent::by_name(&agent).is_none() {
            bail!(
                "environment {:?} has agent {agent:?}; the only agent is \"claude-code\"",
                entry.name
            );
        }
        environments.push(Environment {
            unit: entry
                .unit
                .unwrap_or_else(|| format!("agent-server@{}.service", entry.name)),
            name: entry.name,
            root: entry.root,
            check: entry.check.filter(|c| !c.trim().is_empty()),
            agent,
            config_dir: entry.config_dir,
            github_token_file: entry.github_token_file,
            branch_prefix: entry.branch_prefix,
        });
    }
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
                "refresh",
                file.housekeeping.refresh.as_deref(),
                Duration::from_secs(3_600),
            )?,
            cleanup: parse_schedule(
                "cleanup",
                file.housekeeping.cleanup.as_deref(),
                Duration::from_secs(604_800),
            )?,
            snapshot: parse_schedule(
                "snapshot",
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
        ups,
        report_unsaved,
        notify,
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
    fn environment_names_must_be_ones_watch_accepts() {
        let env = |name: &str| format!("[[environment]]\nname = \"{name}\"\nroot = \"/x\"\n");
        assert!(parse(&env("Client A")).is_err());
        assert!(parse(&(env("ops") + &env("ops"))).is_err());
        assert!(parse(&(env("ops") + "agent = \"another\"\n")).is_err());
        assert!(parse("[housekeeping]\nrefresh = \"sometimes\"\n").is_err());
    }
}
