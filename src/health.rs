//! Machine health for the heartbeat, and whether an agent server is alive.
//!
//! Four numbers about the box and one yes-or-no per environment. Nothing here
//! reads a session or a file inside a nucleus.

use crate::settings::Environment;
use crate::watch::MachineHealth;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Sample the machine. Disk is the fullest filesystem among `/` and the
/// environments' nuclei, since a full nucleus is what stops agents.
pub fn sample(environments: &[Environment]) -> MachineHealth {
    let disk = std::iter::once(Path::new("/"))
        .chain(environments.iter().map(|env| env.root.as_path()))
        .filter_map(disk_used_pct)
        .fold(0.0, f64::max);
    MachineHealth {
        disk_used_pct: round1(disk),
        load_1m: (load_1m().unwrap_or(0.0).clamp(0.0, 100_000.0) * 100.0).round() / 100.0,
        uptime_s: uptime_s().unwrap_or(0),
        memory_pressure_pct: memory_pressure_pct().map(round1),
    }
}

/// Run a command quietly and say whether it succeeded. A command that takes
/// longer than the timeout is stopped and counts as a failure.
fn succeeds(command: &mut Command, timeout: Duration) -> bool {
    let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
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
        let health = sample(&[environment(None)]);
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
    fn a_check_that_hangs_counts_as_not_alive() {
        let started = Instant::now();
        assert!(!succeeds(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(200)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
