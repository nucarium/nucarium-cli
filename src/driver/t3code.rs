//! T3 Code.
//!
//! Everything that knows how a T3 Code server is asked, and what its answer
//! looks like, is in this file. The facts below were read from T3 Code's own
//! repository (pingdotgg/t3code, server package 0.1.10, orchestration
//! protocol 2); each names the file it came from. Nothing here is guessed:
//! where the source says nothing, the adapter says nothing.
//!
//! **What is asked.** Two GETs, to this machine and nowhere else:
//!
//! 1. `GET /.well-known/t3/environment`, with no credentials
//!    (packages/contracts/src/environmentHttp.ts, `descriptor`). Asked
//!    before the first look, again after any look that failed, when the
//!    server's address changes, and when the last good look is more than
//!    ten minutes old. What it said is kept between looks as three numbers
//!    and the environment's id.
//! 2. `GET /api/orchestration/shell`, with the owner's read-only bearer token
//!    and the header `x-t3-orchestration-protocol: 2`
//!    (environmentHttp.ts, `shellSnapshot`; environment.ts,
//!    `ORCHESTRATION_PROTOCOL_HEADER`). Asked at every look.
//!
//! There is no other request in this file and no way to make one: no POST,
//! no `/mcp`, no WebSocket, no `t3` program, no thread, transcript or history
//! route, no database file.
//!
//! **What is read.** T3 Code has no route that returns thread state without
//! thread content: the shell answer carries each thread's title, its latest
//! message, its error text and its goal beside the state words. The structs
//! below name the state fields and nothing else, so serde reads past the rest
//! without keeping it.

use super::{loopback_origin, Condition, Driver, HookProcess, Look, Memory, Observed, Phase};
use crate::allowlist::{self, flag, number, object, present, rows, short_string, Rows, Stated};
use crate::envelope::valid_session_id;
use crate::paths;
use crate::settings::Environment;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct T3Code;

/// The only two paths this adapter ever asks for.
const DESCRIPTOR: &str = "/.well-known/t3/environment";
const SHELL: &str = "/api/orchestration/shell";
/// `ORCHESTRATION_PROTOCOL_VERSION` (packages/contracts/src/environment.ts).
/// The shell route answers only a request whose header is this exact number.
const PROTOCOL: u64 = 2;
const PROTOCOL_HEADER: &str = "x-t3-orchestration-protocol";
const PROTOCOL_TEXT: &str = "2";
/// The version of `server-runtime.json` this adapter reads
/// (apps/server/src/serverRuntimeState.ts).
const RUNTIME_VERSION: u64 = 1;
/// A look, both requests together, is given up after this long: inside
/// [`super::LOOK_LIMIT`] with room to spare.
const BUDGET: Duration = Duration::from_secs(4);
const CONNECT: Duration = Duration::from_secs(1);
/// A runtime file or a token longer than this is not one.
const SMALL_FILE: u64 = 64 * 1024;
/// The variable T3 Code sets, to the thread's id, on every agent it launches
/// (apps/server/src/preview/PortScanner.ts, `AGENT_THREAD_ENV`).
const THREAD_VAR: &str = "T3CODE_THREAD_ID";

impl Driver for T3Code {
    fn name(&self) -> &'static str {
        "t3code"
    }

    fn keys(&self) -> &'static [&'static str] {
        &["driver_home", "driver_url", "driver_token_file"]
    }

    /// `t3 service install` installs `t3code.service` as a systemd user unit
    /// (docs/user/background-service.md).
    fn default_unit(&self) -> Option<&'static str> {
        Some("t3code.service")
    }

    fn observe(
        &self,
        environment: &Environment,
        previous: &Memory,
        now: SystemTime,
    ) -> Result<Look, Condition> {
        let deadline = Instant::now() + BUDGET;
        let origin = origin(environment)?;
        // Without the owner's token the second request cannot be made, so
        // the first is not made either.
        let token = token(environment)?;

        // The descriptor says who the server is and what it speaks. It is
        // asked once, and again after any look that did not go well: a
        // failed look leaves a condition that is not `ok`, and then nothing
        // is recalled.
        let server = match recall(previous, &origin, now) {
            Some(server) => server,
            None => describe(&origin, deadline)?,
        };
        shell(&origin, &token, &server, deadline)
    }

    /// "Copy thread copies a link such as
    /// `https://app.t3.codes/threads/<environment>/<thread>`"
    /// (docs/user/prompt-links.md, Share a thread). Each part is
    /// percent-encoded by whoever fills it in, as `hostedSegment` does
    /// (packages/shared/src/threadLinks.ts).
    fn link_template(&self) -> Option<&'static str> {
        Some("https://app.t3.codes/threads/{driver_ref}/{session_id}")
    }

    /// An agent T3 Code launched carries its thread's id in
    /// `T3CODE_THREAD_ID`, and that id is what the thread is listed under.
    fn claims(&self, hook: &HookProcess) -> Option<String> {
        let thread = (hook.var)(THREAD_VAR)?;
        let thread = thread.trim();
        valid_session_id(thread).then(|| thread.to_string())
    }
}

// ---- where the server is, and the owner's token -----------------------------

/// The server's address: the config's, else the one the running server wrote
/// to `<T3 home>/userdata/server-runtime.json`, which T3 Code's own CLI reads
/// to find it (apps/server/src/serverRuntimeState.ts, config.ts). Only an
/// address on this machine is ever used.
fn origin(environment: &Environment) -> Result<String, Condition> {
    let settings = environment.driver.as_ref().ok_or(Condition::Absent)?;
    if let Some(url) = &settings.url {
        return loopback_origin(url).ok_or(Condition::Unsupported);
    }
    let home = match &settings.home {
        Some(home) => home.clone(),
        // "normally `~/.t3`" (docs/user/background-service.md).
        None => paths::home().ok_or(Condition::Absent)?.join(".t3"),
    };
    let file = home.join("userdata").join("server-runtime.json");
    let mut file = File::open(file).map_err(|_| Condition::Absent)?;
    let runtime: Runtime =
        whole(&mut std::io::Read::take(&mut file, SMALL_FILE)).ok_or(Condition::BadReply)?;
    if runtime.version != Some(RUNTIME_VERSION) {
        return Err(Condition::Unsupported);
    }
    // A server told to listen on the network may name another address here.
    // That one is not asked: `driver_url` names the local one instead.
    runtime
        .origin
        .as_deref()
        .and_then(loopback_origin)
        .ok_or(Condition::Unsupported)
}

/// The bearer token the owner issued with `t3 auth session issue --scope
/// orchestration:read` (apps/server/src/cli/auth.ts). A file others can read
/// is not used, as the daemon's own token file is not.
fn token(environment: &Environment) -> Result<String, Condition> {
    let settings = environment.driver.as_ref().ok_or(Condition::NoToken)?;
    let file: PathBuf = match &settings.token_file {
        Some(file) => file.clone(),
        None => paths::daemon_config_dir()
            .map_err(|_| Condition::NoToken)?
            .join("t3code.token"),
    };
    if paths::mode(&file).is_none_or(|mode| mode & 0o077 != 0) {
        return Err(Condition::NoToken);
    }
    let mut text = String::new();
    let mut file = File::open(file).map_err(|_| Condition::NoToken)?;
    std::io::Read::read_to_string(&mut std::io::Read::take(&mut file, SMALL_FILE), &mut text)
        .map_err(|_| Condition::NoToken)?;
    let token = text.trim();
    // Something that can go in a header as it is, or nothing.
    let plain =
        !token.is_empty() && token.len() <= 4096 && token.bytes().all(|c| c.is_ascii_graphic());
    plain.then(|| token.to_string()).ok_or(Condition::NoToken)
}

// ---- the two requests -------------------------------------------------------

/// What the descriptor said, as much of it as the adapter uses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Server {
    /// T3 Code's own id for this installation, when it is one that can be
    /// sent. An id that is not is not kept, and no link is offered.
    environment_id: Option<String>,
    /// The server advertises `shellThreadFamilies` / `shellOlderSettled`
    /// (packages/contracts/src/environment.ts). T3 Code's own client asks
    /// for the smaller shell only of a server that does
    /// (packages/client-runtime/src/state/shell.ts).
    families: bool,
    older_settled: bool,
}

/// What is kept of a descriptor between looks, in the driver's memory file
/// ([`Look::facts`]): which address said it, as a number made from the
/// address, and the two things it can do. Its environment id is the look's
/// `link_ref`.
const DESCRIBED: &str = "described_by";
const FAMILIES: &str = "shell_thread_families";
const OLDER_SETTLED: &str = "shell_older_settled";
/// What a descriptor said is used for this long after the last look that
/// went well. A daemon that was stopped for longer asks again.
const RECALL: Duration = Duration::from_secs(600);

/// A number that stands for an address: FNV-1a, written out so that it is
/// the same in every build.
fn address_number(origin: &str) -> u64 {
    origin
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// What the server at this address said of itself, when the last look went
/// well, was of this address and was not long ago. Otherwise nothing, and
/// the server is asked.
fn recall(previous: &Memory, origin: &str, now: SystemTime) -> Option<Server> {
    let recent = previous.good_at_ms.is_some_and(|good| {
        crate::envelope::unix_ms(now)
            .checked_sub(good)
            .is_some_and(|age| age <= RECALL.as_millis() as u64)
    });
    let same = previous.facts.get(DESCRIBED) == Some(&address_number(origin));
    (previous.condition == Condition::Ok && recent && same).then(|| Server {
        environment_id: previous.link_ref.clone().filter(|id| valid_session_id(id)),
        families: previous.facts.get(FAMILIES) == Some(&1),
        older_settled: previous.facts.get(OLDER_SETTLED) == Some(&1),
    })
}

/// One GET to the server on this machine. No redirect is followed and no
/// proxy is used, so neither the token nor the answer can go anywhere else.
fn get(
    origin: &str,
    path_and_query: &str,
    token: Option<&str>,
    deadline: Instant,
) -> Result<ureq::http::Response<ureq::Body>, Condition> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(Condition::Timeout)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(left))
        .timeout_connect(Some(CONNECT.min(left)))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .proxy(None)
        .user_agent(concat!("nucleated/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut request = agent
        .get(format!("{origin}{path_and_query}"))
        .header("accept", "application/json");
    if let Some(token) = token {
        request = request
            .header("authorization", format!("Bearer {token}"))
            .header(PROTOCOL_HEADER, PROTOCOL_TEXT);
    }
    let response = request.call().map_err(|error| match error {
        ureq::Error::Timeout(_) => Condition::Timeout,
        // Nothing is listening where the server should be.
        ureq::Error::ConnectionFailed => Condition::Absent,
        ureq::Error::Io(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            Condition::Absent
        }
        ureq::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            Condition::Timeout
        }
        _ => Condition::Unreachable,
    })?;
    match response.status().as_u16() {
        200 => Ok(response),
        // EnvironmentAuthInvalidError and EnvironmentScopeRequiredError
        // (packages/contracts/src/environmentHttp.ts).
        401 | 403 => Err(Condition::Rejected),
        _ => Err(Condition::BadReply),
    }
}

/// Read an answer's body as `T`, as it arrives, keeping none of it.
fn answer<T: DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
    deadline: Instant,
) -> Result<T, Condition> {
    whole(&mut response.body_mut().as_reader()).ok_or(if Instant::now() >= deadline {
        Condition::Timeout
    } else {
        Condition::BadReply
    })
}

fn describe(origin: &str, deadline: Instant) -> Result<Server, Condition> {
    let descriptor: Descriptor = answer(get(origin, DESCRIPTOR, None, deadline)?, deadline)?;
    // A server that does not say it speaks this protocol is not asked in it.
    if descriptor.orchestration_protocol_version != Some(PROTOCOL) {
        return Err(Condition::Unsupported);
    }
    let advertised = |capability: fn(&Capabilities) -> Option<bool>| {
        descriptor.capabilities.as_ref().and_then(capability) == Some(true)
    };
    Ok(Server {
        environment_id: descriptor.environment_id.filter(|id| valid_session_id(id)),
        families: advertised(|c| c.shell_thread_families),
        older_settled: advertised(|c| c.shell_older_settled),
    })
}

fn shell(origin: &str, token: &str, server: &Server, deadline: Instant) -> Result<Look, Condition> {
    // Idle subagents and long-settled threads are no session of ours, so a
    // server that can leave them out is asked to, as T3 Code's own client
    // asks: the second only together with the first.
    let query = match (server.families, server.older_settled) {
        (true, true) => "?omitIdleSubagents=1&omitOlderSettled=1",
        (true, false) => "?omitIdleSubagents=1",
        (false, _) => "",
    };
    let path = format!("{SHELL}{query}");
    let snapshot: Snapshot = answer(get(origin, &path, Some(token), deadline)?, deadline)?;
    // An answer with no thread list is not the shell: an error body, or the
    // `resumable` answer this adapter never asks for.
    let threads = snapshot.threads.ok_or(Condition::BadReply)?;
    let projects = snapshot.projects.unwrap_or_default();
    Ok(Look {
        sessions: sessions(&threads.rows, &projects.rows),
        // A row that was not a thread at all may have been anyone's.
        complete: threads.skipped == 0,
        link_ref: server.environment_id.clone(),
        facts: BTreeMap::from([
            (DESCRIBED, address_number(origin)),
            (FAMILIES, u64::from(server.families)),
            (OLDER_SETTLED, u64::from(server.older_settled)),
        ]),
        // T3 Code's worktrees are not counted as its own: see `docs/t3code.md`.
        worktrees: Vec::new(),
    })
}

// ---- the allowlist ----------------------------------------------------------

/// `<T3 home>/userdata/server-runtime.json`: `version` and `origin` only.
#[derive(Deserialize)]
struct Runtime {
    #[serde(default, deserialize_with = "number")]
    version: Option<u64>,
    #[serde(default, deserialize_with = "short_string")]
    origin: Option<String>,
}

/// `ExecutionEnvironmentDescriptor` (packages/contracts/src/environment.ts).
/// Its `label` is a name a person chose and is not read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    #[serde(default, deserialize_with = "short_string")]
    environment_id: Option<String>,
    #[serde(default, deserialize_with = "number")]
    orchestration_protocol_version: Option<u64>,
    #[serde(default, deserialize_with = "object")]
    capabilities: Option<Capabilities>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    #[serde(default, deserialize_with = "flag")]
    shell_thread_families: Option<bool>,
    #[serde(default, deserialize_with = "flag")]
    shell_older_settled: Option<bool>,
}

/// `OrchestrationV2ShellHttpSnapshot`
/// (packages/contracts/src/orchestrationV2.ts). `archivedThreads` is not read.
#[derive(Deserialize)]
struct Snapshot {
    #[serde(default, deserialize_with = "rows")]
    threads: Option<Rows<Thread>>,
    #[serde(default, deserialize_with = "rows")]
    projects: Option<Rows<Project>>,
}

/// `OrchestrationV2ThreadShell`, the state of it. Not named, and so never
/// read: `title`, `latestVisibleMessage`, `lastError`, `lastErrorCode`,
/// `goal`, `branch`, `tags`, `linkOrigin`, `modelSelection`, every pull
/// request and issue field, and the rest.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    #[serde(default, deserialize_with = "short_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    project_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    status: Option<String>,
    /// Told apart from its absence: `activityRunStatus ?? status` falls back
    /// only when this is null or missing.
    #[serde(default)]
    activity_run_status: Stated,
    #[serde(default, deserialize_with = "object")]
    pending_runtime_request: Option<Request>,
    #[serde(default, deserialize_with = "short_string")]
    last_error_class: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    usage_limit_reset_at: Option<String>,
    #[serde(default, deserialize_with = "object")]
    lineage: Option<Lineage>,
    #[serde(default, deserialize_with = "short_string")]
    creation_source: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    settled_override: Option<String>,
    #[serde(default, deserialize_with = "present")]
    archived_at: bool,
    #[serde(default, deserialize_with = "present")]
    deleted_at: bool,
    #[serde(default, deserialize_with = "number")]
    item_count: Option<u64>,
    #[serde(default, deserialize_with = "rows")]
    pending_background_tasks: Option<Rows<Task>>,
    /// Used on the box to find the repo and branch, as a hook's `cwd` is.
    #[serde(default, deserialize_with = "short_string")]
    worktree_path: Option<String>,
}

/// `OrchestrationV2PendingRuntimeRequestSummary`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    #[serde(default, deserialize_with = "short_string")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    user_only: Option<bool>,
}

/// `OrchestrationV2AppThreadLineage`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lineage {
    #[serde(default, deserialize_with = "short_string")]
    parent_thread_id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    relationship_to_parent: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    side_chat: Option<bool>,
}

/// `OrchestrationV2PendingBackgroundTask`: its kind and whether the agent
/// waits on it. Its `description`, `command` and `taskId` are not read.
#[derive(Deserialize)]
struct Task {
    #[serde(default, deserialize_with = "short_string")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    awaited: Option<bool>,
}

/// `OrchestrationProjectShell`: where the project is. Used on the box only,
/// for a thread with no worktree of its own.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Project {
    #[serde(default, deserialize_with = "short_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "short_string")]
    workspace_root: Option<String>,
}

/// One whole JSON object as `T`, or nothing: never an array.
fn whole<T: DeserializeOwned>(payload: &mut dyn std::io::Read) -> Option<T> {
    allowlist::read_object(payload)
}

// ---- from a thread's state to a phase ---------------------------------------
//
// The order is T3 Code's own, the one it uses to tell its phone app what a
// thread is doing (packages/shared/src/agentAwareness.ts,
// `resolveThreadAwarenessPhaseV2` and `projectThreadAwarenessV2`). Every word
// is matched against the list T3 Code's contracts give. A word that is not
// on its list is no opinion.

/// `OrchestrationV2ThreadShell.activityRunStatus`.
const ACTIVE_RUN: [&str; 4] = ["preparing", "starting", "running", "waiting"];
/// `OrchestrationV2ProviderFailureClass`.
const FAILURE_CLASSES: [&str; 6] = [
    "usage_limit",
    "provider_error",
    "transport_error",
    "permission_error",
    "validation_error",
    "unknown",
];
/// `OrchestrationV2CreationSource`, less `provider`: a subagent the provider
/// made itself asks on its parent and is not counted as waiting on the owner
/// (`isProviderNativeSubagentThread`).
const OWN_SOURCES: [&str; 4] = ["web", "mobile", "mcp", "server"];

/// What a thread is waiting on its owner for, from its pending request's
/// kind (`ProviderRequestKind` and the three more
/// `OrchestrationV2RuntimeRequest.kind` adds). `Ok(None)` is "not waiting":
/// no request, or a token refresh, which is not the owner's to answer.
/// `Err` is a request whose kind is not one of the documented words.
fn wait(request: Option<&Request>) -> Result<Option<Phase>, ()> {
    let Some(request) = request else {
        return Ok(None);
    };
    match request.kind.as_deref() {
        Some("user_input") => Ok(Some(Phase::NeedsInput)),
        Some("mcp-elicitation") => Ok(Some(Phase::NeedsElicitation)),
        Some("command" | "file-read" | "file-change" | "permission" | "dynamic_tool_call") => {
            Ok(Some(Phase::NeedsPermission))
        }
        Some("auth_refresh") => Ok(None),
        _ => Err(()),
    }
}

/// `backgroundWorkHoldsCompletion`
/// (packages/shared/src/orchestrationV2PendingBackgroundWork.ts): the agent
/// said it resumes when this work ends, or the work is a subagent or a
/// background task. A command or a monitor holds nothing by itself.
fn held_by_background_work(thread: &Thread) -> bool {
    thread
        .pending_background_tasks
        .as_ref()
        .is_some_and(|tasks| {
            tasks.rows.iter().any(|task| {
                task.awaited == Some(true)
                    || matches!(task.kind.as_deref(), Some("subagent" | "background_task"))
            })
        })
}

fn is_subagent(lineage: &Lineage) -> bool {
    lineage.relationship_to_parent.as_deref() == Some("subagent")
}

/// `relationshipToParent` is null, `fork` or `subagent`. Under any other
/// word, what the thread is to its parent is not known.
fn known_relationship(lineage: &Lineage) -> bool {
    matches!(
        lineage.relationship_to_parent.as_deref(),
        None | Some("fork" | "subagent")
    )
}

/// `isListedThreadLineage`: "Subagents and side chats live inside their
/// parent's view, never in thread lists."
fn is_listed(lineage: &Lineage) -> bool {
    !is_subagent(lineage) && lineage.side_chat != Some(true)
}

/// `subagentAwaitsUser`: a live subagent of T3 Code's own making holding a
/// request the owner must answer. Its coordinator answers its questions, so
/// a `user_input` counts only when it is marked `userOnly`.
fn subagent_wait(thread: &Thread) -> Option<Phase> {
    let lineage = thread.lineage.as_ref()?;
    let request = thread.pending_runtime_request.as_ref()?;
    let live = thread
        .activity_run_status
        .word()
        .is_some_and(|word| ACTIVE_RUN.contains(&word));
    let own = thread
        .creation_source
        .as_deref()
        .is_some_and(|word| OWN_SOURCES.contains(&word));
    let waits = wait(Some(request)).ok().flatten()?;
    let for_the_owner = waits != Phase::NeedsInput || request.user_only == Some(true);
    (is_subagent(lineage)
        && live
        && own
        && for_the_owner
        && !thread.archived_at
        && !thread.deleted_at)
        .then_some(waits)
}

/// The more pressing of two waits. T3 Code announces the newest of several
/// children's requests; this adapter reads no times, so it keeps one order.
fn pressing(phase: Phase) -> u8 {
    match phase {
        Phase::NeedsInput => 3,
        Phase::NeedsElicitation => 2,
        _ => 1,
    }
}

/// For each listed thread, what a subagent below it is waiting on the owner
/// for. The thread a child's request belongs to is found as T3 Code finds it
/// (apps/server/src/relay/AgentAwarenessRelay.ts, `findTopLevelThreadId`):
/// up through parents while the thread is a subagent or a side chat.
fn subagent_waits<'a>(threads: &[(&'a str, &'a Thread)]) -> BTreeMap<&'a str, Phase> {
    let by_id: BTreeMap<&str, &Thread> = threads.iter().copied().collect();
    let mut waits: BTreeMap<&str, Phase> = BTreeMap::new();
    for (id, thread) in threads {
        let Some(waiting) = subagent_wait(thread) else {
            continue;
        };
        let mut seen = BTreeSet::from([*id]);
        let mut current: &'a Thread = thread;
        let top = loop {
            let Some(lineage) = current.lineage.as_ref().filter(|l| known_relationship(l)) else {
                break None;
            };
            if is_listed(lineage) {
                break current.id.as_deref();
            }
            let Some(parent) = lineage.parent_thread_id.as_deref() else {
                break None;
            };
            if !seen.insert(parent) {
                break None;
            }
            let Some(next) = by_id.get(parent) else {
                break None;
            };
            current = *next;
        };
        let Some(top) = top else {
            continue;
        };
        let kept = waits.entry(top).or_insert(waiting);
        if pressing(waiting) > pressing(*kept) {
            *kept = waiting;
        }
    }
    waits
}

/// What one listed thread is doing, or nothing where T3 Code's words do not
/// say.
fn phase(thread: &Thread, subagent: Option<Phase>) -> Option<Phase> {
    if thread.deleted_at || thread.archived_at {
        return Some(Phase::Ended);
    }
    // 1. Its own request. One of a kind not on the list is not guessed at.
    let own = wait(thread.pending_runtime_request.as_ref()).ok()?;
    // 2. A subagent's request outranks the thread's own run status: "The
    //    parent can finish its own turn while a delegated child keeps
    //    working".
    if let Some(waiting) = own.or(subagent) {
        return Some(waiting);
    }
    // 3. `activityRunStatus ?? status`: the latest run's status only when
    //    the live one is null or missing. Something there that is not a word
    //    is not T3 Code's answer, and nothing is said in its place.
    let run = match &thread.activity_run_status {
        Stated::Word(word) => word.as_str(),
        Stated::Absent => thread.status.as_deref()?,
        Stated::Odd => return None,
    };
    let held = held_by_background_work(thread);
    let phase = match run {
        "preparing" | "starting" | "running" | "waiting" => Phase::Working,
        "completed" if held => Phase::Working,
        "completed" | "idle" | "interrupted" | "cancelled" | "rolled_back" => Phase::Idle,
        "failed" => Phase::Failed,
        // `queued` is neither working nor ready in T3 Code's own rule, and
        // any other word is not one of its words.
        _ => return None,
    };
    // 4. A settled thread is one its owner, or T3 Code's own inactivity
    //    rule, has put away. T3 Code resolves the phase first, "so live or
    //    waiting work stays visible after settlement", and so does this.
    let settled = thread.settled_override.as_deref() == Some("settled");
    Some(match phase {
        Phase::Idle if settled => Phase::Ended,
        Phase::Failed if settled && !held => Phase::Ended,
        other => other,
    })
}

fn sessions(threads: &[Thread], projects: &[Project]) -> Vec<Observed> {
    let roots: BTreeMap<&str, &str> = projects
        .iter()
        .filter_map(|project| Some((project.id.as_deref()?, project.workspace_root.as_deref()?)))
        .collect();
    let named: Vec<(&str, &Thread)> = threads
        .iter()
        .filter_map(|thread| Some((thread.id.as_deref()?, thread)))
        .collect();
    let waits = subagent_waits(&named);
    let mut sessions = Vec::new();
    for (id, thread) in named {
        // An id that could not be sent is not reported, and a thread whose
        // id is a sentence is no thread.
        if !valid_session_id(id) {
            continue;
        }
        let Some(lineage) = thread.lineage.as_ref().filter(|l| known_relationship(l)) else {
            // Listed, and nothing more can be said: without its lineage it
            // may be a subagent.
            sessions.push(Observed::new(id, None));
            continue;
        };
        if !is_listed(lineage) {
            continue;
        }
        let phase = phase(thread, waits.get(id).copied());
        let mut observed = Observed::new(id, phase);
        if phase == Some(Phase::Failed) {
            let class = thread
                .last_error_class
                .as_deref()
                .filter(|class| FAILURE_CLASSES.contains(class));
            observed.error_kind = class.map(str::to_string);
            if class == Some("usage_limit") {
                observed.limit_resets_at = thread.usage_limit_reset_at.as_deref().and_then(instant);
            }
        }
        observed.cwd = thread
            .worktree_path
            .as_deref()
            .or_else(|| roots.get(thread.project_id.as_deref()?).copied())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        // `itemCount` is the number of the thread's visible turn items:
        // messages, commands, file changes, tool calls
        // (apps/server/src/orchestration-v2/ProjectionStore.ts,
        // `activeLocalTurnItems`). It grows while the agent works.
        observed.activity = thread.item_count;
        observed.claim_key = Some(id.to_string());
        sessions.push(observed);
    }
    sessions
}

/// `usageLimitResetAt` is written with `DateTime.formatIso`
/// (packages/provider-core/src/server/failure.ts): `2026-11-20T18:00:00.000Z`.
/// That form, with `Z` or a numeric offset, is read. Anything else is not a
/// time.
fn instant(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || !text.is_ascii() {
        return None;
    }
    let digits = |from: usize, to: usize| -> Option<u64> {
        let part = bytes.get(from..to)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0u64, |n, c| n * 10 + u64::from(c - b'0')))
    };
    let mark = |at: usize, byte: u8| (bytes.get(at) == Some(&byte)).then_some(());
    let (year, month, day) = (digits(0, 4)?, digits(5, 7)?, digits(8, 10)?);
    let (hour, minute, second) = (digits(11, 13)?, digits(14, 16)?, digits(17, 19)?);
    mark(4, b'-')?;
    mark(7, b'-')?;
    mark(10, b'T')?;
    mark(13, b':')?;
    mark(16, b':')?;
    let long_month = matches!(month, 1 | 3 | 5 | 7 | 8 | 10 | 12);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        1..=12 if long_month => 31,
        1..=12 => 30,
        _ => return None,
    };
    if year < 1970 || day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // A fraction of a second, which is dropped, and then the zone.
    let mut at = 19;
    if bytes.get(at) == Some(&b'.') {
        let from = at + 1;
        at = from;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == from {
            return None;
        }
    }
    let offset: i64 = match bytes.get(at..)? {
        b"Z" => 0,
        zone if zone.len() == 6 && zone[3] == b':' => {
            let sign = match zone[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, minutes) = (digits(at + 1, at + 3)?, digits(at + 4, at + 6)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 3600 + minutes * 60) as i64
        }
        _ => return None,
    };
    // Days since 1970-01-01, by the civil calendar.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year / 400;
    let year_of_era = shifted_year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    let seconds = (days * 86_400 + hour * 3600 + minute * 60 + second) as i64 - offset;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn seen(shell: Value) -> Vec<Observed> {
        let snapshot: Snapshot = whole(&mut shell.to_string().as_bytes()).expect("a shell");
        sessions(
            &snapshot.threads.expect("threads").rows,
            &snapshot.projects.unwrap_or_default().rows,
        )
    }

    fn top() -> Value {
        json!({ "parentThreadId": null, "relationshipToParent": null, "rootThreadId": "t" })
    }

    fn thread(id: &str, more: Value) -> Value {
        let mut thread = json!({
            "id": id, "projectId": "p", "status": "idle", "lineage": top(),
            "pendingRuntimeRequest": null, "settledOverride": null,
            "archivedAt": null, "deletedAt": null, "itemCount": 3,
            "creationSource": "web",
        });
        for (key, value) in more.as_object().unwrap() {
            thread[key] = value.clone();
        }
        thread
    }

    fn phase_of(more: Value) -> Option<Phase> {
        seen(json!({ "threads": [thread("t", more)], "projects": [] }))[0].phase
    }

    #[test]
    fn a_run_status_becomes_the_phase_t3_code_itself_would_show() {
        for (status, phase) in [
            ("preparing", Some(Phase::Working)),
            ("starting", Some(Phase::Working)),
            ("running", Some(Phase::Working)),
            ("waiting", Some(Phase::Working)),
            ("completed", Some(Phase::Idle)),
            ("idle", Some(Phase::Idle)),
            ("interrupted", Some(Phase::Idle)),
            ("cancelled", Some(Phase::Idle)),
            ("rolled_back", Some(Phase::Idle)),
            ("failed", Some(Phase::Failed)),
            ("queued", None),
            ("a word t3 code may add one day", None),
        ] {
            assert_eq!(phase_of(json!({ "status": status })), phase, "{status}");
        }
        // The run that is live now outranks the status of the latest run.
        assert_eq!(
            phase_of(json!({ "status": "completed", "activityRunStatus": "running" })),
            Some(Phase::Working)
        );
        assert_eq!(
            phase_of(json!({ "status": "running", "activityRunStatus": "soon" })),
            None
        );
        // `activityRunStatus ?? status`: the latest run's status stands in
        // only for a live status that is null or missing, never for one that
        // holds something else.
        assert_eq!(
            phase_of(json!({ "status": "running", "activityRunStatus": null })),
            Some(Phase::Working)
        );
        for odd in [
            json!(7),
            json!(true),
            json!(["running"]),
            json!({ "is": "running" }),
        ] {
            assert_eq!(
                phase_of(json!({ "status": "running", "activityRunStatus": odd })),
                None,
                "{odd}"
            );
        }
    }

    #[test]
    fn a_pending_request_is_what_the_thread_waits_on_its_owner_for() {
        let asked = |kind: &str| {
            phase_of(
                json!({ "status": "running", "pendingRuntimeRequest": { "id": "r", "kind": kind } }),
            )
        };
        assert_eq!(asked("user_input"), Some(Phase::NeedsInput));
        assert_eq!(asked("mcp-elicitation"), Some(Phase::NeedsElicitation));
        for kind in [
            "command",
            "file-read",
            "file-change",
            "permission",
            "dynamic_tool_call",
        ] {
            assert_eq!(asked(kind), Some(Phase::NeedsPermission), "{kind}");
        }
        // A token refresh is not the owner's to answer.
        assert_eq!(asked("auth_refresh"), Some(Phase::Working));
        // A kind that is not on T3 Code's list is not guessed at.
        assert_eq!(asked("something new"), None);
        assert_eq!(
            phase_of(json!({ "status": "running", "pendingRuntimeRequest": { "id": "r" } })),
            None
        );
    }

    #[test]
    fn a_finished_run_is_still_working_while_awaited_work_holds_it() {
        let with = |tasks: Value| {
            phase_of(json!({ "status": "completed", "pendingBackgroundTasks": tasks }))
        };
        assert_eq!(
            with(json!([{ "taskId": "a", "kind": "subagent" }])),
            Some(Phase::Working)
        );
        assert_eq!(
            with(json!([{ "taskId": "a", "kind": "background_task" }])),
            Some(Phase::Working)
        );
        assert_eq!(
            with(json!([{ "taskId": "a", "kind": "command", "awaited": true }])),
            Some(Phase::Working)
        );
        // A dev server left running holds nothing.
        assert_eq!(
            with(json!([{ "taskId": "a", "kind": "command" }])),
            Some(Phase::Idle)
        );
        assert_eq!(
            with(json!([{ "taskId": "a", "kind": "monitor" }])),
            Some(Phase::Idle)
        );
        assert_eq!(with(json!("not a list")), Some(Phase::Idle));
    }

    #[test]
    fn a_settled_deleted_or_archived_thread_is_over_unless_it_is_still_live() {
        let settled = |more: Value| {
            let mut all = json!({ "settledOverride": "settled" });
            for (key, value) in more.as_object().unwrap() {
                all[key] = value.clone();
            }
            phase_of(all)
        };
        assert_eq!(
            settled(json!({ "status": "completed" })),
            Some(Phase::Ended)
        );
        assert_eq!(settled(json!({ "status": "idle" })), Some(Phase::Ended));
        assert_eq!(settled(json!({ "status": "failed" })), Some(Phase::Ended));
        // Live or waiting work stays visible after settlement.
        assert_eq!(
            settled(json!({ "status": "running" })),
            Some(Phase::Working)
        );
        assert_eq!(
            settled(
                json!({ "status": "completed", "pendingRuntimeRequest": { "kind": "permission" } })
            ),
            Some(Phase::NeedsPermission)
        );
        assert_eq!(
            settled(
                json!({ "status": "completed", "pendingBackgroundTasks": [{ "kind": "subagent" }] })
            ),
            Some(Phase::Working)
        );
        assert_eq!(
            phase_of(json!({ "settledOverride": "active", "status": "completed" })),
            Some(Phase::Idle)
        );
        assert_eq!(
            phase_of(json!({ "status": "running", "deletedAt": "2026-11-20T10:00:00.000Z" })),
            Some(Phase::Ended)
        );
        assert_eq!(
            phase_of(json!({ "status": "running", "archivedAt": "2026-11-20T10:00:00.000Z" })),
            Some(Phase::Ended)
        );
    }

    #[test]
    fn subagents_and_side_chats_are_not_sessions_of_their_own() {
        let child = |relationship: Value, side_chat: Value| json!({ "parentThreadId": "t", "relationshipToParent": relationship, "rootThreadId": "t", "sideChat": side_chat });
        let sessions = seen(json!({ "threads": [
            thread("t", json!({ "status": "running" })),
            thread("sub", json!({ "status": "running", "lineage": child(json!("subagent"), json!(false)) })),
            thread("chat", json!({ "status": "running", "lineage": child(json!(null), json!(true)) })),
            thread("fork", json!({ "status": "running", "lineage": child(json!("fork"), json!(false)) })),
            thread("odd", json!({ "status": "running", "lineage": "a sentence where the lineage belongs" })),
            thread("kin", json!({ "status": "running", "lineage": child(json!("a word t3 code may add"), json!(false)) })),
        ]}));
        let listed: Vec<(&str, Option<Phase>)> =
            sessions.iter().map(|s| (s.id.as_str(), s.phase)).collect();
        assert_eq!(
            listed,
            vec![
                ("t", Some(Phase::Working)),
                ("fork", Some(Phase::Working)),
                // Listed, with no opinion: either may be a subagent.
                ("odd", None),
                ("kin", None),
            ]
        );
    }

    #[test]
    fn a_subagent_waiting_on_the_owner_makes_its_top_level_thread_wait() {
        let under = |parent: &str| json!({ "parentThreadId": parent, "relationshipToParent": "subagent", "rootThreadId": "t" });
        let waiting = |id: &str, parent: &str, more: Value| {
            let mut child = thread(
                id,
                json!({
                    "status": "running", "activityRunStatus": "waiting", "creationSource": "server",
                    "lineage": under(parent),
                    "pendingRuntimeRequest": { "id": "r", "kind": "command" },
                }),
            );
            for (key, value) in more.as_object().unwrap() {
                child[key] = value.clone();
            }
            child
        };
        let parent_phase = |children: Vec<Value>| {
            let mut threads = vec![thread("t", json!({ "status": "completed" }))];
            threads.extend(children);
            let sessions = seen(json!({ "threads": threads }));
            assert_eq!(sessions.len(), 1);
            sessions[0].phase
        };
        assert_eq!(
            parent_phase(vec![waiting("c", "t", json!({}))]),
            Some(Phase::NeedsPermission)
        );
        // Through a subagent of a subagent, to the thread at the top.
        assert_eq!(
            parent_phase(vec![
                thread("c", json!({ "status": "running", "lineage": under("t") })),
                waiting("g", "c", json!({})),
            ]),
            Some(Phase::NeedsPermission)
        );
        // Its coordinator answers a subagent's questions, unless only the
        // owner can.
        let question = json!({ "pendingRuntimeRequest": { "id": "r", "kind": "user_input" } });
        assert_eq!(
            parent_phase(vec![waiting("c", "t", question)]),
            Some(Phase::Idle)
        );
        let secret = json!({ "pendingRuntimeRequest": { "id": "r", "kind": "user_input", "userOnly": true } });
        assert_eq!(
            parent_phase(vec![waiting("c", "t", secret)]),
            Some(Phase::NeedsInput)
        );
        // Not one the provider made itself, not one that is no longer live,
        // and not one whose parent is nowhere in the list.
        assert_eq!(
            parent_phase(vec![waiting(
                "c",
                "t",
                json!({ "creationSource": "provider" })
            )]),
            Some(Phase::Idle)
        );
        assert_eq!(
            parent_phase(vec![waiting(
                "c",
                "t",
                json!({ "activityRunStatus": null })
            )]),
            Some(Phase::Idle)
        );
        assert_eq!(
            parent_phase(vec![waiting("c", "gone", json!({}))]),
            Some(Phase::Idle)
        );
        // Two threads that name each other as parent lead nowhere.
        assert_eq!(
            parent_phase(vec![
                waiting("c", "d", json!({})),
                waiting("d", "c", json!({}))
            ]),
            Some(Phase::Idle)
        );
    }

    #[test]
    fn a_failure_carries_its_class_and_a_usage_limit_its_reset_time() {
        let failed = |more: Value| {
            let mut all = json!({ "status": "failed" });
            for (key, value) in more.as_object().unwrap() {
                all[key] = value.clone();
            }
            seen(json!({ "threads": [thread("t", all)] })).remove(0)
        };
        let limit = failed(json!({
            "lastErrorClass": "usage_limit", "usageLimitResetAt": "2026-11-20T18:00:00.000Z",
        }));
        assert_eq!(limit.error_kind.as_deref(), Some("usage_limit"));
        assert_eq!(
            limit.limit_resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_795_197_600))
        );
        // A reset time beside another class is not a limit's.
        let other = failed(json!({
            "lastErrorClass": "provider_error", "usageLimitResetAt": "2026-11-20T18:00:00.000Z",
        }));
        assert_eq!(other.error_kind.as_deref(), Some("provider_error"));
        assert_eq!(other.limit_resets_at, None);
        // A class that is not one of T3 Code's six is not passed on.
        assert_eq!(
            failed(json!({ "lastErrorClass": "the model said no" })).error_kind,
            None
        );
        // A class with no failure beside it says nothing.
        let fine = seen(
            json!({ "threads": [thread("t", json!({ "status": "running", "lastErrorClass": "unknown" }))] }),
        );
        assert_eq!(fine[0].error_kind, None);
    }

    #[test]
    fn only_the_form_t3_code_writes_is_read_as_a_time() {
        let at = |seconds: u64| Some(UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(instant("1970-01-01T00:00:00Z"), at(0));
        assert_eq!(instant("2026-11-20T18:00:00.000Z"), at(1_795_197_600));
        assert_eq!(instant("2026-11-20T19:00:00+01:00"), at(1_795_197_600));
        assert_eq!(instant("2026-11-20T13:30:00.5-04:30"), at(1_795_197_600));
        assert_eq!(instant("2024-02-29T00:00:00Z"), at(1_709_164_800));
        for not_a_time in [
            "",
            "tomorrow at six",
            "2026-11-20",
            "2026-11-20 18:00:00Z",
            "2026-11-20T18:00:00",
            "2026-11-20T18:00:00.Z",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-11-20T24:00:00Z",
            "1969-12-31T23:59:59Z",
            "1970-01-01T00:00:00+01:00",
            "2026-11-20T18:00:00.000Z and then some",
            "２０２６-11-20T18:00:00Z",
        ] {
            assert_eq!(instant(not_a_time), None, "{not_a_time}");
        }
    }

    #[test]
    fn a_thread_works_in_its_worktree_or_else_in_its_project() {
        let sessions = seen(json!({
            "threads": [
                thread("own", json!({ "worktreePath": "/srv/t3/worktrees/a" })),
                thread("shared", json!({ "worktreePath": null })),
                thread("nowhere", json!({ "projectId": "other" })),
                thread("odd", json!({ "worktreePath": "not/absolute" })),
            ],
            "projects": [{ "id": "p", "workspaceRoot": "/srv/code/app", "title": "App" }, "not a project"],
        }));
        let cwd = |id: &str| {
            sessions
                .iter()
                .find(|session| session.id == id)
                .unwrap()
                .cwd
                .clone()
        };
        assert_eq!(cwd("own"), Some(PathBuf::from("/srv/t3/worktrees/a")));
        assert_eq!(cwd("shared"), Some(PathBuf::from("/srv/code/app")));
        assert_eq!(cwd("nowhere"), None);
        assert_eq!(cwd("odd"), None);
    }

    #[test]
    fn a_session_carries_its_item_count_and_the_key_a_hook_inside_it_would_claim() {
        let sessions =
            seen(json!({ "threads": [thread("thread:a.b-1", json!({ "itemCount": 41 }))] }));
        assert_eq!(sessions[0].activity, Some(41));
        assert_eq!(sessions[0].turn, None);
        assert_eq!(sessions[0].claim_key.as_deref(), Some("thread:a.b-1"));
        assert_eq!(sessions[0].agent_session_id, None);
    }

    #[test]
    fn a_thread_whose_id_could_not_be_sent_is_not_reported() {
        let sessions = seen(json!({ "threads": [
            thread("has%20escapes", json!({ "status": "running" })),
            thread("a sentence where an id belongs", json!({ "status": "running" })),
            thread(&"x".repeat(129), json!({ "status": "running" })),
            { "status": "running", "lineage": top() },
            thread("fine", json!({ "status": "running" })),
        ]}));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "fine");
    }

    #[test]
    fn an_answer_that_is_not_an_object_with_a_thread_list_is_no_shell() {
        let read = |text: &str| whole::<Snapshot>(&mut text.as_bytes());
        assert!(read("[[], []]").is_none());
        assert!(read("\"a sentence\"").is_none());
        assert!(read("not json").is_none());
        assert!(read("{\"resumable\":true}").unwrap().threads.is_none());
        assert!(read("{\"threads\":{\"a\":1}}").unwrap().threads.is_none());
        let odd = read("{\"threads\":[{\"id\":\"t\"},\"a row that is a sentence\",[1],7,null]}")
            .unwrap()
            .threads
            .unwrap();
        assert_eq!(odd.rows.len(), 1);
        assert_eq!(odd.skipped, 4);
    }

    #[test]
    fn what_a_server_said_of_itself_is_recalled_only_from_a_recent_good_look_at_that_address() {
        let origin = "http://127.0.0.1:3773";
        let now = UNIX_EPOCH + Duration::from_secs(1_795_089_600);
        let good = Memory {
            driver: "t3code".into(),
            condition: Condition::Ok,
            good_at_ms: Some(crate::envelope::unix_ms(now) - 15_000),
            link_ref: Some("env-1".into()),
            facts: BTreeMap::from([
                (DESCRIBED.to_string(), address_number(origin)),
                (FAMILIES.to_string(), 1),
                (OLDER_SETTLED.to_string(), 0),
            ]),
            ..Memory::default()
        };
        assert_eq!(
            recall(&good, origin, now),
            Some(Server {
                environment_id: Some("env-1".into()),
                families: true,
                older_settled: false,
            })
        );
        // Another address is another server, whatever the last one said.
        assert_eq!(recall(&good, "http://127.0.0.1:3774", now), None);
        // A look that failed, or none yet, recalls nothing.
        let failed = Memory {
            condition: Condition::BadReply,
            ..good.clone()
        };
        assert_eq!(recall(&failed, origin, now), None);
        assert_eq!(recall(&Memory::default(), origin, now), None);
        // A daemon that was stopped for a while asks again.
        assert_eq!(recall(&good, origin, now + RECALL), None);
        // Memory from before the clock was set back is not from now.
        assert_eq!(recall(&good, origin, now - Duration::from_secs(60)), None);
        // Nothing but a yes is a yes.
        let odd = Memory {
            facts: BTreeMap::from([
                (DESCRIBED.to_string(), address_number(origin)),
                (FAMILIES.to_string(), 7),
            ]),
            ..good.clone()
        };
        let recalled = recall(&odd, origin, now).unwrap();
        assert!(!recalled.families && !recalled.older_settled);
    }

    fn hook_var(value: Option<&'static str>) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "state_dir = \"{0}/state\"\ncache_dir = \"{0}/cache\"\n\
                 [[environment]]\nname = \"t3\"\nroot = \"{0}\"\ndriver = \"t3code\"\n",
                dir.path().display()
            ),
        )
        .unwrap();
        let settings = crate::settings::load_from(&config).unwrap();
        let asked = std::cell::RefCell::new(Vec::new());
        let var = |name: &str| {
            asked.borrow_mut().push(name.to_string());
            value.map(str::to_string)
        };
        let claimed = T3Code.claims(&HookProcess {
            environment: settings.environment("t3").unwrap(),
            agent: "claude-code",
            cwd: None,
            var: &var,
        });
        // The one variable T3 Code documents, and no other.
        assert_eq!(*asked.borrow(), vec!["T3CODE_THREAD_ID".to_string()]);
        claimed
    }

    #[test]
    fn a_hook_inside_a_t3_code_thread_claims_that_thread() {
        assert_eq!(
            hook_var(Some("thread:abc-1")),
            Some("thread:abc-1".to_string())
        );
        assert_eq!(hook_var(Some(" thread-2\n")), Some("thread-2".to_string()));
        assert_eq!(hook_var(None), None);
        assert_eq!(hook_var(Some("")), None);
        assert_eq!(hook_var(Some("../../etc")), None);
        assert_eq!(hook_var(Some("a sentence, not an id")), None);
    }
}
