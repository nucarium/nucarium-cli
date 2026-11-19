//! What the box knows about each agent session.
//!
//! The daemon applies events to these files in order. They follow the same
//! rules Watch does, so `nucleated status` shows what Watch would show.

use crate::envelope::{valid_env_key, valid_session_id, EventType};
use crate::paths;
use crate::spool::Record;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub bridge_session_id: Option<String>,
    /// The nucleus session folder this agent session works in.
    #[serde(default)]
    pub workspace: Option<String>,
    pub started_ms: u64,
    pub last_event_ms: u64,
    #[serde(default)]
    pub ended_ms: Option<u64>,
    /// The newest event applied. An event with an older id changes nothing.
    pub last_event_id: String,
}

/// What applying an event did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub session: Session,
    /// False when the event is older than one already applied.
    pub changed: bool,
    /// Whether the event is worth sending. A tool call by a session already
    /// known to be working tells Watch nothing new.
    pub forward: bool,
}

/// Apply one event to what is known about its session.
pub fn apply(previous: Option<Session>, record: &Record) -> Applied {
    let event = &record.event;
    if let Some(known) = &previous {
        if event.event_id <= known.last_event_id {
            // Already applied before a restart, or late. It stayed in the
            // spool, so it was worth sending.
            return Applied {
                session: known.clone(),
                changed: false,
                forward: true,
            };
        }
    }
    let was = previous.as_ref().map(|session| session.status);
    let revives = event.kind == EventType::SessionStart;
    let forward = match event.kind {
        EventType::Activity => !matches!(was, Some(Status::Working | Status::Ended)),
        _ => true,
    };
    let mut session = match previous {
        // Only a new start brings an ended session back, as a new session.
        Some(ended) if ended.status == Status::Ended && !revives => {
            return Applied {
                session: Session {
                    last_event_id: event.event_id.clone(),
                    ..ended
                },
                changed: true,
                forward,
            };
        }
        Some(known) if !(known.status == Status::Ended && revives) => known,
        _ => Session {
            env: event.env_id.clone(),
            session_id: event.session_id.clone(),
            status: Status::Idle,
            needs_kind: None,
            needs_since_ms: None,
            error_kind: None,
            repos: Vec::new(),
            branch: None,
            bridge_session_id: None,
            workspace: None,
            started_ms: record.at_ms,
            last_event_ms: record.at_ms,
            ended_ms: None,
            last_event_id: String::new(),
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
    };
    // A session that was already waiting has been waiting since the first prompt.
    session.needs_since_ms = match status {
        Status::NeedsInput => session.needs_since_ms.or(Some(record.at_ms)),
        _ => None,
    };
    session.needs_kind = needs_kind;
    session.error_kind = match status {
        Status::Failed => event.error_kind.clone(),
        _ => None,
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
    session.last_event_ms = record.at_ms;
    session.last_event_id = event.event_id.clone();
    Applied {
        session,
        changed: true,
        forward,
    }
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
    use std::time::{Duration, UNIX_EPOCH};

    fn record(kind: EventType, at_ms: u64) -> Record {
        let draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            error_kind: Some("rate_limit".into()),
            ..Draft::default()
        };
        Record {
            event: Envelope::new(kind, draft, UNIX_EPOCH + Duration::from_millis(at_ms)).unwrap(),
            at_ms,
            workspace: None,
        }
    }

    /// Apply events one after another, a second apart, and return each result.
    fn play(kinds: &[EventType]) -> Vec<Applied> {
        let mut session = None;
        kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| {
                let applied = apply(session.clone(), &record(*kind, 1_000 * (i as u64 + 1)));
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
        assert_eq!(played[1].session.error_kind.as_deref(), Some("rate_limit"));
        assert_eq!(played[2].session.error_kind, None);
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
    fn an_event_older_than_the_last_one_applied_changes_nothing() {
        let first = apply(None, &record(UserPromptSubmit, 5_000));
        let late = apply(Some(first.session.clone()), &record(Stop, 1_000));
        assert!(!late.changed);
        assert_eq!(late.session, first.session);
        assert!(late.forward);
    }

    #[test]
    fn sessions_are_kept_per_environment_and_forgotten_after_they_end() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let live = apply(None, &record(UserPromptSubmit, 1_000)).session;
        let mut ended = apply(None, &record(SessionEnd, 1_000)).session;
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
