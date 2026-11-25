//! The agent process behind a session, so the daemon can tell when it has
//! gone without a `SessionEnd`.
//!
//! A hook records the process that ran it: the first of its ancestors that is
//! not a shell, by its pid and its start time. The daemon later looks for that
//! process. A hook may run in a container of its own, in a pid namespace the
//! daemon is not in, so a process is found by the innermost of its `NSpid`
//! values, which is its pid as the hook saw it. The start time tells it apart
//! from a later process given the same pid.
//!
//! This is a fact that stays on the box: it is kept in the spool record and
//! the session file, and never goes into an event. Linux only; elsewhere
//! nothing is recorded and nothing is looked for.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// One process, as a hook saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Identity {
    /// The pid in the hook's own pid namespace.
    pub pid: u32,
    /// When it started, in clock ticks after boot: field 22 of
    /// `/proc/<pid>/stat`.
    pub start: u64,
}

/// What a hook's command runs under on its way from the agent. Anything else
/// is the agent.
const SHELLS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "ash", "ksh", "mksh", "fish", "csh", "tcsh", "busybox",
];

/// No agent starts its hooks through more shells than this.
const MAX_SHELLS: usize = 16;

/// The few fields of `/proc/<pid>/stat` read here.
struct Stat {
    name: String,
    parent: u32,
    start: u64,
}

fn stat(proc: &Path, pid: u32) -> Option<Stat> {
    let text = fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?;
    // The name is in brackets and may hold spaces and brackets of its own.
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?.to_string();
    // From the third field on: the state, the parent, and the start time
    // twentieth after the state.
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    Some(Stat {
        name,
        parent: rest.get(1)?.parse().ok()?,
        start: rest.get(19)?.parse().ok()?,
    })
}

/// The agent that ran a hook whose parent is `parent`: the first process up
/// from there that is not a shell. Nothing when it cannot be read, or when
/// the climb reaches the first process without finding one.
pub fn agent_of(proc: &Path, parent: u32) -> Option<Identity> {
    let mut pid = parent;
    for _ in 0..MAX_SHELLS {
        if pid <= 1 {
            return None;
        }
        let stat = stat(proc, pid)?;
        if !SHELLS.contains(&stat.name.as_str()) {
            return Some(Identity {
                pid,
                start: stat.start,
            });
        }
        pid = stat.parent;
    }
    None
}

/// The agent behind the hook now running, on Linux.
pub fn of_this_hook() -> Option<Identity> {
    #[cfg(target_os = "linux")]
    {
        agent_of(Path::new("/proc"), std::os::unix::process::parent_id())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The `/proc` the daemon looks in, or nothing where there is none.
pub fn proc_root() -> Option<PathBuf> {
    if cfg!(target_os = "linux") {
        Some(PathBuf::from("/proc"))
    } else {
        None
    }
}

/// True when `/proc` is mounted so that other users' processes cannot be
/// read. A process in a rootless container may run as one of them, and one
/// that cannot be seen must not be taken for one that has gone.
fn hides_processes(proc: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string(proc.join("mounts")) else {
        return false;
    };
    mounts.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.get(1) == Some(&"/proc")
            && fields.get(2) == Some(&"proc")
            && fields.get(3).is_some_and(|options| {
                options.split(',').any(|option| {
                    option
                        .strip_prefix("hidepid=")
                        .is_some_and(|value| value != "0" && value != "off")
                })
            })
    })
}

/// Which of `wanted` are running, looking in `proc`. Nothing when that cannot
/// be told: no process could be read at all, or `/proc` hides some.
pub fn running(proc: &Path, wanted: &BTreeSet<Identity>) -> Option<BTreeSet<Identity>> {
    if hides_processes(proc) {
        return None;
    }
    let pids: BTreeSet<u32> = wanted.iter().map(|identity| identity.pid).collect();
    let mut found = BTreeSet::new();
    let mut saw_a_process = false;
    for entry in fs::read_dir(proc).ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            // Gone since the folder was listed.
            continue;
        };
        saw_a_process = true;
        // One value per pid namespace, outermost first. A kernel too old to
        // say has no namespaces to tell apart.
        let innermost = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))
            .and_then(|values| values.split_whitespace().last())
            .and_then(|value| value.parse().ok())
            .unwrap_or(pid);
        if !pids.contains(&innermost) {
            continue;
        }
        if let Some(stat) = stat(proc, pid) {
            let identity = Identity {
                pid: innermost,
                start: stat.start,
            };
            if wanted.contains(&identity) {
                found.insert(identity);
            }
        }
    }
    saw_a_process.then_some(found)
}

#[cfg(test)]
pub(crate) mod fake {
    use std::fs;
    use std::path::Path;

    /// Put a process in a stand-in for `/proc`: `pids` are its pid in each
    /// namespace, outermost first, and the folder is named for the first.
    pub(crate) fn process(proc: &Path, pids: &[u32], name: &str, parent: u32, start: u64) {
        let dir = proc.join(pids[0].to_string());
        fs::create_dir_all(&dir).unwrap();
        let nspid: Vec<String> = pids.iter().map(u32::to_string).collect();
        fs::write(
            dir.join("status"),
            format!(
                "Name:\t{name}\nState:\tS (sleeping)\nPid:\t{}\nNSpid:\t{}\n",
                pids[0],
                nspid.join("\t")
            ),
        )
        .unwrap();
        // Fields 3 to 21 are filler; the start time is field 22.
        let filler = vec!["0"; 17].join(" ");
        fs::write(
            dir.join("stat"),
            format!("{} ({name}) S {parent} {filler} {start} 0 0\n", pids[0]),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fake::process;
    use super::*;

    fn set(identities: &[(u32, u64)]) -> BTreeSet<Identity> {
        identities
            .iter()
            .map(|&(pid, start)| Identity { pid, start })
            .collect()
    }

    #[test]
    fn a_hooks_agent_is_the_first_process_above_it_that_is_not_a_shell() {
        let proc = tempfile::tempdir().unwrap();
        process(proc.path(), &[40], "node", 1, 1_000);
        process(proc.path(), &[41], "bash", 40, 1_100);
        process(proc.path(), &[42], "sh", 41, 1_200);
        assert_eq!(
            agent_of(proc.path(), 42),
            Some(Identity {
                pid: 40,
                start: 1_000
            })
        );
        // A hook started straight from the agent.
        assert_eq!(agent_of(proc.path(), 40).map(|agent| agent.pid), Some(40));
    }

    #[test]
    fn a_hook_with_only_shells_above_it_or_none_readable_has_no_agent() {
        let proc = tempfile::tempdir().unwrap();
        process(proc.path(), &[41], "bash", 1, 1_100);
        process(proc.path(), &[42], "sh", 41, 1_200);
        assert_eq!(agent_of(proc.path(), 42), None);
        assert_eq!(agent_of(proc.path(), 99), None);
    }

    #[test]
    fn a_name_with_spaces_and_brackets_is_read_whole() {
        let proc = tempfile::tempdir().unwrap();
        process(proc.path(), &[40], "my (agent) 2", 1, 7_000);
        assert_eq!(
            agent_of(proc.path(), 40),
            Some(Identity {
                pid: 40,
                start: 7_000
            })
        );
    }

    #[test]
    fn a_process_is_found_by_its_pid_inside_its_container_and_its_start_time() {
        let proc = tempfile::tempdir().unwrap();
        // On the host as 5001, in its container as 40.
        process(proc.path(), &[5001, 40], "node", 1, 1_000);
        // Another container's 41, and the host's own 41.
        process(proc.path(), &[6001, 41], "node", 1, 2_000);
        process(proc.path(), &[41], "node", 1, 3_000);
        let found = running(
            proc.path(),
            &set(&[(40, 1_000), (41, 3_000), (41, 9_000), (5001, 1_000)]),
        );
        // 41 started at 9000 is a pid used again; 5001 is no hook's pid.
        assert_eq!(found, Some(set(&[(40, 1_000), (41, 3_000)])));
    }

    #[test]
    fn a_kernel_that_gives_no_namespaces_is_read_by_the_folder_name() {
        let proc = tempfile::tempdir().unwrap();
        process(proc.path(), &[40], "node", 1, 1_000);
        let status = proc.path().join("40/status");
        let without: String = fs::read_to_string(&status)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("NSpid:"))
            .map(|line| format!("{line}\n"))
            .collect();
        fs::write(&status, without).unwrap();
        assert_eq!(
            running(proc.path(), &set(&[(40, 1_000)])),
            Some(set(&[(40, 1_000)]))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_found_in_the_real_proc() {
        let proc = Path::new("/proc");
        let me = agent_of(proc, std::process::id()).unwrap();
        assert_eq!(me.pid, std::process::id());
        assert_eq!(
            running(proc, &BTreeSet::from([me])),
            Some(BTreeSet::from([me]))
        );
        let later = Identity {
            start: me.start + 1,
            ..me
        };
        assert_eq!(
            running(proc, &BTreeSet::from([later])),
            Some(BTreeSet::new())
        );
    }

    #[test]
    fn nothing_can_be_told_from_a_proc_that_shows_no_process_or_hides_some() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(running(empty.path(), &set(&[(40, 1_000)])), None);
        assert_eq!(
            running(&empty.path().join("absent"), &set(&[(40, 1_000)])),
            None
        );
        let hidden = tempfile::tempdir().unwrap();
        process(hidden.path(), &[7], "nucleated", 1, 500);
        fs::write(
            hidden.path().join("mounts"),
            "sysfs /sys sysfs rw 0 0\nproc /proc proc rw,nosuid,hidepid=invisible 0 0\n",
        )
        .unwrap();
        assert_eq!(running(hidden.path(), &set(&[(40, 1_000)])), None);
        fs::write(
            hidden.path().join("mounts"),
            "proc /proc proc rw,nosuid,hidepid=0 0 0\n",
        )
        .unwrap();
        assert_eq!(running(hidden.path(), &set(&[(40, 1_000)])), Some(set(&[])));
    }
}
