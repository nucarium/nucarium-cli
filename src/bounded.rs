//! Running another program without ever waiting on it for long.
//!
//! The daemon asks other programs things: a driver's own command line for
//! its agents, git for a commit. Each such call gets a time limit and a limit
//! on how much it may print, takes nothing on standard input, and is stopped
//! when it runs over. The limit holds whatever the program started: its
//! output is read no longer than its time, even while something it left
//! behind still holds the pipe open. A call that fails, runs over or prints too much is no
//! answer, and the caller then says nothing. Why there was none is one of
//! the fixed words of [`Failure`], for a caller that tells its owner which.
//!
//! Nothing here chooses what is run. A caller runs only the fixed commands
//! its own design names.

use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most of a program's complaints that is kept for the box's own log.
const MAX_STDERR: u64 = 64 * 1024;

/// What a program printed, and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub ok: bool,
    pub stdout: Vec<u8>,
    /// For the box's own log only. It can name paths and quote content, so it
    /// is never stored in the state folder and never sent.
    pub stderr: String,
}

/// Why a call gave no answer. A fixed word and nothing of what the program
/// said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// There was no program to run: not installed, or not this user's to run.
    Absent,
    /// It was stopped for running over its time.
    Late,
    /// It printed more than it may.
    TooMuch,
    /// It ran and exited with a failure. Only [`try_stream`] says this:
    /// [`run`] hands such a program's output back with `ok` unset.
    Failed,
    /// It ran and succeeded, and what it printed was not an answer. Only
    /// [`try_stream`] says this.
    NoAnswer,
}

/// The children started here that have not been waited for yet, by id. Each
/// leads a process group of its own. An id leaves this list before its
/// child is waited for, so an id in it always names this program's child
/// and never a process that came after it.
static LIVE: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn live() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    LIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start a program with nothing on standard input, its output on a pipe, and
/// a process group of its own, so that stopping it stops what it started too.
fn spawn(command: &mut Command, stderr: Stdio) -> Result<Child, Failure> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    let mut started = live();
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .map_err(|_| Failure::Absent)?;
    started.insert(child.id());
    Ok(child)
}

/// Kill a process group, by the id of the child that leads it.
fn kill_group(id: u32) {
    #[cfg(unix)]
    // SAFETY: `kill` takes no pointers. Only called with the id of a child
    // that has not been reaped, so it names that child's own process group
    // and no one else's.
    unsafe {
        libc::kill(-(id as libc::pid_t), libc::SIGKILL);
    }
}

/// Stop a child and everything in its process group. Only ever called for a
/// child that has not been waited for, so the id is still this program's.
fn stop(child: &mut Child) {
    kill_group(child.id());
    let _ = child.kill();
}

/// Wait for a child that has exited or been stopped, taking it off the list
/// of live ones first.
fn wait(child: &mut Child) -> std::io::Result<ExitStatus> {
    live().remove(&child.id());
    child.wait()
}

/// Stop every program started here that is still running, and everything in
/// its process group, at once. For a process that has to end within a time
/// of its own whatever it is in the middle of: `nucleated look` calls this
/// when its time is up, before it leaves.
pub fn stop_all() {
    for id in live().iter() {
        kill_group(*id);
    }
}

/// A child's pipe, read with one eye on the clock. A read waits for bytes
/// only until the deadline, and from then on the pipe reads as ended.
///
/// The end of a pipe comes when every program that holds it has let go, and
/// that is not only the child: anything it started inherits the pipe, and
/// one that has left the child's process group is not stopped with it. So
/// the limit is kept here, by the reader, and not by waiting for an end
/// that stopping the child was supposed to bring.
struct Timed<R> {
    pipe: R,
    deadline: Instant,
    late: Arc<AtomicBool>,
}

impl<R> Timed<R> {
    fn new(pipe: R, deadline: Instant, late: &Arc<AtomicBool>) -> Timed<R> {
        Timed {
            pipe,
            deadline,
            late: late.clone(),
        }
    }
}

#[cfg(unix)]
impl<R: Read + std::os::fd::AsRawFd> Read for Timed<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.late.load(Ordering::SeqCst) {
                self.late.store(true, Ordering::SeqCst);
                return Ok(0);
            }
            let mut waited = libc::pollfd {
                fd: self.pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let wait = left.as_millis().clamp(1, 60_000) as libc::c_int;
            // SAFETY: `poll` is given one descriptor, which `self.pipe`
            // keeps open for as long as this call lasts.
            let ready = unsafe { libc::poll(&mut waited, 1, wait) };
            // Bytes, an end or an error: the read itself says which.
            if ready > 0 {
                return self.pipe.read(buffer);
            }
            let interrupted =
                std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
            if ready < 0 && !interrupted {
                return self.pipe.read(buffer);
            }
        }
    }
}

#[cfg(not(unix))]
impl<R: Read> Read for Timed<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let _ = (self.deadline, &self.late);
        self.pipe.read(buffer)
    }
}

/// Once a child's output has been read to its end, or for as long as its
/// time allowed: wait for it to exit, at most until `deadline`. A child that
/// ran over is stopped, and is late.
fn reap(child: &mut Child, late: &AtomicBool, deadline: Instant) -> Result<ExitStatus, Failure> {
    let late = late.load(Ordering::SeqCst);
    loop {
        match exited(child) {
            Ok(Some(status)) if !late => return Ok(status),
            Ok(None) if !late && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                stop(child);
                let _ = wait(child);
                return Err(Failure::Late);
            }
        }
    }
}

/// Whether a child has exited, reaping it if it has. The list of live
/// children is held meanwhile, so a reaped id is off it before anyone else
/// can read it.
fn exited(child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
    let mut started = live();
    let status = child.try_wait();
    if !matches!(status, Ok(None)) {
        started.remove(&child.id());
    }
    status
}

/// Read a pipe up to `max` bytes into `keep`. True when there was more than
/// `max`: the rest is then left unread, and the caller stops the program, so
/// an answer that never ends costs no more than `max` bytes and no time.
fn take_up_to(pipe: impl Read, max: u64, keep: impl FnOnce(&mut dyn Read)) -> bool {
    let mut limited = pipe.take(max);
    keep(&mut limited);
    // What `keep` did not take of the first `max` bytes is read past, unkept.
    let _ = std::io::copy(&mut limited, &mut std::io::sink());
    let mut rest = limited.into_inner();
    let mut one = [0u8; 1];
    loop {
        match rest.read(&mut one) {
            Ok(0) => return false,
            Ok(_) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// Read a pipe up to `max` bytes into `keep`, then past whatever is left.
/// True when there was more than `max`.
fn drain(pipe: impl Read, max: u64, keep: impl FnOnce(&mut dyn Read)) -> bool {
    let mut limited = pipe.take(max);
    keep(&mut limited);
    // Whatever was not taken is read past, unkept, so the program is never
    // left blocked on a full pipe.
    let _ = std::io::copy(&mut limited, &mut std::io::sink());
    std::io::copy(&mut limited.into_inner(), &mut std::io::sink()).is_ok_and(|more| more > 0)
}

/// Run a program and hand what it prints to `parse` as a stream, so a large
/// answer is read as it is parsed and never held whole. `parse` sees at most
/// `max` bytes. The program's complaints go nowhere.
///
/// Nothing comes back unless the program was there, finished in time, exited
/// 0, printed no more than `max` and `parse` made something of it.
pub fn stream<T>(
    command: &mut Command,
    timeout: Duration,
    max: u64,
    parse: impl FnOnce(&mut dyn Read) -> Option<T>,
) -> Option<T> {
    try_stream(command, timeout, max, parse).ok()
}

/// As [`stream`], saying why when nothing comes back. The first of these
/// that holds is the word: not there, too much, late, failed, no answer.
pub fn try_stream<T>(
    command: &mut Command,
    timeout: Duration,
    max: u64,
    parse: impl FnOnce(&mut dyn Read) -> Option<T>,
) -> Result<T, Failure> {
    let deadline = Instant::now() + timeout;
    let mut child = spawn(command, Stdio::null())?;
    let late = Arc::new(AtomicBool::new(false));
    let mut parsed = None;
    let over = child.stdout.take().is_none_or(|out| {
        take_up_to(Timed::new(out, deadline, &late), max, |limited| {
            parsed = parse(limited)
        })
    });
    if over {
        stop(&mut child);
        let _ = wait(&mut child);
        return Err(Failure::TooMuch);
    }
    let status = reap(&mut child, &late, deadline)?;
    if !status.success() {
        return Err(Failure::Failed);
    }
    parsed.ok_or(Failure::NoAnswer)
}

/// Run a program and keep what it printed, at most `max` bytes of it.
/// Nothing comes back when the program was not there, ran over its time or
/// printed more than `max`. A program that ran and failed does come back,
/// with `ok` unset, so the caller can tell "refused" from "not there".
pub fn output(command: &mut Command, timeout: Duration, max: u64) -> Option<Captured> {
    run(command, timeout, max).ok()
}

/// As [`output`], saying why when nothing comes back: the program was not
/// there, ran over its time, or printed too much.
pub fn run(command: &mut Command, timeout: Duration, max: u64) -> Result<Captured, Failure> {
    let deadline = Instant::now() + timeout;
    let mut child = spawn(command, Stdio::piped())?;
    let late = Arc::new(AtomicBool::new(false));
    // The two pipes are read side by side: a program that fills one while
    // this side waits on the other would never finish.
    let complaints = child.stderr.take().map(|err| {
        let err = Timed::new(err, deadline, &late);
        std::thread::spawn(move || {
            let mut text = Vec::new();
            drain(err, MAX_STDERR, |limited| {
                let _ = limited.read_to_end(&mut text);
            });
            String::from_utf8_lossy(&text).trim().to_string()
        })
    });
    let mut stdout = Vec::new();
    let over = child.stdout.take().is_none_or(|out| {
        take_up_to(Timed::new(out, deadline, &late), max, |limited| {
            let _ = limited.read_to_end(&mut stdout);
        })
    });
    if over {
        stop(&mut child);
    }
    let stderr = complaints
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    if over {
        let _ = wait(&mut child);
        return Err(Failure::TooMuch);
    }
    let status = reap(&mut child, &late, deadline)?;
    Ok(Captured {
        ok: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn text(stream: &mut dyn Read) -> Option<String> {
        let mut text = String::new();
        stream.read_to_string(&mut text).ok()?;
        Some(text)
    }

    #[test]
    fn a_program_that_answers_in_time_is_heard() {
        let limit = Duration::from_secs(5);
        assert_eq!(
            stream(&mut sh("echo state"), limit, 4096, text).as_deref(),
            Some("state\n")
        );
        let captured = output(&mut sh("echo out; echo why >&2"), limit, 4096).unwrap();
        assert!(captured.ok);
        assert_eq!(captured.stdout, b"out\n");
        assert_eq!(captured.stderr, "why");
    }

    #[test]
    fn a_program_that_fails_or_is_not_there_is_no_answer() {
        let limit = Duration::from_secs(5);
        assert_eq!(
            stream(&mut sh("echo state; exit 3"), limit, 4096, text),
            None
        );
        assert_eq!(
            stream(
                &mut Command::new("no-such-driver-command"),
                limit,
                4096,
                text
            ),
            None
        );
        assert_eq!(
            output(&mut Command::new("no-such-driver-command"), limit, 4096),
            None
        );
        // One that ran and refused is told apart from one that was not there.
        let refused = output(&mut sh("echo no >&2; exit 1"), limit, 4096).unwrap();
        assert!(!refused.ok);
        assert_eq!(refused.stderr, "no");
    }

    #[test]
    fn a_program_that_hangs_is_stopped_and_is_no_answer() {
        let started = Instant::now();
        let limit = Duration::from_millis(200);
        assert_eq!(
            stream(&mut sh("echo state; sleep 30"), limit, 4096, text),
            None
        );
        assert_eq!(output(&mut sh("sleep 30"), limit, 4096), None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Not a test of anything: the program the test below leaves behind.
    /// Started with a file named, it moves to a session of its own, out of
    /// the group it was started in, says so in the file, and keeps the pipes
    /// it was given open.
    #[test]
    fn stand_in_for_something_a_program_left_behind() {
        let Some(ready) = std::env::var_os("NUCLEATE_TEST_LEFT_BEHIND") else {
            return;
        };
        let ready = std::path::PathBuf::from(ready);
        // SAFETY: `setsid` takes no pointers.
        let moved = unsafe { libc::setsid() };
        assert!(moved > 0, "could not leave the group");
        let fresh = ready.with_extension("new");
        std::fs::write(&fresh, std::process::id().to_string()).unwrap();
        std::fs::rename(&fresh, &ready).unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    /// A program that answers, leaves something behind outside its own
    /// process group with its pipes still open, and exits 0.
    fn leaving_something_behind(ready: &std::path::Path) -> Command {
        let mut command = sh("NUCLEATE_TEST_LEFT_BEHIND=\"$READY\" \"$SELF\" --exact \
             bounded::tests::stand_in_for_something_a_program_left_behind &\n\
             while [ ! -s \"$READY\" ]; do :; done\necho state");
        command
            .env("READY", ready)
            .env("SELF", std::env::current_exe().unwrap());
        command
    }

    #[test]
    fn the_time_limit_holds_while_something_outside_the_programs_group_keeps_its_pipe_open() {
        let dir = tempfile::tempdir().unwrap();
        let limit = Duration::from_millis(1500);
        let mut left_behind = Vec::new();
        let started = Instant::now();
        for (name, streamed) in [("streamed", true), ("captured", false)] {
            let ready = dir.path().join(name);
            let mut command = leaving_something_behind(&ready);
            if streamed {
                assert_eq!(
                    try_stream(&mut command, limit, 4096, text),
                    Err(Failure::Late)
                );
            } else {
                assert_eq!(run(&mut command, limit, 4096), Err(Failure::Late));
            }
            // It was there, outside the group, for the whole of the wait.
            let pid: libc::pid_t = std::fs::read_to_string(&ready)
                .expect("the program left nothing behind")
                .parse()
                .unwrap();
            left_behind.push(pid);
        }
        let took = started.elapsed();
        for pid in left_behind {
            // SAFETY: `kill` takes no pointers; the id is the stand-in's.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0, "it had gone");
        }
        assert!(took < Duration::from_secs(15), "waited {took:?}");
    }

    #[test]
    fn a_program_that_prints_more_than_its_limit_is_no_answer() {
        let limit = Duration::from_secs(10);
        let flood = "head -c 300000 /dev/zero";
        assert_eq!(stream(&mut sh(flood), limit, 1024, text), None);
        assert_eq!(output(&mut sh(flood), limit, 1024), None);
        // Up to the limit is an answer.
        let whole = output(&mut sh("head -c 1024 /dev/zero"), limit, 1024).unwrap();
        assert_eq!(whole.stdout.len(), 1024);
    }

    #[test]
    fn why_there_was_no_answer_is_one_fixed_word() {
        let limit = Duration::from_secs(5);
        let nothing = |_: &mut dyn Read| None::<String>;
        let mut missing = Command::new("no-such-driver-command");
        assert_eq!(run(&mut missing, limit, 4096), Err(Failure::Absent));
        assert_eq!(
            try_stream(&mut missing, limit, 4096, text),
            Err(Failure::Absent)
        );
        // A program that ran and failed is heard by `run`, and is a failure
        // to `try_stream`, whatever it printed.
        assert!(!run(&mut sh("echo no >&2; exit 1"), limit, 4096).unwrap().ok);
        assert_eq!(
            try_stream(&mut sh("echo state; exit 3"), limit, 4096, text),
            Err(Failure::Failed)
        );
        let short = Duration::from_millis(200);
        assert_eq!(run(&mut sh("sleep 30"), short, 4096), Err(Failure::Late));
        assert_eq!(
            try_stream(&mut sh("echo state; sleep 30"), short, 4096, text),
            Err(Failure::Late)
        );
        let flood = "head -c 300000 /dev/zero";
        assert_eq!(run(&mut sh(flood), limit, 1024), Err(Failure::TooMuch));
        assert_eq!(
            try_stream(&mut sh(flood), limit, 1024, text),
            Err(Failure::TooMuch)
        );
        assert_eq!(
            try_stream(&mut sh("echo words"), limit, 4096, nothing),
            Err(Failure::NoAnswer)
        );
        assert_eq!(
            try_stream(&mut sh("echo state"), limit, 4096, text).as_deref(),
            Ok("state\n")
        );
    }

    #[test]
    fn an_answer_that_never_ends_is_cut_at_its_limit_at_once() {
        let started = Instant::now();
        let limit = Duration::from_secs(20);
        assert_eq!(
            try_stream(&mut sh("yes state"), limit, 4096, text),
            Err(Failure::TooMuch)
        );
        assert_eq!(
            run(&mut sh("yes state"), limit, 4096),
            Err(Failure::TooMuch)
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// The test below, in a process of its own: stopping everything stops
    /// every child of the process, and other tests run their own beside it.
    #[test]
    fn stand_in_for_a_process_whose_time_is_up() {
        let Some(dir) = std::env::var_os("NUCLEATE_TEST_STOP_ALL") else {
            return;
        };
        let pid_file = std::path::PathBuf::from(dir).join("pid");
        let mut command = sh("echo $$ > \"$PID_FILE\"; exec sleep 30");
        command.env("PID_FILE", &pid_file);
        let started = Instant::now();
        let waiting = std::thread::spawn(move || run(&mut command, Duration::from_secs(20), 4096));
        while std::fs::read_to_string(&pid_file).map_or(true, |pid| pid.trim().is_empty()) {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "it never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        stop_all();
        let heard = waiting.join().unwrap().unwrap();
        assert!(!heard.ok);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(live().is_empty());
    }

    #[test]
    fn everything_still_running_is_stopped_at_once_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bounded::tests::stand_in_for_a_process_whose_time_is_up",
            ])
            .env("NUCLEATE_TEST_STOP_ALL", dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let pid: libc::pid_t = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: `kill` takes no pointers; signal 0 only asks.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "it is still running");
    }

    #[test]
    fn nothing_is_given_on_standard_input() {
        let limit = Duration::from_secs(5);
        assert_eq!(
            stream(&mut sh("cat; echo done"), limit, 4096, text).as_deref(),
            Some("done\n")
        );
    }
}
