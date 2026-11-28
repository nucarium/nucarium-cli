//! `nucleated look`: one look at an environment's driver, from inside the
//! environment's container.
//!
//! The daemon on the host never reads a driver itself. It runs the box's own
//! `nucleated`, mounted read-only at the same path in every container, with
//! one `podman exec` into the environment's container, and this is what runs
//! there. It sees what a hook in that container sees and nothing of any
//! other environment. It asks the driver, reads the folders the driver names
//! for repo names and branches, and prints one answer (see [`super::answer`])
//! that holds only what may cross to the host.
//!
//! It ends within its own time, whatever the driver does: when the time is
//! up it stops every program it started, prints that it ran out of time and
//! leaves, so the `podman exec` that started it returns by itself and leaves
//! nothing behind on either side. The host keeps a deadline of its own as
//! well, a little longer, and ends from the box a look the container held
//! past it (see [`super::cut_off`]).

use super::answer::{self, Answer};
use super::{Condition, Memory, INNER_LIMIT};
use crate::bounded;
use crate::envelope::valid_session_id;
use crate::settings;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The least time a look is given, whatever it is asked for.
const LEAST: Duration = Duration::from_millis(100);

/// Whether an answer has been printed. One answer, and only one, is printed.
static SAID: Mutex<bool> = Mutex::new(false);

/// Print `answer` unless one has been printed already.
fn say(answer: &Answer) {
    let mut said = SAID.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *said {
        return;
    }
    if let Ok(text) = serde_json::to_string(answer) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }
    *said = true;
}

/// What the host asked of this look.
pub struct Asked<'a> {
    /// The environment, by its config name.
    pub env: Option<&'a str>,
    /// How long the look may take, in milliseconds.
    pub within_ms: Option<u64>,
    /// The host's clock, in Unix milliseconds: the look's "now".
    pub now_ms: Option<u64>,
    /// What the last look left that the adapter may use: [`Memory`] as JSON,
    /// of which only the condition, when it was last good, the link
    /// reference and the facts are read.
    pub previous: Option<&'a str>,
    /// Whether the host sends names, and so asks for each session's name.
    pub names: bool,
}

/// Run one look and print its answer. Never fails, always prints one answer,
/// always exits 0 within the time it was given.
pub fn run(asked: &Asked) {
    let within = asked
        .within_ms
        .map(Duration::from_millis)
        .unwrap_or(INNER_LIMIT)
        .clamp(LEAST, INNER_LIMIT);
    std::thread::spawn(move || {
        std::thread::sleep(within);
        bounded::stop_all();
        say(&Answer::nothing("", Condition::Timeout));
        std::process::exit(0);
    });
    std::panic::set_hook(Box::new(|_| {}));
    let answer = std::panic::catch_unwind(|| one(asked))
        .unwrap_or_else(|_| Answer::nothing("", Condition::BadReply));
    say(&answer);
}

fn one(asked: &Asked) -> Answer {
    let Ok(settings) = settings::load() else {
        return Answer::nothing("", Condition::Absent);
    };
    let environment = asked.env.and_then(|name| settings.environment(name));
    let Some(environment) = environment else {
        return Answer::nothing("", Condition::Absent);
    };
    let Some(adapter) = environment.driver_adapter() else {
        return Answer::nothing("", Condition::Absent);
    };
    let now = asked
        .now_ms
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap_or_else(SystemTime::now);
    let previous = previous(adapter.name(), asked.previous);
    let result =
        crate::allowlist::with_names(asked.names, || adapter.observe(environment, &previous, now));
    answer::settle(&settings, environment, adapter.name(), result)
}

/// What the host handed over of the last look, as much of it as an adapter
/// reads, each part held to its own shape.
fn previous(driver: &str, text: Option<&str>) -> Memory {
    let mut memory = text
        .and_then(|text| serde_json::from_str::<Memory>(text).ok())
        .filter(|memory| memory.driver == driver)
        .unwrap_or_default();
    memory.driver = driver.to_string();
    memory.link_ref = memory.link_ref.filter(|id| valid_session_id(id));
    memory.facts.retain(|name, _| super::valid_key(name));
    memory.sessions.clear();
    memory.doubts.clear();
    memory.unheard.clear();
    memory
}

/// A look's token: sixteen lower-case hex digits. It tells one look from
/// another on the box, so that the host can end one it gave up on.
pub fn valid_token(token: &str) -> bool {
    crate::envelope::valid_folder_number(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_handed_over_of_the_last_look_is_held_to_its_shape() {
        let text = serde_json::json!({
            "driver": "t3code", "condition": "ok", "good_at_ms": 5,
            "link_ref": "the user typed this",
            "facts": { "described_by": 1, "the user typed this": 2 },
            "sessions": { "s": { "phase": "idle" } },
        })
        .to_string();
        let memory = previous("t3code", Some(&text));
        assert_eq!(memory.condition, Condition::Ok);
        assert_eq!(memory.good_at_ms, Some(5));
        assert_eq!(memory.link_ref, None);
        assert_eq!(memory.facts.len(), 1);
        assert!(memory.sessions.is_empty());
        // Another driver's memory says nothing about this one.
        let other = previous("orca", Some(&text));
        assert_eq!(other.driver, "orca");
        assert_eq!(other.good_at_ms, None);
        assert_eq!(previous("orca", Some("not json")).driver, "orca");
    }

    #[test]
    fn a_token_is_sixteen_hex_digits_and_nothing_else() {
        assert!(valid_token("0123456789abcdef"));
        assert!(!valid_token("0123456789ABCDEF"));
        assert!(!valid_token("--token"));
        assert!(!valid_token(""));
    }
}
