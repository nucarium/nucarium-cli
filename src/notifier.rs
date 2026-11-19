//! The box speaking for itself while Watch cannot be reached.
//!
//! Watch tells the owner when a box goes dark, and it cannot tell them that
//! it has itself stopped hearing. With an address of the owner's own in the
//! `[notify]` table, the daemon posts there:
//!
//! - once, when no heartbeat has been accepted for the time set (`after`);
//! - while that lasts, once for each session that starts waiting on its
//!   owner, since Watch cannot say so;
//! - once, when a heartbeat is accepted again.
//!
//! A post is a machine's name, an environment's name and a time, in the shape
//! Watch's own alerts have. It goes to that one address and never to
//! Nucarium. With no address set, nothing here runs.
//!
//! This covers Watch being unreachable from a box that still has a network.
//! A box with no network cannot post either; Watch says that one has gone
//! dark.

use crate::daemon::{jittered, Timings};
use crate::envelope::{rfc3339, unix_ms};
use crate::paths;
use crate::settings::Notify;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A session whose "needs input" Watch has not been told of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub env: String,
    pub session_id: String,
    pub since_ms: u64,
}

impl Waiting {
    /// One wait of one session. A session that waits again later is told of
    /// again; the same wait never is.
    fn key(&self) -> String {
        format!("{}/{}@{}", self.env, self.session_id, self.since_ms)
    }
}

/// What has been posted, kept in `notify.json` so that a restart of the
/// daemon in the middle of an outage repeats nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// When heartbeats stopped being accepted, in Unix milliseconds. Nothing
    /// while they are accepted.
    #[serde(default)]
    pub down_since_ms: Option<u64>,
    /// "Cannot reach Watch" has been delivered for this outage.
    #[serde(default)]
    pub announced: bool,
    /// The waits already posted in this outage.
    #[serde(default)]
    pub told: Vec<String>,
    /// "Can reach Watch again" is owed: how long the outage lasted, in seconds.
    #[serde(default)]
    pub back_after_s: Option<u64>,
}

pub fn state_file(state_dir: &Path) -> PathBuf {
    state_dir.join("notify.json")
}

pub fn read_state(state_dir: &Path) -> Option<Saved> {
    serde_json::from_str(&fs::read_to_string(state_file(state_dir)).ok()?).ok()
}

struct Post {
    kind: &'static str,
    title: String,
    body: String,
    priority: &'static str,
    tags: &'static str,
}

/// What a delivered post settles.
enum Settles {
    Back,
    Announced,
    Told(String),
}

/// One line in the daemon's log. Never an address, a token or a session.
fn log(line: &str) {
    eprintln!("nucleated: {line}");
}

/// `13:40 UTC`. The box writes UTC: it does not know the owner's time zone.
fn clock(ms: u64) -> String {
    let stamp = rfc3339(UNIX_EPOCH + Duration::from_millis(ms));
    format!("{} UTC", &stamp[11..16])
}

/// "45 seconds", "25 minutes", "3 hours", as Watch's alerts say a duration.
fn lasted(seconds: u64) -> String {
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    let minutes = (seconds + 30) / 60;
    let hours = (minutes + 30) / 60;
    match () {
        () if seconds < 90 => plural(seconds, "second"),
        () if minutes < 90 => plural(minutes, "minute"),
        () if hours < 48 => plural(hours, "hour"),
        () => plural((hours + 12) / 24, "day"),
    }
}

/// A name as it goes into a post: one line, and not a long one.
fn name(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(100).collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let word = chunk.iter().enumerate().fold(0u32, |word, (i, byte)| {
            word | u32::from(*byte) << (16 - 8 * i)
        });
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A header's value is plain ASCII. Anything else goes in an RFC 2047
/// encoded word, which ntfy decodes, as Watch's own alerts do.
fn header_value(text: &str) -> String {
    if text.bytes().all(|c| (0x20..=0x7e).contains(&c)) {
        text.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", base64(text.as_bytes()))
    }
}

/// The bearer token for the address, from a file only its owner can read.
/// `Err` when the config names a file that cannot be used: a post that was
/// meant to carry a token is not sent without one.
fn token(file: Option<&Path>) -> Result<Option<String>, ()> {
    let Some(file) = file else {
        return Ok(None);
    };
    if paths::mode(file).is_some_and(|mode| mode & 0o077 != 0) {
        return Err(());
    }
    let text = fs::read_to_string(file).map_err(|_| ())?;
    let token = text.trim();
    (!token.is_empty() && token.bytes().all(|c| c.is_ascii_graphic()))
        .then(|| Some(token.to_string()))
        .ok_or(())
}

/// Whether the notifier could post with this config, and why not otherwise.
/// For `nucleated check`: it sends nothing.
pub fn usable(target: &Notify) -> Result<(), &'static str> {
    let Some(file) = target.token_file.as_deref() else {
        return Ok(());
    };
    if !file.is_file() {
        return Err("the token file does not exist");
    }
    if paths::mode(file).is_some_and(|mode| mode & 0o077 != 0) {
        return Err("the token file is readable by others; it must be 600");
    }
    token(Some(file))
        .map(|_| ())
        .map_err(|()| "the token file does not hold a token")
}

/// One attempt, no redirects: an ntfy-shaped webhook, as Watch sends one,
/// with no alert id and no link.
fn send(target: &Notify, post: &Post, timeout: Duration) -> bool {
    let Ok(token) = token(target.token_file.as_deref()) else {
        return false;
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        // A redirect could carry the token to another host.
        .max_redirects(0)
        .max_redirects_will_error(false)
        .user_agent(concat!("nucleated/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut request = agent
        .post(&target.url)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Title", header_value(&post.title))
        .header("Priority", post.priority)
        .header("Tags", post.tags)
        .header("X-Nucarium-Alert-Kind", post.kind);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    request
        .send(post.body.as_str())
        .is_ok_and(|response| (200..300).contains(&response.status().as_u16()))
}

pub struct Notifier {
    file: PathBuf,
    saved: Saved,
    /// The outage on this process's own clock: a moment, and how long the
    /// outage had already lasted at that moment. Deadlines are counted on
    /// this, so a wall clock that jumps moves none of them.
    down: Option<(Instant, Duration)>,
    backoff: Option<Duration>,
    retry_at: Option<Instant>,
}

impl Notifier {
    pub fn new(state_dir: &Path) -> Notifier {
        Notifier {
            file: state_file(state_dir),
            saved: read_state(state_dir).unwrap_or_default(),
            down: None,
            backoff: None,
            retry_at: None,
        }
    }

    pub fn saved(&self) -> &Saved {
        &self.saved
    }

    fn save(&self) {
        if self.saved == Saved::default() {
            let _ = fs::remove_file(&self.file);
        } else if let Ok(text) = serde_json::to_string(&self.saved) {
            let _ = paths::write_private(&self.file, text.as_bytes());
        }
    }

    fn settle_retries(&mut self) {
        self.backoff = None;
        self.retry_at = None;
    }

    /// How long the outage has lasted. After a restart the count resumes from
    /// what the saved time says.
    fn down_for(&mut self, now: Instant, wall: SystemTime) -> Option<Duration> {
        let since_ms = self.saved.down_since_ms?;
        let (then, already) = *self.down.get_or_insert_with(|| {
            let already = Duration::from_millis(unix_ms(wall).saturating_sub(since_ms));
            (now, already)
        });
        Some(already + now.saturating_duration_since(then))
    }

    /// A heartbeat was not accepted. `counts` is false for the two answers
    /// that mean Watch is there: it refused the token, or asked for a pause.
    pub fn heartbeat_failed(&mut self, counts: bool, now: Instant, wall: SystemTime) {
        if counts && self.saved.down_since_ms.is_none() {
            self.saved.down_since_ms = Some(unix_ms(wall));
            self.down = Some((now, Duration::ZERO));
            self.save();
        }
    }

    /// A heartbeat was accepted: the outage, if there was one, is over.
    pub fn heartbeat_accepted(&mut self, now: Instant, wall: SystemTime) {
        let Some(lasted) = self.down_for(now, wall) else {
            return;
        };
        // "Reachable again" follows "cannot reach" only once that was
        // delivered. Had it not been, the owner never heard of the outage,
        // and hears of neither.
        let back = self.saved.announced.then_some(lasted.as_secs());
        self.saved = Saved {
            back_after_s: back.or(self.saved.back_after_s),
            ..Saved::default()
        };
        self.down = None;
        self.settle_retries();
        self.save();
    }

    /// True when there is no outage and no post owed: nothing to do.
    pub fn is_idle(&self) -> bool {
        self.saved == Saved::default()
    }

    /// True while the box is the one that has to say a session is waiting.
    pub fn tells_waits(&self) -> bool {
        self.saved.down_since_ms.is_some() && self.saved.announced
    }

    /// Stop and forget: the notifier is off, or the box no longer reports to
    /// Watch, so there is no outage to speak of.
    fn forget(&mut self) {
        if self.saved != Saved::default() {
            self.saved = Saved::default();
            self.save();
        }
        self.down = None;
        self.settle_retries();
    }

    fn next(
        &mut self,
        target: &Notify,
        machine: &str,
        waiting: &[Waiting],
        now: Instant,
        wall: SystemTime,
    ) -> Option<(Post, Settles)> {
        let machine = name(machine);
        if let Some(seconds) = self.saved.back_after_s {
            return Some((
                Post {
                    kind: "box_watch_reachable",
                    title: format!("{machine} can reach Nucarium Watch again"),
                    body: format!("Heartbeats are accepted again after {}.", lasted(seconds)),
                    priority: "default",
                    tags: "white_check_mark",
                },
                Settles::Back,
            ));
        }
        let down_for = self.down_for(now, wall)?;
        let since_ms = self.saved.down_since_ms?;
        if !self.saved.announced {
            return (down_for >= target.after).then(|| {
                (
                    Post {
                        kind: "box_watch_unreachable",
                        title: format!("{machine} cannot reach Nucarium Watch"),
                        body: format!(
                            "No heartbeat has been accepted since {}. This box tells you here \
                             when a session waits on you.",
                            clock(since_ms)
                        ),
                        priority: "high",
                        tags: "warning",
                    },
                    Settles::Announced,
                )
            });
        }
        let wait = waiting
            .iter()
            .find(|wait| !self.saved.told.contains(&wait.key()))?;
        let env = name(&wait.env);
        Some((
            Post {
                kind: "box_session_waiting",
                title: format!("An agent on {env} is waiting for you"),
                body: format!(
                    "A session on {env} ({machine}) has waited since {}. Watch cannot be \
                     reached, so this comes from the box.",
                    clock(wait.since_ms)
                ),
                priority: "high",
                tags: "hourglass",
            },
            Settles::Told(wait.key()),
        ))
    }

    /// Post what is due, one post a turn. `target` is the config's `[notify]`
    /// table while the box reports to Watch, and nothing otherwise.
    /// `waiting` is every session whose "needs input" Watch has not heard.
    pub fn tick(
        &mut self,
        target: Option<&Notify>,
        machine: &str,
        waiting: &[Waiting],
        timings: &Timings,
        now: Instant,
        wall: SystemTime,
    ) {
        let Some(target) = target else {
            self.forget();
            return;
        };
        if self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        let Some((post, settles)) = self.next(target, machine, waiting, now, wall) else {
            return;
        };
        if !send(target, &post, timings.http_timeout) {
            if self.backoff.is_none() {
                log("a post to the notify address was not delivered; will retry");
            }
            // Tried again as a failed batch of events is: ever less often.
            let wait = self.backoff.unwrap_or(timings.backoff_min);
            self.backoff = Some((wait * 2).min(timings.backoff_max));
            self.retry_at = Some(now + jittered(wait, timings.jitter));
            return;
        }
        match settles {
            Settles::Back => {
                log("told the notify address that Watch can be reached again");
                self.saved.back_after_s = None;
            }
            Settles::Announced => {
                log("told the notify address that Watch cannot be reached");
                self.saved.announced = true;
            }
            Settles::Told(key) => self.saved.told.push(key),
        }
        self.settle_retries();
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_written_in_utc_and_a_duration_as_watch_writes_one() {
        assert_eq!(clock(1_795_089_600_000 + 100 * 60_000), "13:40 UTC");
        assert_eq!(lasted(0), "0 seconds");
        assert_eq!(lasted(1), "1 second");
        assert_eq!(lasted(89), "89 seconds");
        assert_eq!(lasted(90), "2 minutes");
        assert_eq!(lasted(25 * 60), "25 minutes");
        assert_eq!(lasted(60 * 60), "60 minutes");
        assert_eq!(lasted(90 * 60), "2 hours");
        assert_eq!(lasted(47 * 3_600), "47 hours");
        assert_eq!(lasted(48 * 3_600), "2 days");
    }

    #[test]
    fn a_title_that_is_not_plain_ascii_is_sent_as_an_encoded_word() {
        assert_eq!(
            header_value("agentbox cannot reach Nucarium Watch"),
            "agentbox cannot reach Nucarium Watch"
        );
        assert_eq!(header_value("büro"), "=?UTF-8?B?YsO8cm8=?=");
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // A name cannot break out of its line.
        assert_eq!(name("agent\r\nbox\u{1b}"), "agentbox");
    }

    #[test]
    fn a_token_is_read_only_from_a_file_its_owner_alone_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        assert_eq!(token(None), Ok(None));
        assert_eq!(token(Some(&file)), Err(()));
        paths::write_private(&file, b"tk_abc123\n").unwrap();
        assert_eq!(token(Some(&file)), Ok(Some("tk_abc123".to_string())));
        paths::write_private(&file, b"two words\n").unwrap();
        assert_eq!(token(Some(&file)), Err(()));
        paths::write_private(&file, b"\n").unwrap();
        assert_eq!(token(Some(&file)), Err(()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            paths::write_private(&file, b"tk_abc123\n").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(token(Some(&file)), Err(()));
        }
    }
}
