//! What one look at a driver hands the host: a small versioned answer.
//!
//! The look runs inside the environment's container (see [`super::inside`]),
//! where the driver's own answer and the folders it names stay. What crosses
//! to the host is this, and only this: ids, fixed words, numbers, repo names
//! and branches held to Watch's own patterns, a session folder's name, and
//! numbers made from folders. No path, no title, no message, and a session's
//! name only from a look the host asked for names.
//!
//! The host trusts none of it. [`Answer::check`] holds every field to a fixed
//! list, a pattern or a cap, and an answer with one field out of line is no
//! answer at all. Nothing in it is a path the host opens.

use super::{folder_number, resolved, uncounted, valid_key, Condition, Look, Phase, CLAIM_FOLDERS};
use crate::envelope::{
    valid_branch, valid_error_kind, valid_folder_number, valid_repo, valid_session_id,
    valid_session_name,
};
use crate::settings::{Environment, Settings};
use crate::worktrees::valid_folder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

/// The version of the answer this build writes and reads. An answer of any
/// other version is "unsupported".
pub const VERSION: u32 = 1;
/// At most this many sessions in one answer. A driver that lists more has
/// its list cut short, and a cut list ends nothing for being incomplete.
pub const MAX_SESSIONS: usize = 256;
/// At most this many bytes of answer are read. One that runs on is no
/// answer.
pub const MAX_ANSWER: u64 = 1024 * 1024;
/// At most this many facts an adapter asks to have handed back.
pub const MAX_FACTS: usize = 16;
/// At most this many repos for one session, as an event carries.
pub const MAX_REPOS: usize = 32;
/// The most worktrees an answer may say the count of unsaved work leaves out.
pub const MAX_UNCOUNTED: u64 = 4096;

/// One look's answer, as it crosses from the container to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub version: u32,
    /// The adapter that looked, by its config name.
    pub driver: String,
    /// `ok`, or why the look found nothing.
    pub condition: Condition,
    /// False when the list was cut short.
    #[serde(default)]
    pub complete: bool,
    /// The driver's own id for its installation, for its link template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_ref: Option<String>,
    /// What the adapter asks to have handed back at the next look: its own
    /// names and numbers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, u64>,
    /// How many worktrees the driver listed that the count of its unsaved
    /// work does not reach.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncounted_worktrees: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<Seen>,
}

/// One session the driver listed, as the host may hear of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seen {
    /// The driver's id for the session: `[A-Za-z0-9._:-]{1,128}`.
    pub id: String,
    /// Nothing is "no opinion".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// The session's name, only from a look the host asked for names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// When a usage limit resets, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_resets_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    /// The repos of the folder the session works in, found in the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The name of the nucleus session folder the session works in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The number of the folder the driver says the session works in, made
    /// as a hook's claim makes one; nothing when the driver names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// The numbers of the folders above that one that are still inside the
    /// environment's root, nearest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub within: Vec<String>,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

impl Seen {
    /// A session known by its id and nothing else.
    pub fn bare(id: &str) -> Seen {
        Seen {
            id: id.to_string(),
            ..Seen::default()
        }
    }

    /// Whether a claim whose hook works in the folders `numbers` names was
    /// made from where the driver says this session works. A driver that
    /// names no folder is taken at the claim's word. One that names a folder
    /// is agreed with by a hook whose session works in that folder or inside
    /// it, or in a folder above it that is still within the driver
    /// environment's own root: a driver may name the folder of whatever its
    /// agent is running at the moment. A claim that names no folder, or
    /// another, is some other session's.
    pub fn same_place(&self, numbers: &[String]) -> bool {
        let Some(folder) = &self.folder else {
            return true;
        };
        let Some(hooks) = numbers.first() else {
            return false;
        };
        numbers.contains(folder) || self.within.contains(hooks)
    }

    fn fits(&self) -> bool {
        valid_session_id(&self.id)
            && self.name.as_deref().is_none_or(valid_session_name)
            && self.error_kind.as_deref().is_none_or(valid_error_kind)
            && self.claim_key.as_deref().is_none_or(valid_key)
            && self
                .agent_session_id
                .as_deref()
                .is_none_or(valid_session_id)
            && self.repos.len() <= MAX_REPOS
            && self.repos.iter().all(|repo| valid_repo(repo))
            && self.branch.as_deref().is_none_or(valid_branch)
            && self.workspace.as_deref().is_none_or(valid_folder)
            && self.folder.as_deref().is_none_or(valid_folder_number)
            && self.within.len() <= CLAIM_FOLDERS
            && self.within.iter().all(|number| valid_folder_number(number))
    }
}

impl Answer {
    /// An answer that says only why the look found nothing.
    pub fn nothing(driver: &str, condition: Condition) -> Answer {
        Answer {
            version: VERSION,
            driver: driver.to_string(),
            condition,
            complete: false,
            link_ref: None,
            facts: BTreeMap::new(),
            uncounted_worktrees: 0,
            sessions: Vec::new(),
        }
    }

    /// The answer as the host takes it from a look of the driver `driver`:
    /// every field within its list, pattern or cap, or no answer at all. A
    /// look that found nothing says why with one of the fixed words, and
    /// nothing else in it is read.
    pub fn check(self, driver: &str) -> Result<Answer, Condition> {
        if self.version != VERSION {
            return Err(Condition::Unsupported);
        }
        if self.condition != Condition::Ok {
            return Err(self.condition);
        }
        let fits = self.driver == driver
            && self.link_ref.as_deref().is_none_or(valid_session_id)
            && self.facts.len() <= MAX_FACTS
            && self.facts.keys().all(|name| valid_key(name))
            && self.uncounted_worktrees <= MAX_UNCOUNTED
            && self.sessions.len() <= MAX_SESSIONS
            && self.sessions.iter().all(Seen::fits);
        if fits {
            Ok(self)
        } else {
            Err(Condition::BadReply)
        }
    }
}

/// Read one answer: a JSON document and nothing after it but blank space.
/// The caller caps how much is read.
pub fn read(stream: &mut dyn Read) -> Option<Answer> {
    serde_json::from_reader(stream).ok()
}

/// Inside the container: turn one look into the answer the host is given.
/// Each session's folder is read here, for its repos, its branch and its
/// session folder, and for the numbers a claim is matched against, and goes
/// no further. Only what fits the answer's patterns is kept.
pub fn settle(
    settings: &Settings,
    environment: &Environment,
    driver: &str,
    result: Result<Look, Condition>,
) -> Answer {
    let look = match result {
        Ok(look) => look,
        Err(condition) => return Answer::nothing(driver, condition),
    };
    let mut answer = Answer::nothing(driver, Condition::Ok);
    answer.complete = look.complete;
    answer.link_ref = look
        .link_ref
        .filter(|reference| valid_session_id(reference));
    answer.facts = look
        .facts
        .iter()
        .filter(|(name, _)| valid_key(name))
        .take(MAX_FACTS)
        .map(|(name, value)| (name.to_string(), *value))
        .collect();
    answer.uncounted_worktrees =
        uncounted(settings, environment, &look.worktrees).min(MAX_UNCOUNTED);
    let root = resolved(&environment.root);
    let mut ids = BTreeSet::new();
    for observed in look.sessions {
        if !valid_session_id(&observed.id) || ids.contains(&observed.id) {
            continue;
        }
        if answer.sessions.len() == MAX_SESSIONS {
            // What was left out says nothing about whether it is over.
            answer.complete = false;
            break;
        }
        ids.insert(observed.id.clone());
        let place = crate::hook::place(environment, observed.cwd.as_deref());
        let folder = observed.cwd.as_deref().map(resolved);
        let within = folder
            .iter()
            .flat_map(|folder| {
                folder
                    .ancestors()
                    .skip(1)
                    .take(CLAIM_FOLDERS)
                    .take_while(|above| above.starts_with(&root))
                    .map(folder_number)
                    .collect::<Vec<_>>()
            })
            .collect();
        answer.sessions.push(Seen {
            folder: folder.as_deref().map(folder_number),
            within,
            repos: place
                .repos
                .into_iter()
                .filter(|repo| valid_repo(repo))
                .take(MAX_REPOS)
                .collect(),
            branch: place.branch.filter(|branch| valid_branch(branch)),
            workspace: place.workspace.filter(|name| valid_folder(name)),
            phase: observed.phase,
            name: observed.name.filter(|name| valid_session_name(name)),
            error_kind: observed.error_kind.filter(|kind| valid_error_kind(kind)),
            limit_resets_at_ms: observed.limit_resets_at.map(crate::envelope::unix_ms),
            activity: observed.activity,
            turn: observed.turn,
            claim_key: observed.claim_key.filter(|key| valid_key(key)),
            agent_session_id: observed.agent_session_id.filter(|id| valid_session_id(id)),
            id: observed.id,
        });
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> serde_json::Value {
        json!({
            "version": VERSION, "driver": "orca", "condition": "ok", "complete": true,
            "link_ref": "env-1", "facts": { "described_by": 7 }, "uncounted_worktrees": 1,
            "sessions": [{
                "id": "s-1", "phase": "needs_permission", "error_kind": "rate_limit",
                "limit_resets_at_ms": 1_795_089_600_000_u64, "activity": 3, "turn": 2,
                "claim_key": "wt-0123456789abcdef", "agent_session_id": "a-1",
                "repos": ["acme/web"], "branch": "fix/login", "workspace": "fix-login",
                "folder": "0123456789abcdef", "within": ["fedcba9876543210"],
            }],
        })
    }

    fn checked(value: &serde_json::Value) -> Result<Answer, Condition> {
        let text = value.to_string();
        match read(&mut text.as_bytes()) {
            Some(answer) => answer.check("orca"),
            None => Err(Condition::BadReply),
        }
    }

    #[test]
    fn an_answer_whose_every_field_fits_is_taken_as_it_is() {
        let answer = checked(&good()).unwrap();
        assert_eq!(answer.sessions.len(), 1);
        assert_eq!(answer.sessions[0].repos, ["acme/web"]);
        assert_eq!(answer.facts.get("described_by"), Some(&7));
    }

    #[test]
    fn one_field_out_of_line_and_the_answer_is_no_answer() {
        let words = "the user typed this";
        let mut cases: Vec<(&str, serde_json::Value)> = Vec::new();
        for field in [
            "id",
            "error_kind",
            "claim_key",
            "agent_session_id",
            "branch",
            "workspace",
            "folder",
        ] {
            let mut answer = good();
            answer["sessions"][0][field] = json!(words);
            cases.push((field, answer));
        }
        let mut answer = good();
        answer["sessions"][0]["repos"] = json!([words]);
        cases.push(("repos", answer));
        let mut answer = good();
        answer["sessions"][0]["within"] = json!([words]);
        cases.push(("within", answer));
        let mut answer = good();
        answer["sessions"][0]["phase"] = json!(words);
        cases.push(("phase", answer));
        let mut answer = good();
        answer["sessions"][0]["cwd"] = json!("/home/agent/elsewhere");
        cases.push(("a field of its own", answer));
        let mut answer = good();
        answer["link_ref"] = json!(words);
        cases.push(("link_ref", answer));
        let mut answer = good();
        answer["facts"] = json!({ words: 1 });
        cases.push(("facts", answer));
        let mut answer = good();
        answer["driver"] = json!("t3code");
        cases.push(("another driver", answer));
        let mut answer = good();
        answer["uncounted_worktrees"] = json!(MAX_UNCOUNTED + 1);
        cases.push(("uncounted_worktrees", answer));
        let mut answer = good();
        answer["sessions"][0]["repos"] = json!(vec!["web"; MAX_REPOS + 1]);
        cases.push(("too many repos", answer));
        let mut answer = good();
        answer["sessions"][0]["within"] = json!(vec!["0123456789abcdef"; CLAIM_FOLDERS + 1]);
        cases.push(("too many folders", answer));
        let mut answer = good();
        answer["sessions"] = json!(vec![good()["sessions"][0].clone(); MAX_SESSIONS + 1]);
        cases.push(("too many sessions", answer));
        let mut answer = good();
        answer["condition"] = json!(words);
        cases.push(("condition", answer));
        for (what, answer) in cases {
            assert_eq!(checked(&answer).err(), Some(Condition::BadReply), "{what}");
        }
        // Trailing words after the document are not part of an answer.
        let text = format!("{} {words}", good());
        assert!(read(&mut text.as_bytes()).is_none());
    }

    #[test]
    fn a_look_that_found_nothing_says_why_and_nothing_else_is_read() {
        let answer = json!({
            "version": VERSION, "driver": "", "condition": "timeout",
        });
        assert_eq!(checked(&answer).err(), Some(Condition::Timeout));
        let mut newer = good();
        newer["version"] = json!(VERSION + 1);
        assert_eq!(checked(&newer).err(), Some(Condition::Unsupported));
    }

    #[test]
    fn a_claim_agrees_with_a_session_only_from_where_the_driver_places_it() {
        let a = "0123456789abcdef".to_string();
        let b = "fedcba9876543210".to_string();
        let c = "00000000000000aa".to_string();
        let mut seen = Seen::bare("s");
        // A driver that names no folder: the claim's word is taken.
        assert!(seen.same_place(&[]));
        seen.folder = Some(a.clone());
        seen.within = vec![b.clone()];
        // The hook works in that folder, or inside it.
        assert!(seen.same_place(std::slice::from_ref(&a)));
        assert!(seen.same_place(&[c.clone(), a.clone()]));
        // In a folder above it, within the root.
        assert!(seen.same_place(std::slice::from_ref(&b)));
        // Somewhere else, or nowhere said.
        assert!(!seen.same_place(std::slice::from_ref(&c)));
        assert!(!seen.same_place(&[]));
        assert!(!seen.same_place(&[c, b]));
    }
}
