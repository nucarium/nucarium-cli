//! Machine health for the heartbeat, and whether an agent server is alive.
//!
//! Four numbers and two yes-or-noes about the box, and a yes-or-no and a
//! count per environment. Nothing here reads a session or a file inside a
//! nucleus, and nothing here changes anything: every probe only asks.

use crate::settings::Environment;
use crate::watch::MachineHealth;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// No probe prints more than a word or a number. More than this is not an answer.
const PROBE_OUTPUT: u64 = 4_096;
/// Where Debian and Ubuntu say a reboot is needed, and the file that tells
/// such a system from one with another convention.
const REBOOT_FLAG: &str = "/var/run/reboot-required";
const DEBIAN_FAMILY: &str = "/etc/debian_version";

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// The share of a filesystem in use, as `df` reports it.
#[cfg(unix)]
fn disk_used_pct(path: &Path) -> Option<f64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a NUL-terminated string that outlives the call, and
    // `stat` is a properly sized, writable `statvfs`.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let used = (stat.f_blocks as f64) - (stat.f_bfree as f64);
    let reachable = used + stat.f_bavail as f64;
    (reachable > 0.0).then(|| (used / reachable * 100.0).clamp(0.0, 100.0))
}

#[cfg(not(unix))]
fn disk_used_pct(_: &Path) -> Option<f64> {
    None
}

#[cfg(unix)]
fn load_1m() -> Option<f64> {
    let mut loads = [0f64; 3];
    // SAFETY: `loads` has room for the three values asked for.
    let filled = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    (filled >= 1).then_some(loads[0])
}

#[cfg(not(unix))]
fn load_1m() -> Option<f64> {
    None
}

fn uptime_s() -> Option<u64> {
    if let Ok(text) = fs::read_to_string("/proc/uptime") {
        return text
            .split_whitespace()
            .next()?
            .parse::<f64>()
            .ok()
            .map(|s| s as u64);
    }
    // No /proc: ask the kernel when it booted (macOS and the BSDs).
    let out = Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let booted: u64 = text
        .split("sec = ")
        .nth(1)?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(booted))
}

/// Share of the last minute in which some task stalled on memory, from the
/// kernel's pressure stall information. Absent where the kernel has none.
fn memory_pressure_pct() -> Option<f64> {
    parse_pressure(&fs::read_to_string("/proc/pressure/memory").ok()?)
}

fn parse_pressure(text: &str) -> Option<f64> {
    text.lines()
        .find(|line| line.starts_with("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg60="))?
        .parse::<f64>()
        .ok()
        .map(|pct| pct.clamp(0.0, 100.0))
}

/// Whether the operating system says it needs a reboot. Debian and Ubuntu
/// say so with a file; on a system of another family its absence would mean
/// nothing, so there the answer is not known.
fn reboot_required_at(flag: &Path, family: &Path) -> Option<bool> {
    family.exists().then(|| flag.exists())
}

/// True when a UPS status as NUT reports it, such as `OB DISCHRG`, holds the
/// token for "on battery". Nothing for a status with no tokens at all.
fn status_on_battery(status: &str) -> Option<bool> {
    let mut tokens = status.split_whitespace().peekable();
    tokens.peek()?;
    Some(tokens.any(|token| token == "OB"))
}

/// Whether the box runs from its UPS battery, asked of NUT. Nothing on a box
/// with no UPS configured, and when NUT gives no answer.
pub fn on_battery(ups: Option<&str>) -> Option<bool> {
    let status = output(
        Command::new("upsc").args([ups?, "ups.status"]),
        PROBE_TIMEOUT,
    )?;
    status_on_battery(&status)
}

/// Sample the machine. Disk is the fullest filesystem among `/` and the
/// environments' nuclei, since a full nucleus is what stops agents. `ups` is
/// the UPS to ask about its battery, on a box that has one.
pub fn sample(environments: &[Environment], ups: Option<&str>) -> MachineHealth {
    let disk = std::iter::once(Path::new("/"))
        .chain(environments.iter().map(|env| env.root.as_path()))
        .filter_map(disk_used_pct)
        .fold(0.0, f64::max);
    MachineHealth {
        disk_used_pct: round1(disk),
        load_1m: (load_1m().unwrap_or(0.0).clamp(0.0, 100_000.0) * 100.0).round() / 100.0,
        uptime_s: uptime_s().unwrap_or(0),
        memory_pressure_pct: memory_pressure_pct().map(round1),
        on_battery: on_battery(ups),
        reboot_required: reboot_required_at(Path::new(REBOOT_FLAG), Path::new(DEBIAN_FAMILY)),
        ..MachineHealth::default()
    }
}

/// Run a command quietly and wait for it, at most `timeout`. A command that
/// takes longer is stopped and counts as a failure.
fn finishes(
    command: &mut Command,
    stdout: Stdio,
    timeout: Duration,
) -> Option<std::process::Child> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then_some(child),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Run a command quietly and say whether it succeeded.
fn succeeds(command: &mut Command, timeout: Duration) -> bool {
    finishes(command, Stdio::null(), timeout).is_some()
}

/// Run a command quietly and return the little it printed, or nothing when it
/// failed or took too long. A command that prints more than a pipe holds
/// never finishes, and so counts as one that took too long.
fn output(command: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = finishes(command, Stdio::piped(), timeout)?;
    let mut text = String::new();
    child
        .stdout
        .take()?
        .take(PROBE_OUTPUT)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Whether an environment's agent server is alive: its `check` command exits
/// 0, or, with no `check`, its systemd user unit is active. The daemon only
/// asks; it never starts or stops an agent server.
pub fn server_alive(environment: &Environment) -> bool {
    match &environment.check {
        Some(check) => succeeds(Command::new("sh").args(["-c", check]), PROBE_TIMEOUT),
        None => succeeds(
            Command::new("systemctl").args(["--user", "is-active", "--quiet", &environment.unit]),
            PROBE_TIMEOUT,
        ),
    }
}

fn parse_restarts(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// How often systemd has restarted an environment's agent server: the unit's
/// own running count. Nothing for an environment watched by a `check`
/// command, which has no supervisor this program can ask, and nothing when
/// systemd gives no answer.
pub fn server_restarts(environment: &Environment) -> Option<u64> {
    if environment.check.is_some() {
        return None;
    }
    let count = output(
        Command::new("systemctl").args([
            "--user",
            "show",
            "--property=NRestarts",
            "--value",
            &environment.unit,
        ]),
        PROBE_TIMEOUT,
    )?;
    parse_restarts(&count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn environment(check: Option<&str>) -> Environment {
        Environment {
            name: "client-a".into(),
            root: PathBuf::from("/"),
            unit: "agent-server@client-a.service".into(),
            check: check.map(str::to_string),
            agent: "claude-code".into(),
            config_dir: None,
            github_token_file: None,
            branch_prefix: None,
        }
    }

    #[test]
    fn health_is_within_the_ranges_watch_accepts() {
        let health = sample(&[environment(None)], None);
        assert_eq!(health.on_battery, None);
        assert!((0.0..=100.0).contains(&health.disk_used_pct));
        assert!(health.disk_used_pct > 0.0);
        assert!((0.0..=100_000.0).contains(&health.load_1m));
        assert!(health.uptime_s > 0);
        assert!(health
            .memory_pressure_pct
            .is_none_or(|p| (0.0..=100.0).contains(&p)));
    }

    #[test]
    fn memory_pressure_is_the_one_minute_some_average() {
        let text = "some avg10=0.00 avg60=3.25 avg300=1.00 total=12345\n\
                    full avg10=0.00 avg60=0.50 avg300=0.10 total=678\n";
        assert_eq!(parse_pressure(text), Some(3.25));
        assert_eq!(parse_pressure(""), None);
    }

    #[test]
    fn an_agent_server_is_alive_while_its_check_passes() {
        assert!(server_alive(&environment(Some("true"))));
        assert!(!server_alive(&environment(Some("false"))));
        assert!(!server_alive(&environment(Some("exit 3"))));
    }

    #[test]
    fn a_probe_returns_what_the_command_printed_or_nothing() {
        let run = |script: &str| {
            output(
                Command::new("sh").args(["-c", script]),
                Duration::from_secs(5),
            )
        };
        assert_eq!(run("echo 4").as_deref(), Some("4\n"));
        assert_eq!(run("echo 4; exit 3"), None);
        assert_eq!(run("echo words >&2").as_deref(), Some(""));
        assert_eq!(
            output(
                &mut Command::new("no-such-probe-command"),
                Duration::from_secs(5)
            ),
            None
        );
        let started = Instant::now();
        assert_eq!(
            output(
                Command::new("sh").args(["-c", "echo 4; sleep 30"]),
                Duration::from_millis(200)
            ),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_restart_count_is_a_number_and_nothing_else() {
        assert_eq!(parse_restarts("4\n"), Some(4));
        assert_eq!(parse_restarts("0"), Some(0));
        assert_eq!(parse_restarts(""), None);
        assert_eq!(parse_restarts("-1"), None);
        assert_eq!(parse_restarts("Unit not found"), None);
        // An environment watched by a command has no supervisor to ask.
        assert_eq!(server_restarts(&environment(Some("true"))), None);
    }

    #[test]
    fn the_ups_is_on_battery_when_its_status_says_ob() {
        assert_eq!(status_on_battery("OL\n"), Some(false));
        assert_eq!(status_on_battery("OL CHRG"), Some(false));
        assert_eq!(status_on_battery("OB DISCHRG\n"), Some(true));
        assert_eq!(status_on_battery("LB OB"), Some(true));
        // A token that only contains the letters is not the token.
        assert_eq!(status_on_battery("OBSERVING"), Some(false));
        assert_eq!(status_on_battery(""), None);
        assert_eq!(status_on_battery("\n"), None);
        assert_eq!(on_battery(None), None);
    }

    #[test]
    fn a_reboot_is_required_where_the_system_says_so_and_unknown_where_it_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let (flag, family) = (
            dir.path().join("reboot-required"),
            dir.path().join("family"),
        );
        assert_eq!(reboot_required_at(&flag, &family), None);
        std::fs::write(&family, "13.1\n").unwrap();
        assert_eq!(reboot_required_at(&flag, &family), Some(false));
        std::fs::write(&flag, "").unwrap();
        assert_eq!(reboot_required_at(&flag, &family), Some(true));
    }

    #[test]
    fn a_check_that_hangs_counts_as_not_alive() {
        let started = Instant::now();
        assert!(!succeeds(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
