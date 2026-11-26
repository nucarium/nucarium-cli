//! The event envelope: the only shape in which a session event is written to
//! disk or sent to Watch.
//!
//! There is no field here for content. Every value is checked against the
//! shape the Watch API accepts when the envelope is built; a value that does
//! not fit is left out, and an envelope whose required fields do not fit is
//! never made.

use crate::watch::Taken;
use serde::{Deserialize, Deserializer, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A limit reset time is sent only when it lies this far around now: Watch
/// keeps nothing outside the same window, so a time outside it is a misread
/// field, not a reset.
const LIMIT_RESET_BEHIND: Duration = Duration::from_secs(60);
const LIMIT_RESET_AHEAD: Duration = Duration::from_secs(8 * 86_400);

/// The most Watch takes in an outcome's counts and its list of pull requests.
pub const MAX_COMMITS: u32 = 100_000;
pub const MAX_UNCOMMITTED_REPOS: u32 = 32;
pub const MAX_UNPUSHED_COMMITS: u32 = 100_000;
pub const MAX_PULL_REQUESTS: usize = 16;
/// The most folder numbers a claim holds.
pub const MAX_CLAIM_FOLDERS: usize = 64;

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
    /// What a session left behind when it stopped or ended. It moves no
    /// session to another status.
    Outcome,
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
    /// When the agent says a usage limit resets. A timestamp, and only with a
    /// failed stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_resets_at: Option<String>,
    /// With an outcome: how many commits the session made, across its repos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<u32>,
    /// With an outcome: how many of the session's repos hold changes that are
    /// not committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncommitted_repos: Option<u32>,
    /// With an outcome: how many of the session's commits are on no remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpushed_commits: Option<u32>,
    /// With an outcome: the addresses of the open pull requests whose head
    /// is the session's commit. An address has no room for a title. Empty
    /// says there are none; absent says the box could not find out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_requests: Option<Vec<String>>,
    /// The driver environment, on the same machine, whose session this one
    /// runs inside. Only beside `driver_session`, and only from the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_env: Option<String>,
    /// The driver's own id for the session this one runs inside. Only
    /// beside `driver_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_session: Option<String>,
    /// What the hook says about a driver's session its agent runs in. It
    /// stays in the spool and is never sent. One that does not fit is read
    /// as none, and the record with it is kept.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "claim_that_fits"
    )]
    pub claim: Option<Claim>,
    pub client_ts: String,
}

/// A hook's word that its agent runs inside a driver's session: the driver
/// environment, the key the driver knows the session by, and the numbers of
/// the folders the hook's session works in, made with `folder_number`. No
/// folder's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub driver_env: String,
    pub key: String,
    pub folders: Vec<String>,
}

impl Claim {
    /// An environment key, a session id, and at most [`MAX_CLAIM_FOLDERS`]
    /// folder numbers, each sixteen lower-case hex digits.
    pub fn fits(&self) -> bool {
        valid_env_key(&self.driver_env)
            && valid_session_id(&self.key)
            && self.folders.len() <= MAX_CLAIM_FOLDERS
            && self
                .folders
                .iter()
                .all(|folder| valid_folder_number(folder))
    }
}

/// A claim, read without ever failing the record it is part of: what is not
/// a claim, or does not fit, is none.
fn claim_that_fits<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Claim>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<Claim>(value)
        .ok()
        .filter(Claim::fits))
}

/// Both of a session's driver reference when both fit, and nothing else:
/// one without the other is none.
fn driver_pair(
    driver_env: Option<String>,
    driver_session: Option<String>,
) -> (Option<String>, Option<String>) {
    match (driver_env, driver_session) {
        (Some(env), Some(session)) if valid_env_key(&env) && valid_session_id(&session) => {
            (Some(env), Some(session))
        }
        _ => (None, None),
    }
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
    /// A time, never the agent's words for one: there is nothing here a
    /// sentence could be put in.
    pub limit_resets_at: Option<SystemTime>,
    /// The four below travel with an outcome only. Each is left as nothing
    /// when the box could not find it out, which is not the same as zero.
    pub commits: Option<u32>,
    pub uncommitted_repos: Option<u32>,
    pub unpushed_commits: Option<u32>,
    pub pull_requests: Option<Vec<String>>,
    /// A session's driver reference: both or neither.
    pub driver_env: Option<String>,
    pub driver_session: Option<String>,
    pub claim: Option<Claim>,
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
        let outcome = kind == EventType::Outcome;
        // Watch refuses a whole batch for one address on a repo the event
        // does not name, so such an address never gets as far as the spool.
        let pull_requests = draft.pull_requests.filter(|_| outcome).map(|pulls| {
            let mut kept: Vec<String> = Vec::new();
            for pull in pulls {
                if names_one_of(&pull, &repos) && !kept.contains(&pull) {
                    kept.push(pull);
                }
            }
            kept.truncate(MAX_PULL_REQUESTS);
            kept
        });
        let (driver_env, driver_session) = driver_pair(draft.driver_env, draft.driver_session);
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
            limit_resets_at: draft
                .limit_resets_at
                .filter(|_| kind == EventType::StopFailure)
                .filter(|at| within(*at, now, LIMIT_RESET_BEHIND, LIMIT_RESET_AHEAD))
                .map(rfc3339),
            commits: draft.commits.filter(|n| outcome && *n <= MAX_COMMITS),
            uncommitted_repos: draft
                .uncommitted_repos
                .filter(|n| outcome && *n <= MAX_UNCOMMITTED_REPOS),
            unpushed_commits: draft
                .unpushed_commits
                .filter(|n| outcome && *n <= MAX_UNPUSHED_COMMITS),
            pull_requests,
            driver_env,
            driver_session,
            claim: draft.claim.filter(Claim::fits),
            client_ts: rfc3339(now),
        })
    }

    /// The envelope as it goes to a Watch that takes `taken`: never the
    /// claim, and the session's driver reference only to a Watch that takes
    /// `driver_session`, and only whole.
    pub fn as_sent(&self, taken: &Taken) -> Envelope {
        let (driver_env, driver_session) = match taken.takes("driver_session") {
            true => driver_pair(self.driver_env.clone(), self.driver_session.clone()),
            false => (None, None),
        };
        Envelope {
            driver_env,
            driver_session,
            claim: None,
            ..self.clone()
        }
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
            && self.limit_resets_at.as_deref().is_none_or(valid_timestamp)
            && self.commits.is_none_or(|n| n <= MAX_COMMITS)
            && self
                .uncommitted_repos
                .is_none_or(|n| n <= MAX_UNCOMMITTED_REPOS)
            && self
                .unpushed_commits
                .is_none_or(|n| n <= MAX_UNPUSHED_COMMITS)
            && self.pull_requests.as_ref().is_none_or(|pulls| {
                let repos = self.repos.as_deref().unwrap_or_default();
                pulls.len() <= MAX_PULL_REQUESTS
                    && pulls.iter().all(|pull| names_one_of(pull, repos))
            })
            && valid_timestamp(&self.client_ts)
    }
}

/// True when `pull` is the address of a pull request on one of `repos`,
/// whatever the case of the names.
fn names_one_of(pull: &str, repos: &[String]) -> bool {
    pull_request_repo(pull)
        .is_some_and(|named| repos.iter().any(|repo| repo.eq_ignore_ascii_case(&named)))
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

/// `^[0-9a-f]{16}$`: a folder number as `folder_number` makes one.
pub fn valid_folder_number(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
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

/// `^[0-9A-Za-z.+-]{1,32}$`
pub fn valid_version(value: &str) -> bool {
    all(value, 1, 32, |c| {
        c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-')
    })
}

/// `^([0-9a-f]{40}|[0-9a-f]{64})$`: a git commit id, and nothing a word or a
/// branch name could pass for.
pub fn valid_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// `^https://github\.com/<owner>/<name>/pull/[1-9][0-9]{0,9}$`, at most 240
/// characters: an owner, a name and a number, with no query and no fragment.
/// Answers with the `owner/name` the address is on.
pub fn pull_request_repo(value: &str) -> Option<String> {
    let rest = value.strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let (pull, number) = (parts.next()?, parts.next()?);
    let numbered = all(number, 1, 10, |c| c.is_ascii_digit()) && !number.starts_with('0');
    (parts.next().is_none()
        && value.len() <= 240
        && pull == "pull"
        && numbered
        && all(owner, 1, 100, word)
        && all(name, 1, 100, word))
    .then(|| format!("{owner}/{name}"))
}

/// A timestamp as [`rfc3339`] writes one, `2026-11-19T12:00:00.000Z`, and
/// nothing looser: this program is the only writer of the times it sends.
pub fn valid_timestamp(value: &str) -> bool {
    let shape = b"dddd-dd-ddTdd:dd:dd.dddZ";
    value.len() == shape.len()
        && value.bytes().zip(shape).all(|(c, want)| match want {
            b'd' => c.is_ascii_digit(),
            other => c == *other,
        })
}

/// True when `at` is no more than `behind` before `now` and no more than
/// `ahead` after it.
pub fn within(at: SystemTime, now: SystemTime, behind: Duration, ahead: Duration) -> bool {
    match at.duration_since(now) {
        Ok(after) => after <= ahead,
        Err(before) => before.duration() <= behind,
    }
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
        assert!(valid_version("2.1.0"));
        assert!(valid_version("0.0.0-dev+build.7"));
        assert!(!valid_version("2.1.0 (Claude Code)"));
        assert!(!valid_version(""));
        assert!(!valid_version(&"1".repeat(33)));
        assert!(valid_commit_id(&"a1".repeat(20)));
        assert!(valid_commit_id(&"0f".repeat(32)));
        assert!(!valid_commit_id(&"A1".repeat(20)));
        assert!(!valid_commit_id(&"g1".repeat(20)));
        assert!(!valid_commit_id("main"));
        assert!(!valid_commit_id(&"a".repeat(41)));
        assert!(valid_timestamp("2026-11-20T03:10:00.000Z"));
        assert!(!valid_timestamp("2026-11-20T03:10:00Z"));
        assert!(!valid_timestamp("tomorrow at ten past three"));
        assert!(!valid_timestamp("2026-11-20 03:10:00.000Z"));
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
            // Ten years on: a misread field, not a reset time.
            limit_resets_at: Some(UNIX_EPOCH + Duration::from_secs(10 * 365 * 86_400)),
            ..Draft::default()
        };
        let envelope = Envelope::new(EventType::StopFailure, draft, UNIX_EPOCH).unwrap();
        assert_eq!(envelope.repos, Some(vec!["your-org/web".to_string()]));
        assert_eq!(envelope.bridge_session_id, None);
        assert_eq!(envelope.branch, None);
        assert_eq!(envelope.error_kind, None);
        assert_eq!(envelope.limit_resets_at, None);
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

    fn limited(kind: EventType, resets_in: i64) -> Envelope {
        let now = UNIX_EPOCH + Duration::from_millis(1_795_089_600_000);
        let resets = match resets_in {
            ahead @ 0.. => now + Duration::from_secs(ahead as u64),
            behind => now - Duration::from_secs(behind.unsigned_abs()),
        };
        let draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            error_kind: Some("rate_limit".into()),
            limit_resets_at: Some(resets),
            ..Draft::default()
        };
        Envelope::new(kind, draft, now).unwrap()
    }

    #[test]
    fn a_limit_reset_time_only_travels_with_a_failure_and_only_as_a_timestamp() {
        let envelope = limited(EventType::StopFailure, 4 * 3_600);
        assert_eq!(
            envelope.limit_resets_at.as_deref(),
            Some("2026-11-19T16:00:00.000Z")
        );
        assert!(envelope.is_valid());
        assert_eq!(
            serde_json::to_value(&envelope).unwrap()["limit_resets_at"],
            "2026-11-19T16:00:00.000Z"
        );
        for kind in [EventType::Stop, EventType::Activity, EventType::SessionEnd] {
            let envelope = limited(kind, 4 * 3_600);
            assert_eq!(envelope.limit_resets_at, None);
            assert!(serde_json::to_value(&envelope)
                .unwrap()
                .get("limit_resets_at")
                .is_none());
        }
    }

    #[test]
    fn a_limit_reset_time_outside_what_watch_keeps_is_left_out() {
        let kept = |resets_in| limited(EventType::StopFailure, resets_in).limit_resets_at;
        assert!(kept(0).is_some());
        assert!(kept(-60).is_some());
        assert!(kept(-61).is_none());
        assert!(kept(8 * 86_400).is_some());
        assert!(kept(8 * 86_400 + 1).is_none());
    }

    fn outcome(kind: EventType, change: impl Fn(&mut Draft)) -> Envelope {
        let mut draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            repos: vec!["your-org/web".into(), "api".into()],
            commits: Some(3),
            uncommitted_repos: Some(1),
            unpushed_commits: Some(2),
            pull_requests: Some(vec!["https://github.com/your-org/web/pull/412".into()]),
            ..Draft::default()
        };
        change(&mut draft);
        Envelope::new(kind, draft, UNIX_EPOCH).unwrap()
    }

    #[test]
    fn an_outcome_carries_counts_and_addresses_and_no_other_event_does() {
        let envelope = outcome(EventType::Outcome, |_| {});
        assert!(envelope.is_valid());
        let sent = serde_json::to_value(&envelope).unwrap();
        assert_eq!(sent["type"], "outcome");
        assert_eq!(sent["commits"], 3);
        assert_eq!(sent["uncommitted_repos"], 1);
        assert_eq!(sent["unpushed_commits"], 2);
        assert_eq!(
            sent["pull_requests"],
            serde_json::json!(["https://github.com/your-org/web/pull/412"])
        );
        for kind in [EventType::Stop, EventType::SessionEnd, EventType::Activity] {
            let sent = serde_json::to_value(outcome(kind, |_| {})).unwrap();
            for field in [
                "commits",
                "uncommitted_repos",
                "unpushed_commits",
                "pull_requests",
            ] {
                assert!(sent.get(field).is_none(), "{field} went with {kind:?}");
            }
        }
    }

    #[test]
    fn an_outcome_says_nothing_where_the_box_does_not_know() {
        let envelope = outcome(EventType::Outcome, |draft| {
            draft.commits = None;
            draft.pull_requests = None;
        });
        let sent = serde_json::to_value(&envelope).unwrap();
        assert!(sent.get("commits").is_none());
        assert!(sent.get("pull_requests").is_none());
        assert_eq!(sent["uncommitted_repos"], 1);
        // Known to be none is said, so Watch drops the ones it had.
        let none = outcome(EventType::Outcome, |draft| {
            draft.pull_requests = Some(Vec::new())
        });
        assert_eq!(
            serde_json::to_value(&none).unwrap()["pull_requests"],
            serde_json::json!([])
        );
    }

    #[test]
    fn a_count_outside_what_watch_takes_is_left_out() {
        let envelope = outcome(EventType::Outcome, |draft| {
            draft.commits = Some(MAX_COMMITS + 1);
            draft.uncommitted_repos = Some(MAX_UNCOMMITTED_REPOS + 1);
            draft.unpushed_commits = Some(MAX_UNPUSHED_COMMITS);
        });
        assert_eq!(envelope.commits, None);
        assert_eq!(envelope.uncommitted_repos, None);
        assert_eq!(envelope.unpushed_commits, Some(MAX_UNPUSHED_COMMITS));
        assert!(envelope.is_valid());
    }

    #[test]
    fn a_pull_request_is_an_address_on_a_repo_the_event_names_and_nothing_else() {
        let on = |url: &str| pull_request_repo(url);
        assert_eq!(
            on("https://github.com/your-org/web/pull/412").as_deref(),
            Some("your-org/web")
        );
        for not_one in [
            "https://github.com/your-org/web/pull/0",
            "https://github.com/your-org/web/pull/412/files",
            "https://github.com/your-org/web/pull/412?title=Fix+the+login",
            "https://github.com/your-org/web/pull/412#issuecomment-1",
            "https://github.com/your-org/web/issues/412",
            "https://github.com/your-org/web/pull/fix-the-login",
            "https://github.com/your org/web/pull/1",
            "http://github.com/your-org/web/pull/412",
            "https://github.com.example.test/your-org/web/pull/412",
            "https://example.test/your-org/web/pull/412",
            "Fix the login page",
            "",
        ] {
            assert_eq!(on(not_one), None, "{not_one}");
        }
        assert_eq!(
            on(&format!("https://github.com/o/{}/pull/1", "n".repeat(101))),
            None
        );

        let envelope = outcome(EventType::Outcome, |draft| {
            draft.pull_requests = Some(vec![
                "https://github.com/Your-Org/Web/pull/412".into(),
                "https://github.com/your-org/web/pull/412?title=Fix+the+login".into(),
                "https://github.com/someone-else/web/pull/9".into(),
                // A repo named without its owner is not one an address can be on.
                "https://github.com/your-org/api/pull/3".into(),
                "https://github.com/Your-Org/Web/pull/412".into(),
            ]);
        });
        assert_eq!(
            envelope.pull_requests,
            Some(vec!["https://github.com/Your-Org/Web/pull/412".to_string()])
        );
        assert!(envelope.is_valid());

        let many = outcome(EventType::Outcome, |draft| {
            draft.pull_requests = Some(
                (1..=40)
                    .map(|n| format!("https://github.com/your-org/web/pull/{n}"))
                    .collect(),
            );
        });
        assert_eq!(
            many.pull_requests.as_ref().unwrap().len(),
            MAX_PULL_REQUESTS
        );
    }

    #[test]
    fn an_outcome_from_disk_that_watch_would_refuse_is_not_valid() {
        let good = outcome(EventType::Outcome, |_| {});
        let broken = |change: fn(&mut Envelope)| {
            let mut envelope = good.clone();
            change(&mut envelope);
            envelope.is_valid()
        };
        assert!(!broken(|e| e.commits = Some(MAX_COMMITS + 1)));
        assert!(!broken(|e| e.uncommitted_repos = Some(33)));
        assert!(!broken(
            |e| e.unpushed_commits = Some(MAX_UNPUSHED_COMMITS + 1)
        ));
        assert!(!broken(|e| {
            e.pull_requests = Some(vec!["Fix the login page".into()])
        }));
        // An address on a repo the event does not name costs Watch the batch.
        assert!(!broken(|e| e.repos = Some(vec!["your-org/api".into()])));
        assert!(!broken(|e| e.repos = None));
    }

    fn in_a_driver(change: impl Fn(&mut Draft)) -> Envelope {
        let mut draft = Draft {
            env_id: "client-a".into(),
            session_id: "s1".into(),
            driver_env: Some("client-a-t3".into()),
            driver_session: Some("thread_01HZX:2".into()),
            claim: Some(Claim {
                driver_env: "client-a-t3".into(),
                key: "thread_01HZX".into(),
                folders: vec!["0123456789abcdef".into(), "fedcba9876543210".into()],
            }),
            ..Draft::default()
        };
        change(&mut draft);
        Envelope::new(EventType::Stop, draft, UNIX_EPOCH).unwrap()
    }

    fn taking(names: &[&str]) -> Taken {
        Taken::from_names(
            &names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn a_sessions_driver_reference_is_both_fields_or_neither() {
        let both = in_a_driver(|_| {});
        assert_eq!(both.driver_env.as_deref(), Some("client-a-t3"));
        assert_eq!(both.driver_session.as_deref(), Some("thread_01HZX:2"));
        assert!(both.is_valid());
        // One without the other, or one that does not fit, takes both with it.
        for change in [
            (|draft: &mut Draft| draft.driver_env = None) as fn(&mut Draft),
            |draft| draft.driver_session = None,
            |draft| draft.driver_env = Some("Client A's T3".into()),
            |draft| draft.driver_env = Some("a".repeat(65)),
            |draft| draft.driver_session = Some("the thread about the login page".into()),
            |draft| draft.driver_session = Some("t".repeat(129)),
        ] {
            let envelope = in_a_driver(change);
            assert_eq!(envelope.driver_env, None);
            assert_eq!(envelope.driver_session, None);
            assert!(envelope.is_valid());
        }
        let none = in_a_driver(|draft| {
            draft.driver_env = None;
            draft.driver_session = None;
        });
        let written = serde_json::to_value(&none).unwrap();
        assert!(written.get("driver_env").is_none());
        assert!(written.get("driver_session").is_none());
    }

    #[test]
    fn a_claim_stays_in_the_spool_and_one_that_does_not_fit_is_dropped() {
        let envelope = in_a_driver(|_| {});
        assert_eq!(
            serde_json::to_value(&envelope).unwrap()["claim"],
            serde_json::json!({
                "driver_env": "client-a-t3", "key": "thread_01HZX",
                "folders": ["0123456789abcdef", "fedcba9876543210"],
            })
        );
        let folders = |folders: Vec<String>| {
            in_a_driver(move |draft| draft.claim.as_mut().unwrap().folders = folders.clone()).claim
        };
        assert!(folders(Vec::new()).is_some());
        assert!(folders(vec!["0123456789abcdef".into(); MAX_CLAIM_FOLDERS]).is_some());
        for not_one in [
            vec!["0123456789abcdef".to_string(); MAX_CLAIM_FOLDERS + 1],
            vec!["0123456789ABCDEF".into()],
            vec!["0123456789abcde".into()],
            vec!["0123456789abcdef0".into()],
            vec!["/home/agent/work".into()],
        ] {
            assert_eq!(folders(not_one.clone()), None, "{not_one:?}");
        }
        let claimed = |change: fn(&mut Claim)| {
            in_a_driver(move |draft| change(draft.claim.as_mut().unwrap()))
        };
        for change in [
            (|claim: &mut Claim| claim.driver_env = "Client A".into()) as fn(&mut Claim),
            |claim| claim.key = "the thread about the login page".into(),
            |claim| claim.key = String::new(),
        ] {
            let envelope = claimed(change);
            // The record is kept, without its claim.
            assert_eq!(envelope.claim, None);
            assert_eq!(envelope.session_id, "s1");
        }
    }

    #[test]
    fn a_claim_read_back_that_does_not_fit_is_read_as_none_and_the_record_kept() {
        let written = serde_json::to_value(in_a_driver(|_| {})).unwrap();
        let read = |claim: serde_json::Value| {
            let mut record = written.clone();
            record["claim"] = claim;
            serde_json::from_value::<Envelope>(record).unwrap()
        };
        assert!(read(written["claim"].clone()).claim.is_some());
        for odd in [
            serde_json::json!("client-a-t3"),
            serde_json::json!({ "driver_env": "client-a-t3", "key": "k", "folders": ["0123456789abcdef"], "cwd": "/home/agent" }),
            serde_json::json!({ "driver_env": "client-a-t3", "key": "k" }),
            serde_json::json!({ "driver_env": "client-a-t3", "key": "k", "folders": ["/home/agent"] }),
        ] {
            let envelope = read(odd.clone());
            assert_eq!(envelope.claim, None, "{odd}");
            assert!(envelope.is_valid());
        }
        // Any other field the envelope does not have still makes it no record.
        let mut stranger = written.clone();
        stranger["prompt"] = serde_json::json!("fix the login page");
        assert!(serde_json::from_value::<Envelope>(stranger).is_err());
    }

    #[test]
    fn what_goes_to_watch_never_has_the_claim_and_the_reference_only_when_taken() {
        let envelope = in_a_driver(|_| {});
        let sent = |taken: &Taken| serde_json::to_value(envelope.as_sent(taken)).unwrap();
        for taken in [Taken::default(), taking(&["agent", "driver", "driver_env"])] {
            let sent = sent(&taken);
            for field in ["claim", "driver_env", "driver_session"] {
                assert!(sent.get(field).is_none(), "{field} went to {taken:?}");
            }
        }
        let sent = sent(&taking(&["driver_session"]));
        assert!(sent.get("claim").is_none());
        assert_eq!(sent["driver_env"], "client-a-t3");
        assert_eq!(sent["driver_session"], "thread_01HZX:2");
        assert_eq!(sent["session_id"], "s1");
        // Half a reference, however it got there, is never sent.
        let mut half = envelope.clone();
        half.driver_session = None;
        let sent = serde_json::to_value(half.as_sent(&taking(&["driver_session"]))).unwrap();
        assert!(sent.get("driver_env").is_none());
        // The record on the box keeps what it had.
        assert!(envelope.claim.is_some());
    }

    #[test]
    fn an_envelope_from_disk_with_words_for_a_time_is_not_valid() {
        let mut envelope = limited(EventType::StopFailure, 60);
        envelope.limit_resets_at = Some("in about four hours".into());
        assert!(!envelope.is_valid());
        // An envelope written before the field existed still reads.
        let old = r#"{"event_id":"0193f2a0-0000-7000-8000-000000000000","env_id":"client-a",
            "session_id":"s1","type":"stop","client_ts":"2026-11-19T12:00:00.000Z"}"#;
        let envelope: Envelope = serde_json::from_str(old).unwrap();
        assert!(envelope.is_valid());
        assert_eq!(envelope.limit_resets_at, None);
    }
}
