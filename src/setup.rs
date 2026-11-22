//! Environment setup: what the box recipe says an environment is, and how its
//! setup script last ran.
//!
//! The recipe, running as root, writes two small files per environment into
//! one folder, [`DEFAULT_DIR`] unless the config's `setup_dir` names another:
//! `<name>.json`, what the environment is, and `<name>.setup.json`, the setup
//! script's last run, written by the runner. The daemon reads both afresh for
//! every heartbeat. It never writes either, and never runs the script.
//!
//! What leaves the box is the shape of the environment: a fixed word for its
//! network, three counts, its repositories' names, and of the script's last
//! run a fixed word, two times and an exit code. The hash of the script, its
//! text, its output, the variables' names and values and the files' own
//! paths all stay on the box. A file that is missing, cannot be read or is
//! not one the recipe writes is no report, and never a failed heartbeat. The
//! one exception is a run file that is simply not there yet: the script has
//! not run, and the setup is reported with no script.

use crate::watch::{ScriptReport, SetupReport, MAX_SETUP_REPOS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the box recipe writes its files.
pub const DEFAULT_DIR: &str = "/var/lib/nucarium/environments";

/// The recipe's files are a few hundred bytes. One much larger is not one it
/// wrote, and is not read to the end.
const MAX_FILE: u64 = 64 * 1024;

/// What sandboxed commands in the environment's sessions may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    None,
    Limited,
    Full,
}

/// Where the setup script's last run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptState {
    Running,
    Ok,
    Failed,
}

/// `<name>.json`, as the recipe writes it. Every key is required; a key the
/// recipe adds later is passed over.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Defined {
    network: Network,
    allowed_hosts: u64,
    /// Read as values so that one that is not a name costs only itself.
    repos: Vec<Value>,
    variables: u64,
    secrets: u64,
    setup_script: bool,
}

/// `<name>.setup.json`, as the runner writes it. The script's hash is in the
/// file too, and is not read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Run {
    state: ScriptState,
    started_at_ms: u64,
    #[serde(default)]
    finished_at_ms: Option<u64>,
    #[serde(default)]
    exit_code: Option<i64>,
}

/// Why something about an environment's setup is not reported, in one fixed
/// word for `nucleated status` and one line for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Problem {
    /// There is no `<name>.json`: the recipe has not set the environment up.
    Missing,
    /// `<name>.json` is there and could not be read.
    Unreadable,
    /// `<name>.json` is not one the recipe writes.
    Malformed,
    /// A count, or the list of repositories, is more than Watch takes.
    OutOfRange,
    /// A repository's name is not an `owner/name` or a `name`. It alone is
    /// left out.
    RepoLeftOut,
    /// `<name>.setup.json` is there and could not be read. The whole setup
    /// is left out, not only the script.
    RunUnreadable,
    /// `<name>.setup.json` is not one the runner writes, or its times do not
    /// make a run. The whole setup is left out, not only the script.
    RunMalformed,
}

impl Problem {
    pub fn as_str(self) -> &'static str {
        match self {
            Problem::Missing => "missing",
            Problem::Unreadable => "unreadable",
            Problem::Malformed => "malformed",
            Problem::OutOfRange => "out_of_range",
            Problem::RepoLeftOut => "repo_left_out",
            Problem::RunUnreadable => "run_unreadable",
            Problem::RunMalformed => "run_malformed",
        }
    }

    /// One line for the daemon's log. Nothing from the file is in it.
    pub fn says(self) -> &'static str {
        match self {
            Problem::Missing => "no setup file from the box recipe; its setup is not reported",
            Problem::Unreadable => "its setup file cannot be read; its setup is not reported",
            Problem::Malformed => {
                "its setup file is not one the box recipe writes; its setup is not reported"
            }
            Problem::OutOfRange => {
                "its setup file has a count or a list of repositories larger than Watch takes; \
                 its setup is not reported"
            }
            Problem::RepoLeftOut => {
                "a repository in its setup file is not an owner/name or a name; it is left out"
            }
            Problem::RunUnreadable => {
                "its setup script's run file cannot be read; its setup is not reported"
            }
            Problem::RunMalformed => {
                "its setup script's run file is not one the runner writes; its setup is not \
                 reported"
            }
        }
    }
}

/// What one environment's setup comes to: the report, when there is one, and
/// why anything was left out of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub report: Option<SetupReport>,
    pub problems: Vec<Problem>,
}

pub fn defined_file(dir: &Path, environment: &str) -> PathBuf {
    dir.join(format!("{environment}.json"))
}

pub fn run_file(dir: &Path, environment: &str) -> PathBuf {
    dir.join(format!("{environment}.setup.json"))
}

/// One small file, or nothing when there is none. A file that cannot be read,
/// or is far larger than the recipe writes, is an error.
fn read_small(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    match bytes.len() as u64 > MAX_FILE {
        true => Err(()),
        false => Ok(Some(bytes)),
    }
}

fn at_ms(ms: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_millis(ms))
}

/// The setup script's last run, as Watch takes one, or nothing when it has
/// not run.
fn script(dir: &Path, environment: &str, now: SystemTime) -> Result<Option<ScriptReport>, Problem> {
    let Some(bytes) =
        read_small(&run_file(dir, environment)).map_err(|_| Problem::RunUnreadable)?
    else {
        return Ok(None);
    };
    let run: Run = serde_json::from_slice(&bytes).map_err(|_| Problem::RunMalformed)?;
    let started = at_ms(run.started_at_ms).ok_or(Problem::RunMalformed)?;
    let finished = run.finished_at_ms.and_then(at_ms);
    ScriptReport::new(run.state, started, finished, run.exit_code, now)
        .map(Some)
        .ok_or(Problem::RunMalformed)
}

/// Read one environment's two files and make its report. Nothing here fails:
/// what cannot be vouched for is left out, and [`Found::problems`] says what.
pub fn report(dir: &Path, environment: &str, now: SystemTime) -> Found {
    let defined = match read_small(&defined_file(dir, environment)) {
        Ok(Some(bytes)) => {
            serde_json::from_slice::<Defined>(&bytes).map_err(|_| Problem::Malformed)
        }
        Ok(None) => Err(Problem::Missing),
        Err(()) => Err(Problem::Unreadable),
    };
    let defined = match defined {
        Ok(defined) => defined,
        Err(problem) => {
            return Found {
                report: None,
                problems: vec![problem],
            }
        }
    };
    // A file that says there is no script is believed over a run file left
    // behind from when there was one. A run file that is there and cannot be
    // used costs the whole setup for now: a setup with no script would tell
    // Watch the script is gone, and close what it knows of a failed run.
    let script = match defined.setup_script {
        true => match script(dir, environment, now) {
            Ok(script) => script,
            Err(problem) => {
                return Found {
                    report: None,
                    problems: vec![problem],
                }
            }
        },
        false => None,
    };
    // A list longer than the recipe writes is not cut down to one Watch
    // takes: a shorter list would read as the whole of it.
    let repos: Vec<String> = defined
        .repos
        .iter()
        .filter_map(|repo| repo.as_str().map(str::to_string))
        .collect();
    let report = SetupReport::new(
        defined.network,
        defined.allowed_hosts,
        &repos,
        defined.variables,
        defined.secrets,
        script,
    )
    .filter(|_| defined.repos.len() <= MAX_SETUP_REPOS);
    let problems = match &report {
        None => vec![Problem::OutOfRange],
        Some(report) if report.repos.len() < defined.repos.len() => vec![Problem::RepoLeftOut],
        Some(_) => Vec::new(),
    };
    Found { report, problems }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(1_795_089_600_000)
    }

    fn defined(extra: Value) -> Value {
        let mut file = json!({
            "network": "full",
            "allowed_hosts": 0,
            "repos": ["your-org/web", "your-org/api"],
            "variables": 2,
            "secrets": 0,
            "setup_script": true,
        });
        for (key, value) in extra.as_object().unwrap() {
            file[key] = value.clone();
        }
        file
    }

    fn folder(defined: Option<Value>, run: Option<Value>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(defined) = defined {
            fs::write(defined_file(dir.path(), "client-a"), defined.to_string()).unwrap();
        }
        if let Some(run) = run {
            fs::write(run_file(dir.path(), "client-a"), run.to_string()).unwrap();
        }
        dir
    }

    fn found(defined: Option<Value>, run: Option<Value>) -> (Option<Value>, Vec<&'static str>) {
        let dir = folder(defined, run);
        let found = report(dir.path(), "client-a", now());
        (
            found
                .report
                .map(|report| serde_json::to_value(report).unwrap()),
            found.problems.into_iter().map(Problem::as_str).collect(),
        )
    }

    const HASH: &str = "9f2c1e7a5b3d4c6e8f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6";

    #[test]
    fn a_failed_run_is_reported_with_its_times_and_exit_code_and_never_its_hash() {
        let (report, problems) = found(
            Some(defined(json!({}))),
            Some(json!({
                "state": "failed",
                "started_at_ms": 1_795_082_400_000u64,
                "finished_at_ms": 1_795_082_442_000u64,
                "exit_code": 3,
                "script_sha256": HASH,
            })),
        );
        assert_eq!(problems, Vec::<&str>::new());
        assert_eq!(
            report.unwrap(),
            json!({
                "network": "full",
                "allowed_hosts": 0,
                "repos": ["your-org/web", "your-org/api"],
                "variables": 2,
                "secrets": 0,
                "script": {
                    "state": "failed",
                    "started_at": "2026-11-19T10:00:00.000Z",
                    "finished_at": "2026-11-19T10:00:42.000Z",
                    "exit_code": 3,
                },
            })
        );
    }

    #[test]
    fn each_state_of_a_run_says_only_what_belongs_to_it() {
        let run = |state: &str| {
            found(
                Some(defined(json!({}))),
                Some(json!({
                    "state": state,
                    "started_at_ms": 1_795_082_400_000u64,
                    "finished_at_ms": 1_795_082_442_000u64,
                    "exit_code": 0,
                    "script_sha256": HASH,
                })),
            )
            .0
            .unwrap()["script"]
                .clone()
        };
        assert_eq!(
            run("ok"),
            json!({
                "state": "ok",
                "started_at": "2026-11-19T10:00:00.000Z",
                "finished_at": "2026-11-19T10:00:42.000Z",
            })
        );
        // While it runs there is no end and no exit, whatever a stale file
        // left beside the start.
        assert_eq!(
            run("running"),
            json!({ "state": "running", "started_at": "2026-11-19T10:00:00.000Z" })
        );
        let (report, problems) = found(
            Some(defined(json!({}))),
            Some(json!({ "state": "running", "started_at_ms": 1_795_082_400_000u64 })),
        );
        assert_eq!(report.unwrap()["script"]["state"], "running");
        assert_eq!(problems, Vec::<&str>::new());
    }

    #[test]
    fn with_no_script_or_no_run_yet_there_is_no_script_in_the_report() {
        let run = json!({
            "state": "failed",
            "started_at_ms": 1_795_082_400_000u64,
            "finished_at_ms": 1_795_082_442_000u64,
            "exit_code": 1,
        });
        // The recipe took the script away; the last run's file is left behind.
        let (report, problems) = found(Some(defined(json!({ "setup_script": false }))), Some(run));
        let report = report.unwrap();
        assert!(report.get("script").is_none(), "{report}");
        assert_eq!(problems, Vec::<&str>::new());
        // A script that has not run yet.
        let (report, problems) = found(Some(defined(json!({}))), None);
        assert!(report.unwrap().get("script").is_none());
        assert_eq!(problems, Vec::<&str>::new());
    }

    #[test]
    fn a_missing_or_unreadable_or_malformed_file_is_no_report() {
        assert_eq!(found(None, None), (None, vec!["missing"]));
        for malformed in [
            json!("full"),
            json!({ "network": "everything", "allowed_hosts": 0, "repos": [], "variables": 0, "secrets": 0, "setup_script": false }),
            defined(json!({ "variables": -1 })),
            defined(json!({ "secrets": 1.5 })),
            defined(json!({ "setup_script": "yes" })),
            defined(json!({ "repos": "your-org/web" })),
            {
                let mut file = defined(json!({}));
                file.as_object_mut().unwrap().remove("network");
                file
            },
        ] {
            assert_eq!(found(Some(malformed), None), (None, vec!["malformed"]));
        }
        // Not JSON at all, and a folder where the file should be.
        let dir = folder(None, None);
        fs::write(defined_file(dir.path(), "client-a"), "network = full").unwrap();
        assert_eq!(
            report(dir.path(), "client-a", now()).problems,
            [Problem::Malformed]
        );
        let dir = folder(None, None);
        fs::create_dir(defined_file(dir.path(), "client-a")).unwrap();
        assert_eq!(
            report(dir.path(), "client-a", now()),
            Found {
                report: None,
                problems: vec![Problem::Unreadable],
            }
        );
        // Far larger than the recipe writes.
        let dir = folder(None, None);
        let mut big = defined(json!({}));
        big["padding"] = json!("x".repeat(70 * 1024));
        fs::write(defined_file(dir.path(), "client-a"), big.to_string()).unwrap();
        assert_eq!(
            report(dir.path(), "client-a", now()).problems,
            [Problem::Unreadable]
        );
        // A key the recipe adds later costs nothing.
        let (report, _) = found(Some(defined(json!({ "later": { "a": 1 } }))), None);
        assert!(report.unwrap().get("later").is_none());
    }

    #[test]
    fn a_run_file_that_is_there_and_cannot_be_used_costs_the_whole_setup() {
        // A setup with no script says the script is gone. A run file that is
        // there and unusable says nothing of the kind, so nothing is said.
        let ahead = 1_795_089_600_000u64;
        let bad_runs = [
            json!({ "state": "crashed", "started_at_ms": 1_795_082_400_000u64 }),
            json!({ "state": "ok", "started_at_ms": "yesterday" }),
            json!({ "finished_at_ms": 1_795_082_442_000u64 }),
            // Ended, and no end.
            json!({ "state": "ok", "started_at_ms": 1_795_082_400_000u64 }),
            json!({ "state": "failed", "started_at_ms": 1_795_082_400_000u64, "exit_code": 1 }),
            // Ended before it started.
            json!({ "state": "ok", "started_at_ms": 1_795_082_442_000u64, "finished_at_ms": 1_795_082_400_000u64 }),
            // Ended before it started, both ahead of the clock: as written
            // it is no run, whatever both would be once sent as now.
            json!({
                "state": "failed", "started_at_ms": ahead + 7_200_000,
                "finished_at_ms": ahead + 3_600_000, "exit_code": 1,
            }),
        ];
        for run in bad_runs {
            assert_eq!(
                found(Some(defined(json!({}))), Some(run.clone())),
                (None, vec!["run_malformed"]),
                "{run}"
            );
        }
        let dir = folder(Some(defined(json!({}))), None);
        fs::create_dir(run_file(dir.path(), "client-a")).unwrap();
        assert_eq!(
            report(dir.path(), "client-a", now()),
            Found {
                report: None,
                problems: vec![Problem::RunUnreadable],
            }
        );
        // With no script, a bad run file left behind costs nothing.
        let (report, problems) = found(
            Some(defined(json!({ "setup_script": false }))),
            Some(json!({ "state": "crashed" })),
        );
        assert_eq!(report.unwrap()["variables"], 2);
        assert_eq!(problems, Vec::<&str>::new());
    }

    #[test]
    fn a_count_out_of_range_drops_the_whole_report() {
        for over in [
            json!({ "allowed_hosts": 10_001 }),
            json!({ "variables": 1_001 }),
            json!({ "secrets": 1_001 }),
            json!({ "repos": vec!["web"; 33] }),
        ] {
            assert_eq!(
                found(Some(defined(over.clone())), None),
                (None, vec!["out_of_range"]),
                "{over}"
            );
        }
        let (report, problems) = found(
            Some(defined(json!({
                "network": "limited", "allowed_hosts": 10_000, "variables": 1_000,
                "secrets": 1_000, "repos": vec!["web"; 32],
            }))),
            None,
        );
        let report = report.unwrap();
        assert_eq!(report["network"], "limited");
        assert_eq!(report["allowed_hosts"], 10_000);
        assert_eq!(report["repos"].as_array().unwrap().len(), 32);
        assert_eq!(problems, Vec::<&str>::new());
    }

    #[test]
    fn a_repo_that_is_not_a_name_is_left_out_alone() {
        let (report, problems) = found(
            Some(defined(json!({
                "repos": ["your-org/web", "a/b/c", "has space", "", 7, "api", "../etc/passwd"],
            }))),
            None,
        );
        assert_eq!(report.unwrap()["repos"], json!(["your-org/web", "api"]));
        assert_eq!(problems, ["repo_left_out"]);
    }

    #[test]
    fn a_time_ahead_of_the_clock_is_sent_as_now() {
        let ahead = 1_795_089_600_000u64 + 3_600_000;
        let (report, _) = found(
            Some(defined(json!({}))),
            Some(json!({
                "state": "ok", "started_at_ms": 1_795_089_000_000u64, "finished_at_ms": ahead,
            })),
        );
        assert_eq!(
            report.unwrap()["script"],
            json!({
                "state": "ok",
                "started_at": "2026-11-19T11:50:00.000Z",
                "finished_at": "2026-11-19T12:00:00.000Z",
            })
        );
        let (report, _) = found(
            Some(defined(json!({}))),
            Some(json!({ "state": "running", "started_at_ms": ahead })),
        );
        assert_eq!(
            report.unwrap()["script"]["started_at"],
            "2026-11-19T12:00:00.000Z"
        );
    }

    #[test]
    fn an_exit_code_that_is_not_one_is_left_out() {
        for code in [json!(256), json!(-1)] {
            let (report, problems) = found(
                Some(defined(json!({}))),
                Some(json!({
                    "state": "failed", "started_at_ms": 1_795_082_400_000u64,
                    "finished_at_ms": 1_795_082_442_000u64, "exit_code": code,
                })),
            );
            let script = report.unwrap()["script"].clone();
            assert_eq!(script["state"], "failed");
            assert!(script.get("exit_code").is_none(), "{script}");
            assert_eq!(problems, Vec::<&str>::new());
        }
    }
}
