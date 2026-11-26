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
//! process on the box with `SIGKILL`, which nothing can block.
//! Its `podman exec`, its command ended, then returns by itself, and the
//! engine forgets the exec.
//!
//! A process is taken for the look only when it is in the container's own
//! process namespace and its command line is the box's own program, then
//! `look`, then the look's token. Then everything it started, however deep,
//! in that namespace or one made inside it.
//!
//! A process id names whatever holds it now, and an id freed is soon given
//! to another process: another environment's monitor, say, or anything else
//! the agent user runs. So each process is held, as soon as it is found, by
//! a descriptor of its own (a pidfd), which names that one process for as
//! long as it is held, and only once it is known to be the very process
//! that was found: its start time, read before and after the descriptor is
//! opened, is the one it was found with, and it is in the container's
//! namespace. Every signal goes through that descriptor, never by id. Where
//! the box cannot give such a descriptor, nothing is signalled.

use std::path::Path;

/// As much of a process's command line as is read: the look's program, the
/// word `look` and its token come well inside this.
#[cfg(target_os = "linux")]
const COMMAND_LINE: u64 = 8 * 1024;
/// As much of a process's `stat` as is read.
#[cfg(target_os = "linux")]
const STAT: u64 = 1024;
/// The most process namespaces, one inside another, followed out of a
/// process's own on the way to the container's.
#[cfg(target_os = "linux")]
const NESTING: usize = 32;

/// End the look `program` started with `token` in the container whose first
/// process is `first` on the box, with everything it started.
#[cfg(target_os = "linux")]
pub fn end(first: u32, program: &Path, token: &str) {
    use std::collections::{BTreeMap, BTreeSet};
    if first == 0 || !super::inside::valid_token(token) {
        return;
    }
    let Some(theirs) = namespace(first) else {
        return;
    };
    let mut held: BTreeMap<u32, Held> = BTreeMap::new();
    // Held still first, so that none of them starts another meanwhile. A
    // second pass finds what one of them started before it was held.
    for _ in 0..2 {
        let processes = processes();
        let started: BTreeMap<u32, u64> = processes.iter().map(|p| (p.pid, p.start)).collect();
        // The look itself is in the container's own namespace.
        let looks: Vec<&Process> = processes
            .iter()
            .filter(|process| {
                !held.contains_key(&process.pid)
                    && process.is_look(program, token)
                    && namespace(process.pid) == Some(theirs)
            })
            .collect();
        for process in looks {
            if let Some(one) = Held::open(process.pid, process.start, |pid| {
                namespace(pid) == Some(theirs)
            }) {
                one.signal(libc::SIGSTOP);
                held.insert(process.pid, one);
            }
        }
        // Those found that could not be held, which are not tried again.
        let mut passed: BTreeSet<u32> = BTreeSet::new();
        loop {
            // A child of a process held: its parent's id is one held, and
            // that id is still the process held, by its start time.
            let children: Vec<&Process> = processes
                .iter()
                .filter(|p| !held.contains_key(&p.pid) && !passed.contains(&p.pid))
                .filter(|p| {
                    held.get(&p.parent)
                        .is_some_and(|parent| started.get(&p.parent) == Some(&parent.start))
                })
                .collect();
            if children.is_empty() {
                break;
            }
            for process in children {
                match Held::open(process.pid, process.start, |pid| in_or_under(pid, theirs)) {
                    Some(one) => {
                        one.signal(libc::SIGSTOP);
                        held.insert(process.pid, one);
                    }
                    None => {
                        passed.insert(process.pid);
                    }
                }
            }
        }
    }
    for one in held.values() {
        one.signal(libc::SIGKILL);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn end(_first: u32, _program: &Path, _token: &str) {
    // Boxes are Linux. Elsewhere there is no container to end a look in,
    // and no pidfd to end one safely with.
}

#[cfg(target_os = "linux")]
struct Process {
    pid: u32,
    parent: u32,
    /// When it started, in clock ticks since the box booted.
    start: u64,
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

/// One process, held by a descriptor that names it and no other.
#[cfg(target_os = "linux")]
struct Held {
    fd: std::os::fd::OwnedFd,
    start: u64,
}

#[cfg(target_os = "linux")]
impl Held {
    /// Hold the process that has `pid` now, only if it is the one that
    /// started at `start` and `belongs` says it is the container's. Its
    /// start time is read before and after its descriptor is opened: the
    /// same both times, the descriptor is of that process.
    fn open(pid: u32, start: u64, belongs: impl Fn(u32) -> bool) -> Option<Held> {
        use std::os::fd::FromRawFd;
        if stat(pid)?.start != start {
            return None;
        }
        // SAFETY: `pidfd_open` takes a process id and flags, no pointers.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `pidfd_open` just returned this descriptor, and nothing
        // else owns it.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as libc::c_int) };
        if stat(pid)?.start != start || !belongs(pid) {
            return None;
        }
        Some(Held { fd, start })
    }

    fn signal(&self, signal: libc::c_int) {
        use std::os::fd::AsRawFd;
        // SAFETY: a pidfd this holds, a signal number, no info and no flags.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

/// A process's parent and start time, from its `stat`.
#[cfg(target_os = "linux")]
struct Stat {
    parent: u32,
    start: u64,
}

#[cfg(target_os = "linux")]
fn stat(pid: u32) -> Option<Stat> {
    let stat = read_capped(Path::new(&format!("/proc/{pid}/stat")), STAT);
    let stat = String::from_utf8_lossy(&stat);
    // `pid (name) state ppid ...`: the name can hold anything, so the
    // fields are counted from its last closing bracket. The third field,
    // the state, is the first after it; the start time is the 22nd.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(Stat {
        parent: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

/// Every process on the box: its id, its parent's, when it started, and
/// the start of its command line.
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
        let Some(Stat { parent, start }) = stat(pid) else {
            continue;
        };
        let line = read_capped(&entry.path().join("cmdline"), COMMAND_LINE);
        let args = line.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();
        found.push(Process {
            pid,
            parent,
            start,
            args,
        });
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

/// A process namespace, as the box names it: the device and inode of its
/// file.
#[cfg(target_os = "linux")]
type Namespace = (u64, u64);

#[cfg(target_os = "linux")]
fn identify(file: &std::fs::File) -> Option<Namespace> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|meta| (meta.dev(), meta.ino()))
}

/// The process namespace `pid` is in.
#[cfg(target_os = "linux")]
fn namespace(pid: u32) -> Option<Namespace> {
    identify(&std::fs::File::open(format!("/proc/{pid}/ns/pid")).ok()?)
}

/// Whether `pid` is in the namespace `theirs`, or in one made inside it:
/// what a look starts may make a namespace of its own, and is still the
/// look's.
#[cfg(target_os = "linux")]
fn in_or_under(pid: u32, theirs: Namespace) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd};
    /// `NS_GET_PARENT`, from `linux/nsfs.h`: `_IO(0xb7, 0x2)`.
    const NS_GET_PARENT: libc::c_ulong = 0xb702;
    let Ok(mut file) = std::fs::File::open(format!("/proc/{pid}/ns/pid")) else {
        return false;
    };
    for _ in 0..NESTING {
        if identify(&file) == Some(theirs) {
            return true;
        }
        // SAFETY: an ioctl that takes no argument, on a namespace file this
        // owns; it returns a new descriptor or fails.
        let parent = unsafe { libc::ioctl(file.as_raw_fd(), NS_GET_PARENT as _) };
        if parent < 0 {
            return false;
        }
        // SAFETY: the ioctl just returned this descriptor, and nothing else
        // owns it.
        file = unsafe { std::fs::File::from_raw_fd(parent) };
    }
    false
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn process(args: &[&str]) -> Process {
        Process {
            pid: 2,
            parent: 1,
            start: 1,
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

    /// A child that sleeps, and its state letter as the box tells it.
    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap()
    }

    fn state(pid: u32) -> char {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat.rsplit_once(')')
            .unwrap()
            .1
            .trim()
            .chars()
            .next()
            .unwrap()
    }

    #[test]
    fn a_process_whose_start_time_no_longer_matches_is_not_held_or_signalled() {
        let mut child = sleeper();
        let pid = child.id();
        let start = stat(pid).unwrap().start;
        let ours = namespace(pid).unwrap();
        // Found with another start time: the id is another process's now.
        assert!(Held::open(pid, start + 1, |pid| in_or_under(pid, ours)).is_none());
        // Not the container's.
        assert!(Held::open(pid, start, |_| false).is_none());
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(state(pid), 'S', "it was signalled");
        // The process found is held, and a signal reaches it.
        let held = Held::open(pid, start, |pid| in_or_under(pid, ours)).unwrap();
        held.signal(libc::SIGSTOP);
        let mut stopped = false;
        for _ in 0..50 {
            if state(pid) == 'T' {
                stopped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(stopped, "a held process was not stopped");
        held.signal(libc::SIGKILL);
        let _ = child.wait();
    }

    #[test]
    fn a_process_gone_before_it_is_held_is_not_signalled() {
        let mut child = sleeper();
        let pid = child.id();
        let start = stat(pid).unwrap().start;
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(Held::open(pid, start, |_| true).is_none());
    }

    #[test]
    fn a_process_is_in_its_own_namespace_and_not_in_a_stranger() {
        let ours = namespace(std::process::id()).unwrap();
        assert!(in_or_under(std::process::id(), ours));
        assert!(!in_or_under(std::process::id(), (0, 0)));
    }
}
