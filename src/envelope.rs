//! The event envelope: the only shape in which a session event is written to
//! disk or sent to Watch.
//!
//! There is no field here for content. Every value is checked against the
//! shape the Watch API accepts when the envelope is built; a value that does
//! not fit is left out, and an envelope whose required fields do not fit is
//! never made.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// What happened to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    SessionStart,
    UserPromptSubmit,
    Activity,
    PermissionPrompt,
    InputPrompt,
    ElicitationPrompt,
    Stop,
    StopFailure,
    SessionEnd,
}

impl EventType {
    /// True for the events that mean the agent is waiting on its owner. These
    /// are sent at once rather than batched.
    pub fn needs_input(self) -> bool {
        matches!(
            self,
            EventType::PermissionPrompt | EventType::InputPrompt | EventType::ElicitationPrompt
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub event_id: String,
    pub env_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_session_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: EventType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    pub client_ts: String,
}

/// What the box knows about an event before it is checked.
#[derive(Debug, Default)]
pub struct Draft {
    pub env_id: String,
    pub session_id: String,
    pub bridge_session_id: Option<String>,
    pub repos: Vec<String>,
    pub branch: Option<String>,
    pub error_kind: Option<String>,
}

impl Envelope {
    /// Build an envelope, or nothing if the environment or session id is not
    /// one Watch would accept. Optional values that do not fit are dropped.
    pub fn new(kind: EventType, draft: Draft, now: SystemTime) -> Option<Envelope> {
        if !valid_env_key(&draft.env_id) || !valid_session_id(&draft.session_id) {
            return None;
        }
        let repos: Vec<String> = draft
            .repos
            .into_iter()
            .filter(|repo| valid_repo(repo))
            .take(32)
            .collect();
        Some(Envelope {
            event_id: uuid7(now),
            env_id: draft.env_id,
            session_id: draft.session_id,
            bridge_session_id: draft.bridge_session_id.filter(|id| valid_session_id(id)),
            kind,
            repos: (!repos.is_empty()).then_some(repos),
            branch: draft.branch.filter(|branch| valid_branch(branch)),
            error_kind: draft
                .error_kind
                .filter(|_| kind == EventType::StopFailure)
                .filter(|kind| valid_error_kind(kind)),
            client_ts: rfc3339(now),
        })
    }

    /// True when every field still fits the shape Watch accepts. Used on
    /// envelopes read back from disk, which something else may have written.
    pub fn is_valid(&self) -> bool {
        valid_event_id(&self.event_id)
            && valid_env_key(&self.env_id)
            && valid_session_id(&self.session_id)
            && self
                .bridge_session_id
                .as_deref()
                .is_none_or(valid_session_id)
            && self
                .repos
                .as_ref()
                .is_none_or(|repos| repos.len() <= 32 && repos.iter().all(|r| valid_repo(r)))
            && self.branch.as_deref().is_none_or(valid_branch)
            && self.error_kind.as_deref().is_none_or(valid_error_kind)
            && valid_timestamp(&self.client_ts)
    }
}

fn all(value: &str, min: usize, max: usize, allowed: impl Fn(u8) -> bool) -> bool {
    (min..=max).contains(&value.len()) && value.bytes().all(allowed)
}

fn word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-')
}

/// `^[a-z0-9][a-z0-9._-]{0,63}$`
pub fn valid_env_key(value: &str) -> bool {
    let lower = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    value.bytes().next().is_some_and(lower)
        && all(value, 1, 64, |c| {
            lower(c) || matches!(c, b'.' | b'_' | b'-')
        })
}

/// `^[A-Za-z0-9_-]{8,64}$`
pub fn valid_event_id(value: &str) -> bool {
    all(value, 8, 64, |c| {
        c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')
    })
}

/// `^[A-Za-z0-9._:-]{1,128}$`
pub fn valid_session_id(value: &str) -> bool {
    all(value, 1, 128, |c| word(c) || c == b':')
}

/// `^[A-Za-z0-9._-]{1,100}(/[A-Za-z0-9._-]{1,100})?$`
pub fn valid_repo(value: &str) -> bool {
    let mut parts = value.split('/');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    parts.next().is_none()
        && all(first, 1, 100, word)
        && second.is_none_or(|name| all(name, 1, 100, word))
}

/// `^[A-Za-z0-9._/@+=#-]{1,255}$`
pub fn valid_branch(value: &str) -> bool {
    all(value, 1, 255, |c| {
        word(c) || matches!(c, b'/' | b'@' | b'+' | b'=' | b'#')
    })
}

/// `^[a-z0-9_.-]{1,64}$`
pub fn valid_error_kind(value: &str) -> bool {
    all(value, 1, 64, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-')
    })
}

fn valid_timestamp(value: &str) -> bool {
    all(value, 20, 32, |c| {
        c.is_ascii_digit() || matches!(c, b'-' | b':' | b'.' | b'T' | b'Z')
    })
}

/// Milliseconds since the Unix epoch.
pub fn unix_ms(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// RFC 3339 in UTC with milliseconds: `2026-11-19T12:00:00.000Z`.
pub fn rfc3339(now: SystemTime) -> String {
    let ms = unix_ms(now);
    let (secs, millis) = (ms / 1000, ms % 1000);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since the epoch to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A UUIDv7: 48 bits of Unix milliseconds, then random bits. Ids made later
/// sort later, which is the order the spool is replayed in.
pub fn uuid7(now: SystemTime) -> String {
    let ms = unix_ms(now);
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes[6..]).is_err() {
        // No system randomness: fall back to the clock and the process id,
        // which still tells this event from the last one.
        let nanos = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0) as u64;
        let mix = nanos.rotate_left(17) ^ (u64::from(std::process::id()) << 32);
        bytes[6..14].copy_from_slice(&mix.to_be_bytes());
    }
    bytes[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_are_rfc3339_utc_with_milliseconds() {
        let at = |ms: u64| rfc3339(UNIX_EPOCH + Duration::from_millis(ms));
        assert_eq!(at(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(1_795_089_603_120), "2026-11-19T12:00:03.120Z");
        assert_eq!(at(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(at(1_735_689_599_999), "2024-12-31T23:59:59.999Z");
    }

    #[test]
    fn event_ids_are_uuid7_and_sort_by_time() {
        let early = uuid7(UNIX_EPOCH + Duration::from_millis(1_795_089_603_120));
        let late = uuid7(UNIX_EPOCH + Duration::from_millis(1_795_089_603_121));
        assert!(valid_event_id(&early));
        assert_eq!(early.len(), 36);
        assert_eq!(early.as_bytes()[14], b'7');
        assert!(matches!(early.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert!(early < late);
        assert_ne!(uuid7(UNIX_EPOCH), uuid7(UNIX_EPOCH));
    }

    #[test]
    fn shapes_match_the_api() {
        assert!(valid_env_key("client-a"));
        assert!(!valid_env_key("Client-A"));
        assert!(!valid_env_key("-a"));
        assert!(!valid_env_key(&"a".repeat(65)));
        assert!(valid_repo("your-org/web"));
        assert!(valid_repo("web"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("has space"));
        assert!(valid_branch("workspace/fix-login"));
        assert!(!valid_branch("has space"));
        assert!(valid_error_kind("rate_limit"));
        assert!(!valid_error_kind("Rate limited: try again later"));
        assert!(valid_session_id("7f3c2a1e-58b4-4d0c-9a6e-2b1f0c9d8e7a"));
        assert!(!valid_session_id("a b"));
    }

    #[test]
    fn values_that_do_not_fit_are_left_out() {
        let draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            bridge_session_id: Some("not an id".into()),
            repos: vec!["your-org/web".into(), "not a repo".into()],
            branch: Some("a branch with spaces".into()),
            error_kind: Some("An error message, not a kind".into()),
        };
        let envelope = Envelope::new(EventType::StopFailure, draft, UNIX_EPOCH).unwrap();
        assert_eq!(envelope.repos, Some(vec!["your-org/web".to_string()]));
        assert_eq!(envelope.bridge_session_id, None);
        assert_eq!(envelope.branch, None);
        assert_eq!(envelope.error_kind, None);
        assert!(envelope.is_valid());
    }

    #[test]
    fn an_event_with_a_bad_session_or_environment_is_never_made() {
        let draft = |env: &str, session: &str| Draft {
            env_id: env.into(),
            session_id: session.into(),
            ..Draft::default()
        };
        assert!(Envelope::new(EventType::Stop, draft("Client A", "s1"), UNIX_EPOCH).is_none());
        assert!(Envelope::new(EventType::Stop, draft("client-a", "s 1"), UNIX_EPOCH).is_none());
    }

    #[test]
    fn error_kind_only_travels_with_a_failure() {
        let draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            error_kind: Some("rate_limit".into()),
            ..Draft::default()
        };
        let envelope = Envelope::new(EventType::Stop, draft, UNIX_EPOCH).unwrap();
        assert_eq!(envelope.error_kind, None);
    }
}
