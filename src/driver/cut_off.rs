//! Ending, from the box, a look the host gave up on.
//!
//! A look runs in its container through `podman exec`. When the host's
//! deadline passes first, the daemon stops its `podman exec`, but that is
//! only the client: the look itself runs under the container engine's
//! monitor, outside anything the daemon started, and the container can hold
//! it where it is, stopped or deaf to every signal it may ignore. Asking the
//! container to end it would be one more program the container can hold the
//! same way. So the daemon ends it from the box: a container runs as the
//! box's agent user, the daemon's own, so the daemon can kill the look's
//! process by its id on the box with `SIGKILL`, which nothing can block.
//! Its `podman exec`, its command ended, then returns by itself, and the
//! engine forgets the exec.
//!
//! A process is taken for the look only when it is in the container's own
//! process namespace and its command line is the box's own program, then
//! `look`, then the look's token. Then everything it started, however deep.

use std::path::Path;

/// As much of a process's command line as is read: the look's program, the
/// word `look` and its token come well inside this.
#[cfg(target_os = "linux")]
const COMMAND_LINE: u64 = 8 * 1024;
/// As much of a process's `stat` as is read.
#[cfg(target_os = "linux")]
const STAT: u64 = 1024;

/// End the look `program` started with `token` in the container whose first
/// process is `first` on the box, with everything it started.
#[cfg(target_os = "linux")]
pub fn end(first: u32, program: &Path, token: &str) {
    use std::collections::BTreeSet;
    if first == 0 || !super::inside::valid_token(token) {
        return;
    }
    let Some(theirs) = namespace(first) else {
        return;
    };
    let mut doomed: BTreeSet<u32> = BTreeSet::new();
    let mut held: BTreeSet<u32> = BTreeSet::new();
    // Held still first, so that none of them starts another meanwhile. A
    // second pass finds what one of them started before it was held.
    for _ in 0..2 {
        let processes = processes();
        doomed.extend(
            processes
                .iter()
                .filter(|process| {
                    process.is_look(program, token)
                        && namespace(process.pid).as_ref() == Some(&theirs)
                })
                .map(|process| process.pid),
        );
        loop {
            let more: Vec<u32> = processes
                .iter()
                .filter(|p| doomed.contains(&p.parent) && !doomed.contains(&p.pid))
                .map(|p| p.pid)
                .collect();
            if more.is_empty() {
                break;
            }
            doomed.extend(more);
        }
        let fresh: Vec<u32> = doomed.difference(&held).copied().collect();
        for pid in fresh {
            signal(pid, libc::SIGSTOP);
            held.insert(pid);
        }
    }
    for pid in &doomed {
        signal(*pid, libc::SIGKILL);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn end(_first: u32, _program: &Path, _token: &str) {
    // Boxes are Linux. Elsewhere there is no container to end a look in.
}

#[cfg(target_os = "linux")]
struct Process {
    pid: u32,
    parent: u32,
    args: Vec<Vec<u8>>,
}

#[cfg(target_os = "linux")]
impl Process {
    fn is_look(&self, program: &Path, token: &str) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let args = &self.args;
        args.len() >= 4
            && args[0] == program.as_os_str().as_bytes()
            && args[1] == b"look"
            && args
                .windows(2)
                .any(|pair| pair[0] == b"--token" && pair[1] == token.as_bytes())
    }
}

/// Every process on the box: its id, its parent's, and the start of its
/// command line.
#[cfg(target_os = "linux")]
fn processes() -> Vec<Process> {
    let mut found = Vec::new();
    let own = std::process::id();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        let stat = read_capped(&entry.path().join("stat"), STAT);
        let stat = String::from_utf8_lossy(&stat);
        // `pid (name) state ppid ...`: the name can hold anything, so the
        // fields are counted from its last closing bracket.
        let Some(parent) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|ppid| ppid.parse::<u32>().ok())
        else {
            continue;
        };
        let line = read_capped(&entry.path().join("cmdline"), COMMAND_LINE);
        let args = line.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();
        found.push(Process { pid, parent, args });
    }
    found
}

#[cfg(target_os = "linux")]
fn read_capped(path: &Path, cap: u64) -> Vec<u8> {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(cap).read_to_end(&mut bytes);
    }
    bytes
}

/// The process namespace `pid` is in, as the box names it.
#[cfg(target_os = "linux")]
fn namespace(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/ns/pid")).ok()
}

#[cfg(target_os = "linux")]
fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: `kill` takes no pointers. Each id was read from the box's
    // process list a moment ago.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn process(args: &[&str]) -> Process {
        Process {
            pid: 2,
            parent: 1,
            args: args.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
        }
    }

    #[test]
    fn a_look_is_the_boxs_program_then_look_then_its_token() {
        let program = Path::new("/opt/nucarium/bin/nucleated");
        let token = "00112233445566ff";
        let look = |args: &[&str]| process(args).is_look(program, token);
        let ours = [
            "/opt/nucarium/bin/nucleated",
            "look",
            "--env",
            "orca",
            "--token",
            token,
        ];
        assert!(look(&ours));
        // Another look's token, another program, the client that started it,
        // or the token where it is not the token.
        let mut other = ours;
        other[5] = "00112233445566aa";
        assert!(!look(&other));
        let mut elsewhere = ours;
        elsewhere[0] = "/tmp/nucleated";
        assert!(!look(&elsewhere));
        assert!(!look(&[
            "podman",
            "exec",
            "agent-env-client-a",
            "/opt/nucarium/bin/nucleated",
            "look",
            "--token",
            token,
        ]));
        assert!(!look(&[
            "/opt/nucarium/bin/nucleated",
            "look",
            "--env",
            token,
        ]));
        assert!(!look(&["/opt/nucarium/bin/nucleated", "look"]));
    }
}
