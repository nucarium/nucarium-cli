//! `nucleated pause` and `nucleated resume`: a maintenance pause announced
//! from the box.
//!
//! A pause is one small file holding the time it ends. While it stands, each
//! heartbeat carries that time, and Watch puts off its alerts for this
//! machine and its agent servers going dark. It ends by itself. It pauses
//! alerts and nothing else: no agent, no agent server and no housekeeping job
//! is touched, and the daemon keeps reporting as it did.

use crate::envelope::{rfc3339, unix_ms};
use crate::paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The longest pause. Watch keeps no more of one than this.
pub const LONGEST: Duration = crate::watch::MAX_PAUSE;

#[derive(Debug, Serialize, Deserialize)]
struct Pause {
    until_ms: u64,
    /// The same time, for a person reading the file.
    until: String,
    set_at: String,
}

pub fn file(state_dir: &Path) -> PathBuf {
    state_dir.join("pause.json")
}

/// Pause for `length` from `now`, replacing any pause already set. Returns
/// when it ends.
pub fn set(state_dir: &Path, length: Duration, now: SystemTime) -> Result<SystemTime> {
    if length.is_zero() {
        bail!("a pause of no time at all; give a duration such as \"2h\"");
    }
    if length > LONGEST {
        bail!("a pause lasts at most 24h");
    }
    let until = now + length;
    let pause = Pause {
        until_ms: unix_ms(until),
        until: rfc3339(until),
        set_at: rfc3339(now),
    };
    paths::write_private(&file(state_dir), serde_json::to_string(&pause)?.as_bytes())?;
    Ok(until)
}

/// End a pause now. Returns whether there was one.
pub fn clear(state_dir: &Path) -> Result<bool> {
    match fs::remove_file(file(state_dir)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("removing the pause"),
    }
}

/// When the pause in force ends, or nothing when the box is not paused. A
/// pause that has passed is over, and a file that is not a pause is none.
pub fn until(state_dir: &Path, now: SystemTime) -> Option<SystemTime> {
    let text = fs::read_to_string(file(state_dir)).ok()?;
    let pause: Pause = serde_json::from_str(&text).ok()?;
    let until = UNIX_EPOCH + Duration::from_millis(pause.until_ms);
    // A file changed by hand does not make a pause longer than one can be.
    (until > now).then(|| until.min(now + LONGEST))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn a_pause_stands_until_its_time_and_ends_by_itself() {
        let dir = tempfile::tempdir().unwrap();
        let now = at(1_795_089_600_000);
        let hours = |n: u64| Duration::from_secs(3_600 * n);
        assert_eq!(until(dir.path(), now), None);
        assert_eq!(set(dir.path(), hours(2), now).unwrap(), now + hours(2));
        assert_eq!(until(dir.path(), now), Some(now + hours(2)));
        assert_eq!(until(dir.path(), now + hours(1)), Some(now + hours(2)));
        assert_eq!(until(dir.path(), now + hours(2)), None);
        assert_eq!(until(dir.path(), now + hours(30)), None);
    }

    #[test]
    fn a_pause_is_more_than_nothing_and_at_most_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let now = at(1_795_089_600_000);
        assert!(set(dir.path(), Duration::ZERO, now).is_err());
        assert!(set(dir.path(), LONGEST + Duration::from_secs(1), now).is_err());
        assert_eq!(until(dir.path(), now), None);
        assert!(set(dir.path(), LONGEST, now).is_ok());
        // A later pause replaces the one before it.
        set(dir.path(), Duration::from_secs(60), now).unwrap();
        assert_eq!(until(dir.path(), now), Some(now + Duration::from_secs(60)));
    }

    #[test]
    fn resuming_ends_a_pause_and_is_harmless_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let now = at(1_795_089_600_000);
        assert!(!clear(dir.path()).unwrap());
        set(dir.path(), Duration::from_secs(600), now).unwrap();
        assert!(clear(dir.path()).unwrap());
        assert_eq!(until(dir.path(), now), None);
    }

    #[test]
    fn a_file_that_is_not_a_pause_pauses_nothing_and_one_edited_by_hand_is_a_day_at_most() {
        let dir = tempfile::tempdir().unwrap();
        let now = at(1_795_089_600_000);
        fs::write(file(dir.path()), "until next Tuesday").unwrap();
        assert_eq!(until(dir.path(), now), None);
        let far = unix_ms(now + Duration::from_secs(30 * 86_400));
        fs::write(
            file(dir.path()),
            format!(r#"{{"until_ms":{far},"until":"x","set_at":"y"}}"#),
        )
        .unwrap();
        assert_eq!(until(dir.path(), now), Some(now + LONGEST));
    }
}
