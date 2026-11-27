//! What `nucleated repos` hands the host: a small versioned answer.
//!
//! It runs inside the environment's container (see [`super::inside`]), where
//! git reads what the container wrote. What crosses to the host is this, and
//! only this: fixed words, numbers, commit ids, repo names, folder names and
//! branches held to Watch's own patterns, and `owner/name` for a repo on
//! GitHub. No path, no file's name or contents, no commit's subject, nothing
//! git said in its own words.
//!
//! The host trusts none of it. [`Answer::check`] holds every field to a fixed
//! list, a pattern or a cap, and an answer with one field out of line is no
//! answer at all. Nothing in it is a path the host opens.

use crate::driver::Condition;
use crate::envelope::{valid_branch, valid_folder_number};
use crate::git::Reason;
use crate::reporter::commit_id;
use crate::workspace::valid_slug;
use crate::worktrees::{reportable_branch, valid_folder, valid_repo};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Read;

/// The version of the answer this build writes and reads. An answer of any
/// other version is "unsupported".
pub const VERSION: u32 = 1;
/// At most this many bytes of answer are read. One that runs on is no
/// answer.
pub const MAX_ANSWER: u64 = 2 * 1024 * 1024;
/// At most this many session folders in one answer, as Watch takes them.
pub const MAX_FOLDERS: usize = crate::worktrees::MAX_FOLDERS;
/// At most this many repos, by the name they are reported by, in one
/// folder, as Watch takes them.
pub const MAX_FOLDER_REPOS: usize = crate::worktrees::MAX_REPOS;
/// At most this many repos a folder's manifest names, as an event carries.
pub const MAX_LABELS: usize = crate::driver::answer::MAX_REPOS;
/// At most this many worktrees in one answer, in its folders and outside
/// them together.
pub const MAX_WORKTREES: usize = 4096;
/// At most this many of a nucleus's repos.
pub const MAX_REPOS: usize = 4096;
/// The most any count in an answer may be.
pub const MAX_COUNT: u32 = 100_000;
/// The latest time an answer may name, in Unix milliseconds: the year 3000.
pub const MAX_TIME_MS: u64 = 32_503_680_000_000;
/// Where a session's subagents keep their worktrees, as a key starts.
const SUBWORKTREES: &str = ".subworktrees/";

/// What the host asked `nucleated repos` to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Read what every worktree holds, or one session folder's. Nothing is
    /// changed, nothing is fetched, GitHub is not asked.
    Census,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Census => "census",
        }
    }

    pub fn parse(word: &str) -> Option<Op> {
        [Op::Census].into_iter().find(|op| op.as_str() == word)
    }
}

/// One answer, as it crosses from the container to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub version: u32,
    pub op: Op,
    /// `ok`, or why nothing could be done: the look's own words.
    pub condition: Condition,
    /// False when the answer was cut short, to keep within its caps.
    #[serde(default)]
    pub complete: bool,
    /// Something of the nucleus's was not looked at because it is outside
    /// the environment's own folders: a count without it is not known.
    #[serde(default, skip_serializing_if = "is_false")]
    pub left_out: bool,
    /// Something of the nucleus's could not be read: a count without it is
    /// not known.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unreadable: bool,
    /// The nucleus's repos, each with why git is not run in it when it is
    /// not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<Repo>,
    /// Its session folders, or the one asked about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<Folder>,
    /// The worktrees of its repos that are in no session folder, or a
    /// driver's worktrees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loose: Vec<Worktree>,
}

/// One of the nucleus's repos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    /// Its name, as its folder has it.
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Reason>,
}

/// One session folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Folder {
    /// Its name under `.worktrees/`.
    pub folder: String,
    /// It is not there: what existed only in it is gone with it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub gone: bool,
    /// Its manifest, one of its worktrees, or the folder itself could not be
    /// read, or it holds more repos than are told: nothing of it is known.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unreadable: bool,
    /// Something at its root that nucleate did not put there.
    #[serde(default, skip_serializing_if = "is_false")]
    pub other_files: bool,
    /// Its `.subworktrees` holds something that is no worktree.
    #[serde(default, skip_serializing_if = "is_false")]
    pub leftovers: bool,
    /// When the folder or its manifest last changed, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touched_ms: Option<u64>,
    /// The newest change to a file git tracks or would track in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_change_ms: Option<u64>,
    /// A number made from what it holds, which changes when that does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<String>,
    /// The branch its manifest names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The repos its manifest names, by the names they go by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktrees: Vec<Worktree>,
}

/// One worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    /// The name it has in its session folder: a repo's, or
    /// `.subworktrees/<name>` for a subagent's. Nothing for one in no
    /// session folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The name of the repo it is a worktree of, as its folder has it.
    pub repo: String,
    /// The name it is reported by: its repo's label, else its key.
    pub label: String,
    /// The branch checked out, blank when `HEAD` is detached or the branch
    /// is not one Watch takes.
    #[serde(default)]
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Reason>,
    /// What git says of it; nothing when git was not run there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<Facts>,
    /// The commits made on it since the commit its manifest says it started
    /// from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<u32>,
    /// When its newest commit was made, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_s: Option<u64>,
}

/// What git says of a worktree, as [`crate::workspace::Facts`] has it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    pub uncommitted: bool,
    pub tracked: bool,
    #[serde(default)]
    pub ahead: u32,
    pub on_remote: bool,
    #[serde(default)]
    pub alone: u32,
    pub head: String,
    pub on_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

impl Facts {
    /// The facts as they cross, every count held to its cap.
    pub fn of(facts: &crate::workspace::Facts) -> Facts {
        Facts {
            uncommitted: facts.uncommitted,
            tracked: facts.tracked,
            ahead: facts.ahead.min(MAX_COUNT),
            on_remote: facts.on_remote,
            alone: facts.alone.min(MAX_COUNT),
            head: facts.head.clone(),
            on_default: facts.on_default,
            slug: facts.slug.clone().filter(|slug| valid_slug(slug)),
        }
    }

    /// The facts as the host's rules read them.
    pub fn taken(&self) -> crate::workspace::Facts {
        crate::workspace::Facts {
            uncommitted: self.uncommitted,
            tracked: self.tracked,
            ahead: self.ahead,
            on_remote: self.on_remote,
            alone: self.alone,
            head: self.head.clone(),
            on_default: self.on_default,
            slug: self.slug.clone(),
            changes: 0,
        }
    }

    fn fits(&self) -> bool {
        commit_id(&self.head)
            && self.ahead <= MAX_COUNT
            && self.alone <= MAX_COUNT
            && self.slug.as_deref().is_none_or(valid_slug)
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A repo's name as its folder has it: one plain name, never `.` or `..`.
pub fn valid_name(name: &str) -> bool {
    valid_folder(name)
}

/// The name a repo is reported by, as Watch takes one: `owner/name`, or a
/// name, each part a plain one.
pub fn valid_label(label: &str) -> bool {
    valid_repo(label) && label.split('/').all(valid_folder)
}

/// A worktree's name in its session folder: a repo's folder there, or one
/// of its subagents'.
pub fn valid_key(key: &str) -> bool {
    valid_folder(key) || key.strip_prefix(SUBWORKTREES).is_some_and(valid_folder)
}

impl Worktree {
    fn fits(&self, in_folder: bool) -> bool {
        self.key.is_some() == in_folder
            && self.key.as_deref().is_none_or(valid_key)
            && valid_name(&self.repo)
            && valid_label(&self.label)
            && (self.branch.is_empty() || reportable_branch(&self.branch))
            && self.facts.as_ref().is_none_or(Facts::fits)
            && self.commits.is_none_or(|commits| commits <= MAX_COUNT)
            && self
                .last_commit_s
                .is_none_or(|seconds| seconds <= MAX_TIME_MS / 1000)
    }
}

impl Folder {
    fn fits(&self) -> bool {
        let named: BTreeSet<&str> = self.worktrees.iter().map(|w| w.label.as_str()).collect();
        valid_folder(&self.folder)
            && !(self.gone && (self.unreadable || !self.worktrees.is_empty()))
            && self.stamp.as_deref().is_none_or(valid_folder_number)
            && self.touched_ms.is_none_or(|ms| ms <= MAX_TIME_MS)
            && self.last_change_ms.is_none_or(|ms| ms <= MAX_TIME_MS)
            && self.branch.as_deref().is_none_or(valid_branch)
            && self.labels.len() <= MAX_LABELS
            && self.labels.iter().all(|label| valid_label(label))
            && named.len() <= MAX_FOLDER_REPOS
            && self.worktrees.iter().all(|worktree| worktree.fits(true))
    }
}

/// What the host asked, which the answer must be an answer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asked<'a> {
    pub op: Op,
    /// The one session folder asked about.
    pub folder: Option<&'a str>,
    /// A driver's worktrees were asked about, which are in no folder.
    pub driver: bool,
}

impl Answer {
    /// An answer that says only why nothing could be done.
    pub fn nothing(op: Op, condition: Condition) -> Answer {
        Answer {
            version: VERSION,
            op,
            condition,
            complete: false,
            left_out: false,
            unreadable: false,
            repos: Vec::new(),
            folders: Vec::new(),
            loose: Vec::new(),
        }
    }

    /// Every worktree the answer tells of, in its folders and outside them.
    pub fn worktrees(&self) -> impl Iterator<Item = &Worktree> {
        self.folders
            .iter()
            .flat_map(|folder| &folder.worktrees)
            .chain(&self.loose)
    }

    /// The answer as the host takes it, to what it `asked`: every field
    /// within its list, pattern or cap, or no answer at all. One that says
    /// nothing could be done says why with one of the fixed words, and
    /// nothing else in it is read.
    pub fn check(self, asked: &Asked) -> Result<Answer, Condition> {
        if self.version != VERSION {
            return Err(Condition::Unsupported);
        }
        if self.condition != Condition::Ok {
            return Err(self.condition);
        }
        let names: BTreeSet<&str> = self.folders.iter().map(|f| f.folder.as_str()).collect();
        let shape = match (asked.folder, asked.driver) {
            (Some(folder), _) => {
                self.folders.len() == 1
                    && self.folders[0].folder == folder
                    && self.repos.is_empty()
                    && self.loose.is_empty()
            }
            (None, true) => self.folders.is_empty() && self.repos.is_empty(),
            (None, false) => true,
        };
        let fits = self.op == asked.op
            && shape
            && names.len() == self.folders.len()
            && self.repos.len() <= MAX_REPOS
            && self.repos.iter().all(|repo| valid_name(&repo.repo))
            && self.folders.len() <= MAX_FOLDERS
            && self.folders.iter().all(Folder::fits)
            && self.loose.iter().all(|worktree| worktree.fits(false))
            && self.worktrees().count() <= MAX_WORKTREES;
        match fits {
            true => Ok(self),
            false => Err(Condition::BadReply),
        }
    }
}

/// Read one answer: a JSON document and nothing after it but blank space.
/// The caller caps how much is read.
pub fn read(stream: &mut dyn Read) -> Option<Answer> {
    serde_json::from_reader(stream).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn worktree(key: Option<&str>) -> Value {
        let mut worktree = json!({
            "repo": "web", "label": "acme/web", "branch": "workspace/fix-login",
            "facts": {
                "uncommitted": true, "tracked": true, "ahead": 2, "on_remote": false,
                "alone": 0, "head": HEAD, "on_default": false, "slug": "acme/web",
            },
            "commits": 2, "last_commit_s": 1_795_089_600_u64,
        });
        if let Some(key) = key {
            worktree["key"] = json!(key);
        }
        worktree
    }

    fn good() -> Value {
        json!({
            "version": VERSION, "op": "census", "condition": "ok", "complete": true,
            "repos": [{ "repo": "web" }, { "repo": "api", "refused": "config" }],
            "folders": [{
                "folder": "fix-login", "other_files": true, "touched_ms": 5,
                "last_change_ms": 6, "stamp": "0123456789abcdef",
                "branch": "workspace/fix-login", "labels": ["acme/web"],
                "worktrees": [worktree(Some("web")), worktree(Some(".subworktrees/agent-1"))],
            }, { "folder": "gone-one", "gone": true }],
            "loose": [worktree(None)],
        })
    }

    const CENSUS: Asked = Asked {
        op: Op::Census,
        folder: None,
        driver: false,
    };

    fn checked(value: &Value, asked: &Asked) -> Result<Answer, Condition> {
        let text = value.to_string();
        match read(&mut text.as_bytes()) {
            Some(answer) => answer.check(asked),
            None => Err(Condition::BadReply),
        }
    }

    #[test]
    fn an_answer_whose_every_field_fits_is_taken_as_it_is() {
        let answer = checked(&good(), &CENSUS).unwrap();
        assert_eq!(answer.folders.len(), 2);
        assert_eq!(answer.worktrees().count(), 3);
        assert_eq!(answer.repos[1].refused, Some(Reason::Config));
    }

    #[test]
    fn one_field_out_of_line_and_the_answer_is_no_answer() {
        let words = "the user typed this";
        let mut cases: Vec<(String, Value)> = Vec::new();
        let mut case = |what: &str, change: &dyn Fn(&mut Value)| {
            let mut answer = good();
            change(&mut answer);
            cases.push((what.to_string(), answer));
        };
        case("op", &|a| a["op"] = json!("snapshot"));
        case("an unknown field", &|a| a["path"] = json!("/home/agent"));
        case("a repo", &|a| a["repos"][0]["repo"] = json!("../x"));
        case("a refusal", &|a| a["repos"][0]["refused"] = json!(words));
        case("a folder", &|a| a["folders"][0]["folder"] = json!(words));
        case("a folder up", &|a| a["folders"][0]["folder"] = json!(".."));
        case("a stamp", &|a| a["folders"][0]["stamp"] = json!(words));
        case("a manifest branch", &|a| {
            a["folders"][0]["branch"] = json!(words)
        });
        case("a label", &|a| a["folders"][0]["labels"] = json!([words]));
        case("too many labels", &|a| {
            a["folders"][0]["labels"] = json!(vec!["web"; MAX_LABELS + 1])
        });
        case("a gone folder with worktrees", &|a| {
            a["folders"][1]["worktrees"] = json!([worktree(Some("web"))])
        });
        case("the same folder twice", &|a| {
            a["folders"][1] = a["folders"][0].clone()
        });
        for field in ["key", "repo", "label"] {
            case(field, &|a| {
                a["folders"][0]["worktrees"][0][field] = json!(words)
            });
        }
        case("a key up", &|a| {
            a["folders"][0]["worktrees"][0]["key"] = json!(".subworktrees/..")
        });
        case("no key in a folder", &|a| {
            a["folders"][0]["worktrees"][0]
                .as_object_mut()
                .unwrap()
                .remove("key");
        });
        case("a key outside any folder", &|a| {
            a["loose"][0]["key"] = json!("web")
        });
        case("a branch with spaces", &|a| {
            a["loose"][0]["branch"] = json!("fix login")
        });
        case("a branch with a control character", &|a| {
            a["loose"][0]["branch"] = json!("fix\u{7}login")
        });
        case("a head", &|a| {
            a["loose"][0]["facts"]["head"] = json!("HEAD~1")
        });
        case("a slug", &|a| {
            a["loose"][0]["facts"]["slug"] = json!("a/b/c")
        });
        case("a count over its cap", &|a| {
            a["loose"][0]["facts"]["ahead"] = json!(MAX_COUNT + 1)
        });
        case("commits over the cap", &|a| {
            a["loose"][0]["commits"] = json!(MAX_COUNT + 1)
        });
        case("a time past any", &|a| {
            a["folders"][0]["last_change_ms"] = json!(u64::MAX)
        });
        case("a commit time past any", &|a| {
            a["loose"][0]["last_commit_s"] = json!(u64::MAX)
        });
        case("a fact of its own", &|a| {
            a["loose"][0]["facts"]["subject"] = json!(words)
        });
        case("a condition", &|a| a["condition"] = json!(words));
        case("too many folders", &|a| {
            let folders: Vec<Value> = (0..=MAX_FOLDERS)
                .map(|n| json!({ "folder": format!("f{n}") }))
                .collect();
            a["folders"] = json!(folders);
        });
        case("too many repos in a folder", &|a| {
            let worktrees: Vec<Value> = (0..=MAX_FOLDER_REPOS)
                .map(|n| {
                    let mut worktree = worktree(Some(&format!("r{n}")));
                    worktree["label"] = json!(format!("r{n}"));
                    worktree
                })
                .collect();
            a["folders"][0]["worktrees"] = json!(worktrees);
        });
        case("too many worktrees", &|a| {
            a["loose"] = json!(vec![worktree(None); MAX_WORKTREES]);
        });
        for (what, answer) in cases {
            assert_eq!(
                checked(&answer, &CENSUS).err(),
                Some(Condition::BadReply),
                "{what}"
            );
        }
        // Trailing words after the document are not part of an answer.
        let text = format!("{} {words}", good());
        assert!(read(&mut text.as_bytes()).is_none());
    }

    #[test]
    fn an_answer_says_why_nothing_was_done_and_nothing_else_is_read() {
        let mut answer = json!({ "version": VERSION, "op": "census", "condition": "timeout" });
        assert_eq!(checked(&answer, &CENSUS).err(), Some(Condition::Timeout));
        answer["version"] = json!(VERSION + 1);
        assert_eq!(
            checked(&answer, &CENSUS).err(),
            Some(Condition::Unsupported)
        );
    }

    #[test]
    fn an_answer_about_one_folder_tells_of_that_folder_and_nothing_else() {
        let asked = Asked {
            op: Op::Census,
            folder: Some("fix-login"),
            driver: false,
        };
        let mut one = good();
        one["folders"] = json!([good()["folders"][0].clone()]);
        one.as_object_mut().unwrap().remove("repos");
        one.as_object_mut().unwrap().remove("loose");
        assert!(checked(&one, &asked).is_ok());
        let other = Asked {
            folder: Some("another"),
            ..asked
        };
        assert_eq!(checked(&one, &other).err(), Some(Condition::BadReply));
        assert_eq!(checked(&good(), &asked).err(), Some(Condition::BadReply));
        // A driver's worktrees are in no folder.
        let driver = Asked {
            folder: None,
            driver: true,
            ..asked
        };
        assert_eq!(checked(&good(), &driver).err(), Some(Condition::BadReply));
        let mut loose = good();
        loose.as_object_mut().unwrap().remove("folders");
        loose.as_object_mut().unwrap().remove("repos");
        assert!(checked(&loose, &driver).is_ok());
    }
}
