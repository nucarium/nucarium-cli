//! What the box knows about each agent session.
//!
//! The daemon applies events to these files in order. They follow the rules
//! Watch has for what an event does to a session, so `nucleated status` shows
//! what Watch would show. Two things are Watch's alone: which kinds of failure
//! are a usage limit or an expired login, and when a working session counts as
//! stuck. Here such a session reads `failed`, with its kind, or `working`.
//!
//! An outcome is the one event that moves nothing: it says what the session
//! left behind, and the session keeps the status it had.

use crate::envelope::{valid_env_key, valid_session_id, EventType};
use crate::paths;
use crate::process::Identity;
use crate::spool::Record;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a working session's tool calls are passed on. Watch calls a
/// session stuck when it hears of no tool activity for long enough, so it has
/// to hear of some; every tool call would be noise. The heartbeat tells Watch
/// this number, and Watch never judges on less than twice it.
pub const ACTIVITY_BEAT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Working,
    NeedsInput,
    Failed,
    Ended,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Working => "working",
            Status::NeedsInput => "needs_input",
            Status::Failed => "failed",
            Status::Ended => "ended",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub env: String,
    pub session_id: String,
    pub status: Status,
    /// What the session is waiting for: `permission`, `input` or `elicitation`.
    #[serde(default)]
    pub needs_kind: Option<String>,
    #[serde(default)]
    pub needs_since_ms: Option<u64>,
    #[serde(default)]
    pub error_kind: Option<String>,
    /// For a failure on a usage limit, when the agent said the limit resets.
    #[serde(default)]
    pub limit_resets_at: Option<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub bridge_session_id: Option<String>,
    /// The nucleus session folder this agent session works in.
    #[serde(default)]
    pub workspace: Option<String>,
    /// The agent process its hooks last ran from. The daemon ends the
    /// session when that process has gone. Never sent.
    #[serde(default)]
    pub process: Option<Identity>,
    pub started_ms: u64,
    pub last_event_ms: u64,
    #[serde(default)]
    pub ended_ms: Option<u64>,
    /// The newest event applied. An event with an older id changes nothing.
    pub last_event_id: String,
    /// When an event of this session was last worth sending, on the clock of
    /// the hook that recorded it.
    #[serde(default)]
    pub last_forwarded_ms: u64,
    /// What the session left behind, as last reported.
    #[serde(default)]
    pub outcome: Option<Outcome>,
}

/// What a session left behind when it stopped or ended: counts and addresses.
/// A count that is nothing was not found out, which is not the same as zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub reported_ms: u64,
    #[serde(default)]
    pub commits: Option<u32>,
    #[serde(default)]
    pub uncommitted_repos: Option<u32>,
    #[serde(default)]
    pub unpushed_commits: Option<u32>,
    #[serde(default)]
    pub pull_requests: Option<Vec<String>>,
}

/// What applying an event did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub session: Session,
    /// False when the event is older than one already applied.
    pub changed: bool,
    /// Whether the event is worth sending. A tool call by a session already
    /// known to be working is worth sending once in [`ACTIVITY_BEAT`].
    pub forward: bool,
}

/// Apply one event to what is known about its session. Nothing comes back
/// for an outcome of a session the box does not know: an outcome never makes
/// a session, and Watch would have no use for it either.
pub fn apply(previous: Option<Session>, record: &Record) -> Option<Applied> {
    let event = &record.event;
    if let Some(known) = &previous {
        if event.event_id <= known.last_event_id {
            // Already applied before a restart, or late. It stayed in the
            // spool, so it was worth sending.
            return Some(Applied {
                session: known.clone(),
                changed: false,
                forward: true,
            });
        }
    }
    if event.kind == EventType::Outcome {
        // The session's status, its last event and its place in the order of
        // events all stay as they were, whatever the status is: an ended
        // session is the one whose outcome matters most.
        let mut session = previous?;
        let mut outcome = session.outcome.take().unwrap_or_default();
        outcome.reported_ms = record.at_ms;
        // What the event says replaces what was known; what it leaves out stands.
        if event.commits.is_some() {
            outcome.commits = event.commits;
        }
        if event.uncommitted_repos.is_some() {
            outcome.uncommitted_repos = event.uncommitted_repos;
        }
        if event.unpushed_commits.is_some() {
            outcome.unpushed_commits = event.unpushed_commits;
        }
        if event.pull_requests.is_some() {
            outcome.pull_requests = event.pull_requests.clone();
        }
        session.outcome = Some(outcome);
        return Some(Applied {
            session,
            changed: true,
            forward: true,
        });
    }
    let revives = event.kind == EventType::SessionStart;
    let forward = match (event.kind, &previous) {
        (EventType::Activity, Some(known)) => match known.status {
            Status::Ended => false,
            // Watch already has it as working. It still has to hear, now and
            // then, that the session's tools are running.
            Status::Working => {
                record.at_ms.saturating_sub(known.last_forwarded_ms)
                    >= ACTIVITY_BEAT.as_millis() as u64
            }
            _ => true,
        },
        _ => true,
    };
    let mut session = match previous {
        // Only a new start brings an ended session back, as a new session.
        Some(ended) if ended.status == Status::Ended && !revives => {
            return Some(Applied {
                session: Session {
                    last_event_id: event.event_id.clone(),
                    ..ended
                },
                changed: true,
                forward,
            });
        }
        Some(known) if !(known.status == Status::Ended && revives) => known,
        _ => Session {
            env: event.env_id.clone(),
            session_id: event.session_id.clone(),
            status: Status::Idle,
            needs_kind: None,
            needs_since_ms: None,
            error_kind: None,
            limit_resets_at: None,
            repos: Vec::new(),
            branch: None,
            bridge_session_id: None,
            workspace: None,
            process: None,
            started_ms: record.at_ms,
            last_event_ms: record.at_ms,
            ended_ms: None,
            last_event_id: String::new(),
            last_forwarded_ms: 0,
            outcome: None,
        },
    };
    let needs = |kind: &str| (Status::NeedsInput, Some(kind.to_string()));
    let (status, needs_kind) = match event.kind {
        EventType::SessionStart | EventType::Stop => (Status::Idle, None),
        EventType::UserPromptSubmit | EventType::Activity => (Status::Working, None),
        EventType::PermissionPrompt => needs("permission"),
        EventType::InputPrompt => needs("input"),
        EventType::ElicitationPrompt => needs("elicitation"),
        EventType::StopFailure => (Status::Failed, None),
        EventType::SessionEnd => (Status::Ended, None),
        // Dealt with above: an outcome moves no session.
        EventType::Outcome => (session.status, session.needs_kind.clone()),
    };
    // A session that was already waiting has been waiting since the first prompt.
    session.needs_since_ms = match status {
        Status::NeedsInput => session.needs_since_ms.or(Some(record.at_ms)),
        _ => None,
    };
    session.needs_kind = needs_kind;
    // The box keeps `failed` with the kind. Which kinds are a usage limit or
    // an expired login is Watch's list to keep, not the box's.
    (session.error_kind, session.limit_resets_at) = match status {
        Status::Failed => (event.error_kind.clone(), event.limit_resets_at.clone()),
        _ => (None, None),
    };
    session.ended_ms = (status == Status::Ended).then_some(record.at_ms);
    session.status = status;
    if let Some(repos) = &event.repos {
        session.repos = repos.clone();
    }
    if event.branch.is_some() {
        session.branch = event.branch.clone();
    }
    if event.bridge_session_id.is_some() {
        session.bridge_session_id = event.bridge_session_id.clone();
    }
    if record.workspace.is_some() {
        session.workspace = record.workspace.clone();
    }
    if record.process.is_some() {
        session.process = record.process;
    }
    session.last_event_ms = record.at_ms;
    session.last_event_id = event.event_id.clone();
    if forward {
        session.last_forwarded_ms = record.at_ms;
    }
    Some(Applied {
        session,
        changed: true,
        forward,
    })
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store under the daemon's state folder.
    pub fn new(state_dir: &Path) -> Store {
        Store {
            dir: state_dir.join("sessions"),
        }
    }

    fn path(&self, env: &str, session_id: &str) -> Option<PathBuf> {
        (valid_env_key(env) && valid_session_id(session_id))
            .then(|| self.dir.join(env).join(format!("{session_id}.json")))
    }

    pub fn load(&self, env: &str, session_id: &str) -> Option<Session> {
        let text = fs::read_to_string(self.path(env, session_id)?).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, session: &Session) -> anyhow::Result<()> {
        let path = self
            .path(&session.env, &session.session_id)
            .ok_or_else(|| anyhow::anyhow!("not a session this box can name"))?;
        paths::write_private(&path, serde_json::to_string(session)?.as_bytes())
    }

    /// Every session of one environment.
    pub fn list(&self, env: &str) -> Vec<Session> {
        let Ok(entries) = fs::read_dir(self.dir.join(env)) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|entry| serde_json::from_str(&fs::read_to_string(entry.path()).ok()?).ok())
            .collect()
    }

    /// Forget sessions that ended more than `max_age_ms` ago.
    pub fn prune(&self, env: &str, max_age_ms: u64, now_ms: u64) {
        for session in self.list(env) {
            let stale = session
                .ended_ms
                .is_some_and(|ended| now_ms.saturating_sub(ended) > max_age_ms);
            if let (true, Some(path)) = (stale, self.path(env, &session.session_id)) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Draft, Envelope};
    use serde_json::Value;
    use std::time::UNIX_EPOCH;

    fn record(kind: EventType, at_ms: u64) -> Record {
        let now = UNIX_EPOCH + Duration::from_millis(at_ms);
        let draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            error_kind: Some("rate_limit".into()),
            limit_resets_at: Some(now + Duration::from_secs(3_600)),
            ..Draft::default()
        };
        Record {
            event: Envelope::new(kind, draft, now).unwrap(),
            at_ms,
            workspace: None,
            process: None,
        }
    }

    /// Apply events at the given seconds and say which were worth sending.
    fn forwarded(events: &[(EventType, u64)]) -> Vec<bool> {
        let mut session = None;
        events
            .iter()
            .map(|(kind, at_s)| {
                let applied = apply(session.clone(), &record(*kind, at_s * 1_000)).unwrap();
                session = Some(applied.session.clone());
                applied.forward
            })
            .collect()
    }

    /// Apply events one after another, a second apart, and return each result.
    fn play(kinds: &[EventType]) -> Vec<Applied> {
        let mut session = None;
        kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| {
                let applied =
                    apply(session.clone(), &record(*kind, 1_000 * (i as u64 + 1))).unwrap();
                session = Some(applied.session.clone());
                applied
            })
            .collect()
    }

    fn statuses(kinds: &[EventType]) -> Vec<Status> {
        play(kinds).iter().map(|a| a.session.status).collect()
    }

    use EventType::*;

    #[test]
    fn a_session_moves_through_its_statuses() {
        assert_eq!(
            statuses(&[
                SessionStart,
                UserPromptSubmit,
                PermissionPrompt,
                Activity,
                Stop,
                InputPrompt,
                UserPromptSubmit,
                StopFailure,
                SessionEnd
            ]),
            [
                Status::Idle,
                Status::Working,
                Status::NeedsInput,
                Status::Working,
                Status::Idle,
                Status::NeedsInput,
                Status::Working,
                Status::Failed,
                Status::Ended
            ]
        );
    }

    #[test]
    fn waiting_is_timed_from_the_first_prompt_and_cleared_by_anything_else() {
        let played = play(&[
            UserPromptSubmit,
            PermissionPrompt,
            ElicitationPrompt,
            Activity,
        ]);
        assert_eq!(played[1].session.needs_since_ms, Some(2_000));
        assert_eq!(played[1].session.needs_kind.as_deref(), Some("permission"));
        assert_eq!(played[2].session.needs_since_ms, Some(2_000));
        assert_eq!(played[2].session.needs_kind.as_deref(), Some("elicitation"));
        assert_eq!(played[3].session.needs_since_ms, None);
        assert_eq!(played[3].session.needs_kind, None);
    }

    #[test]
    fn a_failure_keeps_its_kind_until_the_session_moves_on() {
        let played = play(&[UserPromptSubmit, StopFailure, UserPromptSubmit]);
        assert_eq!(played[1].session.status, Status::Failed);
        assert_eq!(played[1].session.error_kind.as_deref(), Some("rate_limit"));
        assert_eq!(played[2].session.error_kind, None);
    }

    #[test]
    fn a_limit_reset_time_is_kept_with_the_failure_and_no_longer() {
        let played = play(&[UserPromptSubmit, StopFailure, Activity]);
        assert_eq!(played[0].session.limit_resets_at, None);
        assert_eq!(
            played[1].session.limit_resets_at.as_deref(),
            Some("1970-01-01T01:00:02.000Z")
        );
        assert_eq!(played[2].session.limit_resets_at, None);
    }

    #[test]
    fn only_a_new_start_brings_an_ended_session_back() {
        let played = play(&[
            SessionStart,
            SessionEnd,
            Stop,
            UserPromptSubmit,
            SessionStart,
        ]);
        assert_eq!(played[2].session.status, Status::Ended);
        assert_eq!(played[3].session.status, Status::Ended);
        assert_eq!(played[4].session.status, Status::Idle);
        assert_eq!(played[4].session.started_ms, 5_000);
        assert_eq!(played[4].session.ended_ms, None);
    }

    #[test]
    fn a_tool_call_is_only_worth_sending_when_it_changes_the_status() {
        let forwarded: Vec<bool> = play(&[
            UserPromptSubmit,
            Activity,
            Activity,
            PermissionPrompt,
            Activity,
            Activity,
            SessionEnd,
            Activity,
        ])
        .iter()
        .map(|a| a.forward)
        .collect();
        assert_eq!(
            forwarded,
            [true, false, false, true, true, false, true, false]
        );
        // A session first heard of mid-flight is worth sending.
        assert!(play(&[Activity])[0].forward);
    }

    #[test]
    fn a_working_sessions_tool_calls_are_sent_once_in_five_minutes() {
        assert_eq!(
            forwarded(&[
                (UserPromptSubmit, 0),
                (Activity, 1),
                (Activity, 299),
                (Activity, 300),
                (Activity, 301),
                (Activity, 599),
                (Activity, 600),
            ]),
            [true, false, false, true, false, false, true]
        );
    }

    #[test]
    fn the_five_minutes_run_from_the_last_event_sent_whatever_it_was() {
        assert_eq!(
            forwarded(&[
                (UserPromptSubmit, 0),
                (Activity, 200),
                // Waiting and granted: both sent, and the clock starts again.
                (PermissionPrompt, 250),
                (Activity, 260),
                (Activity, 500),
                (Activity, 559),
                (Activity, 560),
            ]),
            [true, false, true, true, false, false, true]
        );
    }

    #[test]
    fn a_session_recorded_before_the_beat_existed_sends_its_next_tool_call() {
        let mut known = apply(None, &record(UserPromptSubmit, 1_000))
            .unwrap()
            .session;
        let stored = serde_json::to_value(&known).unwrap();
        let mut old = stored.as_object().unwrap().clone();
        old.remove("last_forwarded_ms");
        old.remove("limit_resets_at");
        old.remove("outcome");
        known = serde_json::from_value(Value::Object(old)).unwrap();
        assert_eq!(known.status, Status::Working);
        // A real box's clock is a long way from zero, which is what an old
        // file reads as.
        let next = apply(Some(known), &record(Activity, 1_800_000_000_000)).unwrap();
        assert!(next.forward);
        assert_eq!(next.session.last_forwarded_ms, 1_800_000_000_000);
    }

    #[test]
    fn an_ended_sessions_tool_call_is_never_sent() {
        assert_eq!(
            forwarded(&[(SessionEnd, 0), (Activity, 1), (Activity, 900)]),
            [true, false, false]
        );
    }

    /// An outcome at `at_ms`, saying only what `say` fills in.
    fn outcome(at_ms: u64, say: impl Fn(&mut Draft)) -> Record {
        let now = UNIX_EPOCH + Duration::from_millis(at_ms);
        let mut draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            repos: vec!["your-org/web".into()],
            ..Draft::default()
        };
        say(&mut draft);
        Record {
            event: Envelope::new(Outcome, draft, now).unwrap(),
            at_ms,
            workspace: None,
            process: None,
        }
    }

    #[test]
    fn an_outcome_is_kept_with_the_session_and_moves_nothing() {
        for last in [Stop, StopFailure, SessionEnd, PermissionPrompt, Activity] {
            let before = play(&[UserPromptSubmit, last])[1].session.clone();
            let applied = apply(
                Some(before.clone()),
                &outcome(9_000, |draft| {
                    draft.commits = Some(3);
                    draft.uncommitted_repos = Some(1);
                    draft.unpushed_commits = Some(2);
                    draft.pull_requests =
                        Some(vec!["https://github.com/your-org/web/pull/412".into()]);
                }),
            )
            .unwrap();
            assert!(applied.changed && applied.forward);
            let after = applied.session;
            assert_eq!(
                after.outcome,
                Some(super::Outcome {
                    reported_ms: 9_000,
                    commits: Some(3),
                    uncommitted_repos: Some(1),
                    unpushed_commits: Some(2),
                    pull_requests: Some(vec!["https://github.com/your-org/web/pull/412".into()]),
                })
            );
            // Everything else is as it was, the order of events included.
            assert_eq!(
                Session {
                    outcome: None,
                    ..after
                },
                before,
                "after {last:?}"
            );
        }
    }

    #[test]
    fn a_later_outcome_replaces_what_it_says_and_keeps_what_it_leaves_out() {
        let stopped = play(&[UserPromptSubmit, Stop])[1].session.clone();
        let first = apply(
            Some(stopped),
            &outcome(9_000, |draft| {
                draft.commits = Some(3);
                draft.pull_requests = Some(vec!["https://github.com/your-org/web/pull/412".into()]);
            }),
        )
        .unwrap()
        .session;
        let second = apply(
            Some(first),
            &outcome(70_000, |draft| {
                draft.uncommitted_repos = Some(0);
                draft.unpushed_commits = Some(0);
                draft.pull_requests = Some(Vec::new());
            }),
        )
        .unwrap()
        .session;
        let outcome = second.outcome.unwrap();
        assert_eq!(outcome.reported_ms, 70_000);
        assert_eq!(outcome.commits, Some(3));
        assert_eq!(outcome.uncommitted_repos, Some(0));
        assert_eq!(outcome.pull_requests, Some(Vec::new()));
    }

    #[test]
    fn an_outcome_never_makes_a_session_and_a_new_start_forgets_the_last_one() {
        assert_eq!(apply(None, &outcome(9_000, |_| {})), None);
        let ended = play(&[UserPromptSubmit, SessionEnd])[1].session.clone();
        let reported = apply(
            Some(ended),
            &outcome(9_000, |draft| draft.commits = Some(1)),
        )
        .unwrap()
        .session;
        assert_eq!(reported.status, Status::Ended);
        let again = apply(Some(reported), &record(SessionStart, 20_000))
            .unwrap()
            .session;
        assert_eq!(again.outcome, None);
        // An outcome from before the new start is late, and changes nothing.
        let late = apply(Some(again.clone()), &outcome(15_000, |_| {})).unwrap();
        assert!(!late.changed);
        assert_eq!(late.session, again);
    }

    #[test]
    fn a_session_keeps_the_last_agent_process_its_hooks_ran_from() {
        let from = |kind: EventType, at_ms: u64, pid: Option<u32>| Record {
            process: pid.map(|pid| Identity { pid, start: 1_000 }),
            ..record(kind, at_ms)
        };
        let first = apply(None, &from(SessionStart, 1_000, Some(40)))
            .unwrap()
            .session;
        assert_eq!(first.process.map(|p| p.pid), Some(40));
        // An event from no known process leaves the last one known.
        let quiet = apply(Some(first), &from(UserPromptSubmit, 2_000, None))
            .unwrap()
            .session;
        assert_eq!(quiet.process.map(|p| p.pid), Some(40));
        let resumed = apply(Some(quiet), &from(Stop, 3_000, Some(41)))
            .unwrap()
            .session;
        assert_eq!(resumed.process.map(|p| p.pid), Some(41));
        // A new start after an end is a new session, with only its own.
        let ended = apply(Some(resumed), &from(SessionEnd, 4_000, None))
            .unwrap()
            .session;
        let again = apply(Some(ended), &from(SessionStart, 5_000, None))
            .unwrap()
            .session;
        assert_eq!(again.process, None);
    }

    #[test]
    fn an_event_older_than_the_last_one_applied_changes_nothing() {
        let first = apply(None, &record(UserPromptSubmit, 5_000)).unwrap();
        let late = apply(Some(first.session.clone()), &record(Stop, 1_000)).unwrap();
        assert!(!late.changed);
        assert_eq!(late.session, first.session);
        assert!(late.forward);
    }

    #[test]
    fn sessions_are_kept_per_environment_and_forgotten_after_they_end() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let live = apply(None, &record(UserPromptSubmit, 1_000))
            .unwrap()
            .session;
        let mut ended = apply(None, &record(SessionEnd, 1_000)).unwrap().session;
        ended.session_id = "s2".into();
        store.save(&live).unwrap();
        store.save(&ended).unwrap();
        assert_eq!(store.load("client-a", "s1"), Some(live.clone()));
        assert_eq!(store.list("client-a").len(), 2);
        assert!(store.list("ops").is_empty());
        store.prune("client-a", 10_000, 20_000);
        assert_eq!(store.list("client-a"), vec![live]);
        // A name that is not an environment's never becomes a path.
        assert_eq!(store.load("../etc", "s1"), None);
    }
}
