//! herdr-core, the server behind the Herdr desktop and phone apps.
//!
//! Everything that knows how a herdr-core socket is asked, and what its
//! answer looks like, is in this file, with the few things the Herdr TUI
//! adapter shares: the two request lines and how a socket is asked one of
//! them. An answer and every object in it is read through
//! [`allowlist::Nested`], which takes an object or nothing: a derived struct
//! alone would also take an array, one value after another in the order of
//! its fields, and so read a title as a state.
//!
//! herdr-core answers `terminal.list` on a Unix socket of the agent user's
//! own, one JSON-RPC message per line (herdr-core `schema/README.md` and
//! `schema/hc.openrpc.json`, read at 0.1.21). Each terminal carries the word
//! its program last reported for itself with OSC 7501; core stores what the
//! program says and guesses nothing, so a program that reports no status is
//! not an agent here and is not listed.
//!
//! The socket is a control socket: the same connection could run a command,
//! type into a terminal or read a screen, and herdr-core offers no read-only
//! way in. This adapter writes one fixed line to it, [`TERMINAL_LIST`], and
//! nothing else, ever. The answer carries titles, messages, command lines,
//! labels and the names of other machines beside the state. They are skipped
//! unread: the structs below name the only fields this program takes.

use super::{Condition, Driver, HookProcess, Look, Memory, Observed, Phase};
use crate::allowlist::{self, flag, number, short_string, Nested};
use crate::paths;
use crate::settings::Environment;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant, SystemTime};

// ---- what is asked ----------------------------------------------------------

/// Every byte this program ever writes to a herdr-core socket: the one read
/// call, as herdr-core's `schema/README.md` writes it.
pub(super) const TERMINAL_LIST: &str =
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"terminal.list\",\"params\":{}}\n";
/// Every byte this program ever writes to a Herdr TUI socket: the one read
/// call, in the shape of the TUI's `socket-api` page.
pub(super) const AGENT_LIST: &str = "{\"id\":\"n1\",\"method\":\"agent.list\",\"params\":{}}\n";

/// The calls there are. A socket is asked through this and no other way, so
/// no other line can be written to one.
#[derive(Clone, Copy)]
pub(super) enum Call {
    TerminalList,
    AgentList,
}

impl Call {
    fn line(self) -> &'static str {
        match self {
            Call::TerminalList => TERMINAL_LIST,
            Call::AgentList => AGENT_LIST,
        }
    }
}

/// A socket is connected to within this long.
const CONNECT: Duration = Duration::from_secs(2);
/// Once connected, the call is written and its answer read within this long.
/// Both together take no longer than [`super::ASK_LIMIT`].
const ANSWER: Duration = Duration::from_secs(2);
/// An answer is one line no longer than this. A box's terminals fit many
/// times over; what does not fit is not an answer.
const MAX_ANSWER: u64 = 4 * 1024 * 1024;

/// Sockets a connect is still waiting on. Connecting to a local socket is
/// instant unless its server has stopped taking connections, and then it can
/// wait for ever: such a wait is left behind on a thread of its own, and no
/// second one is started for the same socket while it lasts.
static WAITING: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

fn waiting() -> std::sync::MutexGuard<'static, BTreeSet<PathBuf>> {
    WAITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn connect(socket: &Path) -> Result<UnixStream, Condition> {
    if !waiting().insert(socket.to_path_buf()) {
        return Err(Condition::Timeout);
    }
    let (done, connected) = mpsc::channel();
    let path = socket.to_path_buf();
    let started = std::thread::Builder::new()
        .name("herdr-connect".into())
        .spawn(move || {
            let stream = UnixStream::connect(&path);
            waiting().remove(&path);
            let _ = done.send(stream);
        });
    if started.is_err() {
        waiting().remove(socket);
        return Err(Condition::Unreachable);
    }
    match connected.recv_timeout(CONNECT) {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(match error.kind() {
            ErrorKind::NotFound => Condition::Absent,
            ErrorKind::PermissionDenied => Condition::Rejected,
            ErrorKind::TimedOut | ErrorKind::WouldBlock => Condition::Timeout,
            _ => Condition::Unreachable,
        }),
        Err(_) => Err(Condition::Timeout),
    }
}

/// A stream that stops being read at a moment, however slowly it drips.
struct Until<'a> {
    stream: &'a UnixStream,
    at: Instant,
    late: &'a Cell<bool>,
}

impl Read for Until<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            self.late.set(true);
            return Err(ErrorKind::TimedOut.into());
        }
        // A socket whose other end has already answered and hung up may
        // refuse a new time limit (macOS does). What it sent is still there
        // to read, and the limit set when the call was written still holds.
        let _ = self.stream.set_read_timeout(Some(left));
        self.stream.read(buffer).inspect_err(|error| {
            if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
                self.late.set(true);
            }
        })
    }
}

/// Ask a Herdr socket its one read call and read the one line it answers
/// with into an allowlist struct. The connection is then closed. Nothing is
/// kept of an answer that is late, too long or not the shape `T` names.
pub(super) fn ask<T: DeserializeOwned>(socket: &Path, call: Call) -> Result<T, Condition> {
    let mut stream = connect(socket)?;
    let until = Instant::now() + ANSWER;
    let sent = stream
        .set_write_timeout(Some(ANSWER))
        .and_then(|()| stream.set_read_timeout(Some(ANSWER)))
        .and_then(|()| stream.write_all(call.line().as_bytes()));
    if let Err(error) = sent {
        return Err(match error.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => Condition::Timeout,
            _ => Condition::Unreachable,
        });
    }
    let late = Cell::new(false);
    let mut answer = BufReader::with_capacity(
        64 * 1024,
        Until {
            stream: &stream,
            at: until,
            late: &late,
        },
    );
    match allowlist::read_line::<Nested<T>>(&mut answer, MAX_ANSWER) {
        Some(Nested::Is(answer)) => Ok(answer),
        _ if late.get() => Err(Condition::Timeout),
        _ => Err(Condition::BadReply),
    }
}

/// True for two names of one socket.
pub(super) fn same_socket(one: &Path, other: &Path) -> bool {
    one == other
        || one
            .canonicalize()
            .is_ok_and(|one| other.canonicalize().is_ok_and(|other| one == other))
}

/// A folder as a driver's answer gives it, when it is one: used on the box
/// to find a repo name and a branch, and never stored or sent.
pub(super) fn folder(first: Option<&String>, second: Option<&String>) -> Option<PathBuf> {
    first
        .into_iter()
        .chain(second)
        .find(|path| path.starts_with('/'))
        .map(PathBuf::from)
}

/// One named variable of this process, when it is set to something.
pub(super) fn own_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

// ---- herdr-core's answer ----------------------------------------------------

/// One line from the socket: a JSON-RPC answer.
#[derive(Deserialize)]
struct Reply {
    /// Whether the answer is an error. What the error says is not read.
    #[serde(default, deserialize_with = "allowlist::present")]
    error: bool,
    #[serde(default)]
    result: Nested<Listing>,
}

/// `ListResult`. `machines`, the other machines this core knows by name, and
/// `hidden_finished` are not read.
#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    terminals: Option<Vec<Nested<Terminal>>>,
}

/// The fields of a `Terminal` this program takes. Every other key of the
/// record is skipped unread: name, argv, title, labels, template,
/// first_message, identity, parent, records, reason and the rest.
#[derive(Deserialize)]
struct Terminal {
    /// `t7`: "this terminal forever on this core".
    #[serde(default, deserialize_with = "short_string")]
    address: Option<String>,
    /// "live" or "finished".
    #[serde(default, deserialize_with = "short_string")]
    state: Option<String>,
    /// "Whether the program ever sent a 7501 report."
    #[serde(default, deserialize_with = "flag")]
    reported: Option<bool>,
    /// "Goes up each time the status leaves `working`."
    #[serde(default, deserialize_with = "number")]
    turn: Option<u64>,
    #[serde(default, deserialize_with = "number")]
    created_ms: Option<u64>,
    /// The program's own word for what it is doing, "null if it never said,
    /// or after it exited".
    #[serde(default)]
    status: Nested<Status>,
    #[serde(default)]
    foreground: Nested<Foreground>,
    /// Box only.
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
}

/// One 7501 record, less its `app`, `title`, `msg` and `progress`.
#[derive(Deserialize)]
struct Status {
    /// "idle, working, done, blocked or error."
    #[serde(default, deserialize_with = "short_string")]
    state: Option<String>,
    /// "permission, question or auth (blocked only, when the program said)."
    #[serde(default, deserialize_with = "short_string")]
    kind: Option<String>,
}

/// What runs in a terminal now, less its name and command line.
#[derive(Deserialize)]
struct Foreground {
    /// Box only.
    #[serde(default, deserialize_with = "short_string")]
    cwd: Option<String>,
}

/// The digits herdr-core counts terminals in (`terminals.rs`, `SYMBOLS`).
const ADDRESS_DIGITS: &[u8] = b"0123456789bcdfghjkmnpqrstvwxyz";

/// A terminal of this core, as herdr-core's own `id_shaped` has it: `t`, a
/// digit from 1 to 9, then more of its digits. A terminal of another machine
/// is `machine/t7`, does not fit, and is never reported: its name would name
/// a machine other than this box.
fn local_address(address: &str) -> bool {
    let Some(rest) = address.strip_prefix('t') else {
        return false;
    };
    (1..=12).contains(&rest.len())
        && matches!(rest.as_bytes()[0], b'1'..=b'9')
        && rest.bytes().all(|digit| ADDRESS_DIGITS.contains(&digit))
}

/// What a terminal's status says its agent is doing. Nothing for a word
/// this file does not know: the session then keeps the phase it had.
fn phase(status: &Status) -> Option<Phase> {
    match status.state.as_deref()? {
        // The two differ only in whether a person has looked since.
        "idle" | "done" => Some(Phase::Idle),
        "working" => Some(Phase::Working),
        "blocked" => Some(match status.kind.as_deref() {
            Some("permission") => Phase::NeedsPermission,
            // A question, a login to renew, or no kind given.
            _ => Phase::NeedsInput,
        }),
        // herdr-core gives the reason only as the program's free text, which
        // is not read: a failure is reported with no kind.
        "error" => Some(Phase::Failed),
        _ => None,
    }
}

/// One terminal as a session, or nothing for a terminal that is not an
/// agent of this core.
fn observed(terminal: &Terminal) -> Option<Observed> {
    let address = terminal.address.as_deref().filter(|a| local_address(a))?;
    let created = terminal.created_ms?;
    let phase = match (terminal.state.as_deref(), &terminal.status) {
        (Some("finished"), _) => Some(Phase::Ended),
        (Some("live"), Nested::Is(status)) => phase(status),
        // Status is cleared when the program ends or its shell comes back.
        (Some("live"), Nested::Absent) if terminal.reported == Some(true) => Some(Phase::Ended),
        // A program that never reported a status is not an agent here.
        (Some("live"), Nested::Absent) => return None,
        _ => None,
    };
    let mut session = Observed::new(format!("hc:{address}:{created}"), phase);
    session.turn = terminal.turn;
    session.cwd = folder(
        terminal.foreground.get().and_then(|f| f.cwd.as_ref()),
        terminal.cwd.as_ref(),
    );
    session.claim_key = Some(address.to_string());
    Some(session)
}

// ---- where the socket is ----------------------------------------------------

/// Where herdr-core puts a session's socket when nothing says otherwise
/// (`docs/security.md`, "State and endpoints"; `paths.rs`; `platform/unix.rs`):
/// under the user's runtime folder, else beside the session's state.
fn default_socket(
    session: &str,
    runtime: Option<PathBuf>,
    state_home: Option<PathBuf>,
) -> Option<PathBuf> {
    let name = format!("{session}.sock");
    match runtime {
        Some(runtime) => Some(runtime.join("herdr-core").join(name)),
        None => Some(state_home?.join("herdr-core").join(session).join(name)),
    }
}

/// The socket an environment's herdr-core is asked on: the config's, else
/// where herdr-core itself would put it. herdr-core's own `HC_SOCKET` and
/// config file are not read; an owner who set either says so in
/// `driver_socket`.
fn socket(environment: &Environment) -> Option<PathBuf> {
    let driver = environment.driver.as_ref()?;
    if let Some(socket) = &driver.socket {
        return Some(socket.clone());
    }
    let runtime = own_var("XDG_RUNTIME_DIR").map(PathBuf::from).or_else(|| {
        // SAFETY: getuid takes nothing, changes nothing and cannot fail.
        let user = unsafe { libc::getuid() };
        Some(PathBuf::from(format!("/run/user/{user}"))).filter(|dir| dir.is_dir())
    });
    let state_home = own_var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(paths::home()?.join(".local/state")));
    default_socket(
        driver.session.as_deref().unwrap_or("default"),
        runtime,
        state_home,
    )
}

pub struct HerdrCore;

impl Driver for HerdrCore {
    fn name(&self) -> &'static str {
        "herdr-core"
    }

    fn keys(&self) -> &'static [&'static str] {
        &["driver_socket", "driver_session"]
    }

    fn default_poll(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn min_poll(&self) -> Duration {
        Duration::from_secs(2)
    }

    fn observe(
        &self,
        environment: &Environment,
        _previous: &Memory,
        _now: SystemTime,
    ) -> Result<Look, Condition> {
        let socket = socket(environment).ok_or(Condition::Absent)?;
        let reply: Reply = ask(&socket, Call::TerminalList)?;
        // herdr-core answered the call with an error. What it says is not
        // read, so nothing more is known than that.
        if reply.error {
            return Err(Condition::Error);
        }
        let Nested::Is(Listing {
            terminals: Some(terminals),
        }) = reply.result
        else {
            return Err(Condition::BadReply);
        };
        Ok(Look {
            sessions: terminals
                .iter()
                .filter_map(|terminal| observed(terminal.get()?))
                .collect(),
            // Without `all` the list leaves out finished terminals and
            // nothing else, so a terminal it does not hold is over.
            complete: true,
            // herdr-core documents no address that opens one terminal.
            link_ref: None,
            ..Look::default()
        })
    }

    /// herdr-core gives each terminal's processes `HC_TERMINAL_ID`, their
    /// terminal's address, and `HC_TERMINAL_SOCKET`, the core it means
    /// something to (`client.rs`, `terminal_id`). An address on another
    /// core's socket is another terminal.
    fn claims(&self, hook: &HookProcess) -> Option<String> {
        let theirs = PathBuf::from((hook.var)("HC_TERMINAL_SOCKET").filter(|s| !s.is_empty())?);
        let address = (hook.var)("HC_TERMINAL_ID").filter(|a| local_address(a))?;
        same_socket(&theirs, &socket(hook.environment)?).then_some(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terminal(value: serde_json::Value) -> Option<Observed> {
        let terminal: Nested<Terminal> = serde_json::from_value(value).unwrap();
        observed(terminal.get()?)
    }

    #[test]
    fn a_terminal_address_is_one_of_this_core_and_never_another_machines() {
        for local in ["t1", "t7", "t10", "t3b", "t9zz"] {
            assert!(local_address(local), "{local}");
        }
        for other in [
            "", "t", "t0", "t07", "T7", "7", "t7a", "t7/", "box/t7", "t7 ", "t-1",
        ] {
            assert!(!local_address(other), "{other}");
        }
        assert!(!local_address(&format!("t{}", "1".repeat(13))));
        let remote = json!({ "address": "box/t7", "state": "live", "created_ms": 1,
            "status": { "state": "working" } });
        assert_eq!(terminal(remote), None);
    }

    #[test]
    fn each_documented_status_word_is_one_phase_and_any_other_is_no_opinion() {
        let said = |state: &str, kind: Option<&str>| {
            terminal(
                json!({ "address": "t7", "state": "live", "created_ms": 5, "reported": true,
                "status": { "state": state, "kind": kind } }),
            )
            .expect("an agent")
            .phase
        };
        assert_eq!(said("idle", None), Some(Phase::Idle));
        assert_eq!(said("done", None), Some(Phase::Idle));
        assert_eq!(said("working", None), Some(Phase::Working));
        assert_eq!(
            said("blocked", Some("permission")),
            Some(Phase::NeedsPermission)
        );
        assert_eq!(said("blocked", Some("question")), Some(Phase::NeedsInput));
        assert_eq!(said("blocked", Some("auth")), Some(Phase::NeedsInput));
        assert_eq!(said("blocked", None), Some(Phase::NeedsInput));
        assert_eq!(said("error", None), Some(Phase::Failed));
        assert_eq!(said("thinking hard", None), None);
        assert_eq!(said("Working", None), None);
    }

    #[test]
    fn a_terminal_whose_program_never_reported_is_not_an_agent() {
        let shell = json!({ "address": "t2", "state": "live", "created_ms": 5,
            "reported": false, "status": null });
        assert_eq!(terminal(shell), None);
        let unsaid = json!({ "address": "t2", "state": "live", "created_ms": 5 });
        assert_eq!(terminal(unsaid), None);
        // One that did report, and whose status is gone, has ended.
        let over = json!({ "address": "t2", "state": "live", "created_ms": 5,
            "reported": true, "status": null });
        assert_eq!(terminal(over).unwrap().phase, Some(Phase::Ended));
        let finished = json!({ "address": "t2", "state": "finished", "created_ms": 5 });
        assert_eq!(terminal(finished).unwrap().phase, Some(Phase::Ended));
    }

    #[test]
    fn a_session_is_named_by_its_address_and_the_time_its_terminal_was_made() {
        let seen = terminal(json!({
            "address": "t7", "state": "live", "created_ms": 1795140000000u64, "turn": 4,
            "status": { "state": "working" },
            "cwd": "/home/agent/started-here",
            "foreground": { "cwd": "/home/agent/now-here", "name": "node" },
        }))
        .unwrap();
        assert_eq!(seen.id, "hc:t7:1795140000000");
        assert_eq!(seen.turn, Some(4));
        assert_eq!(seen.claim_key.as_deref(), Some("t7"));
        assert_eq!(seen.cwd, Some(PathBuf::from("/home/agent/now-here")));
        assert_eq!(seen.activity, None);
        assert_eq!(seen.error_kind, None);
        // With no creation time there is no id to give it.
        let undated = json!({ "address": "t7", "state": "live", "status": { "state": "idle" } });
        assert_eq!(terminal(undated), None);
    }

    #[test]
    fn what_is_not_an_object_is_not_read_as_one() {
        // An array in a record's place would otherwise be read field by
        // field, in order.
        assert_eq!(
            terminal(json!(["t7", "live", true, 1, 5, { "state": "working" }])),
            None
        );
        let odd = terminal(json!({ "address": "t7", "state": "live", "created_ms": 5,
            "status": "working", "foreground": ["/somewhere"] }))
        .unwrap();
        assert_eq!(odd.phase, None);
        assert_eq!(odd.cwd, None);
        let relative = terminal(json!({ "address": "t7", "state": "live", "created_ms": 5,
            "status": { "state": "idle" }, "cwd": "a sentence, not a folder" }))
        .unwrap();
        assert_eq!(relative.cwd, None);
    }

    #[test]
    fn the_socket_is_where_herdr_core_puts_it() {
        assert_eq!(
            default_socket("default", Some("/run/user/1001".into()), Some("/s".into())),
            Some(PathBuf::from("/run/user/1001/herdr-core/default.sock"))
        );
        assert_eq!(
            default_socket("scratch", None, Some("/home/agent/.local/state".into())),
            Some(PathBuf::from(
                "/home/agent/.local/state/herdr-core/scratch/scratch.sock"
            ))
        );
        assert_eq!(default_socket("default", None, None), None);
    }

    #[test]
    fn the_only_lines_there_are_to_send_are_the_two_read_calls() {
        for (call, method) in [
            (Call::TerminalList, "terminal.list"),
            (Call::AgentList, "agent.list"),
        ] {
            let line = call.line();
            assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
            let sent: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(sent["method"], method);
            assert_eq!(sent["params"], json!({}));
        }
    }
}
