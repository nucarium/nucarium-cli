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
    /// Move repos to their owner folders, then fetch each one.
    Refresh,
    /// Push what is not committed to a hidden ref.
    Snapshot,
    /// Push one session folder's branches: Watch's Push.
    Push,
    /// Remove session folders: cleanup's removal, and Watch's Clean up.
    Remove,
}

const OPS: [Op; 5] = [Op::Census, Op::Refresh, Op::Snapshot, Op::Push, Op::Remove];

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Census => "census",
            Op::Refresh => "refresh",
            Op::Snapshot => "snapshot",
            Op::Push => "push",
            Op::Remove => "remove",
        }
    }

    pub fn parse(word: &str) -> Option<Op> {
        OPS.into_iter().find(|op| op.as_str() == word)
    }

    /// Whether it changes anything. One that does and is cut short says
    /// what it did; what it does not list was not done.
    pub fn writes(self) -> bool {
        self != Op::Census
    }
}

/// Why something was done or not done, or why it failed: one fixed word.
/// Nothing git, GitHub or a repository said is ever passed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Word {
    /// Its git config sets a key Nucarium does not allow.
    Config,
    /// A git pointer in it leads outside the nucleus's repos.
    Pointer,
    /// It could not be read.
    Unreadable,
    /// It holds a submodule.
    Submodule,
    NotOnABranch,
    NothingUncommitted,
    BeingWrittenTo,
    AlreadySnapshotted,
    OwnedByADriver,
    /// Not under the nucleus's `.worktrees`.
    NotOurs,
    /// The environment's network does not reach GitHub: its proxy refused.
    NoNetwork,
    TimedOut,
    /// The branch on GitHub has commits this one does not.
    Rejected,
    /// GitHub did not accept the sign-in.
    SignIn,
    /// GitHub could not be reached.
    Unreachable,
    /// GitHub refused it.
    Refused,
    /// Git failed for some other reason.
    Git,
    /// Git could not be run there.
    CannotRun,
    /// Its branch has a name that cannot be used safely.
    BadBranch,
    /// It holds another git repository, which cannot be backed up.
    NestedRepo,
    /// The folder is gone.
    Gone,
    /// The folder is no session folder.
    NotASession,
    /// The box leaves the folder alone.
    LeftAlone,
    /// A process is working in the folder.
    InUse,
    /// The folder changed since the host looked at it.
    Changed,
    /// It holds work that is not on GitHub.
    Unsaved,
    /// It holds files outside its repos.
    OtherFiles,
    /// A backup could not be made.
    NotMade,
    /// A backup could not be pushed.
    NotPushed,
    /// It could not be removed.
    NotRemoved,
}

impl Word {
    /// The word for a refusal by the vetting.
    pub fn of(reason: Reason) -> Word {
        match reason {
            Reason::Config => Word::Config,
            Reason::Pointer => Word::Pointer,
            Reason::Unreadable => Word::Unreadable,
            Reason::Submodule => Word::Submodule,
        }
    }

    /// The vetting's refusal this word is, if it is one.
    pub fn refusal(self) -> Option<Reason> {
        match self {
            Word::Config => Some(Reason::Config),
            Word::Pointer => Some(Reason::Pointer),
            Word::Unreadable => Some(Reason::Unreadable),
            Word::Submodule => Some(Reason::Submodule),
            _ => None,
        }
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
    /// Refresh: how many repos were moved to their owner folders.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub moved: u32,
    /// Why the whole of it was not done, or, for a refresh, why the move
    /// failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<Word>,
    /// Refresh: each repo fetched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetched: Vec<Fetched>,
    /// Snapshot: each worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<Snapshot>,
    /// Push: each repo of the folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pushes: Vec<Pushed>,
    /// Remove: each folder, as it came out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removals: Vec<Removal>,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// One repo a refresh fetched, or did not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fetched {
    pub repo: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Word>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Reason>,
}

/// What a snapshot did with one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapAction {
    Snapshot,
    WouldSnapshot,
    Skipped,
    Refused,
    Failed,
}

/// One worktree a snapshot looked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// The repo it is a worktree of, as its folder has it.
    pub repo: String,
    /// The session folder it is in, if it is in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Its branch, blank when there is none to name.
    #[serde(default)]
    pub branch: String,
    pub action: SnapAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Word>,
    /// The tree that was pushed, or would be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The ref it was pushed to, or would be.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// One repo of a folder Push looked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pushed {
    /// The name it is reported by.
    pub repo: String,
    /// Commits pushed.
    #[serde(default)]
    pub pushed: u32,
    #[serde(default)]
    pub uncommitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Word>,
}

/// How a removal came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Removed,
    /// Not removed, for `why`.
    Kept,
    Failed,
    /// It was gone already.
    Gone,
}

/// One folder a removal was asked about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Removal {
    pub folder: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<Word>,
    /// The repo `why` is about, by the name it is reported by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// What its repos hold that is not on GitHub.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holding: Vec<Holding>,
    /// It holds files outside its repos.
    #[serde(default, skip_serializing_if = "is_false")]
    pub other_files: bool,
    /// The backups pushed before it was removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backed_up: Vec<Backup>,
}

/// What one repo of a folder holds that is not on GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holding {
    pub repo: String,
    #[serde(default)]
    pub unpushed: u32,
    #[serde(default)]
    pub uncommitted: bool,
}

/// One backup pushed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    pub repo: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

/// `refs/wip/` and a name of `[A-Za-z0-9._-]{1,240}`.
pub fn valid_wip_ref(reference: &str) -> bool {
    reference.strip_prefix("refs/wip/").is_some_and(|name| {
        (1..=240).contains(&name.len())
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    })
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
    /// The session folders a removal was asked about.
    pub folders: &'a [String],
    /// The action whose backups a removal may push, when it may.
    pub discard: Option<&'a str>,
}

impl<'a> Asked<'a> {
    /// A census of the whole nucleus.
    pub fn census() -> Asked<'a> {
        Asked {
            op: Op::Census,
            folder: None,
            driver: false,
            folders: &[],
            discard: None,
        }
    }
}

impl Snapshot {
    fn fits(&self) -> bool {
        let pushes = matches!(
            self.action,
            SnapAction::Snapshot | SnapAction::WouldSnapshot
        );
        valid_name(&self.repo)
            && self.folder.as_deref().is_none_or(valid_folder)
            && (self.branch.is_empty() || reportable_branch(&self.branch))
            && self.tree.as_deref().is_none_or(commit_id)
            && (self.action != SnapAction::Snapshot || self.tree.is_some())
            && match &self.reference {
                // The one ref a snapshot of this branch goes to.
                Some(reference) => {
                    pushes
                        && valid_wip_ref(reference)
                        && *reference == crate::housekeeping::wip_ref(&self.branch)
                }
                None => !pushes,
            }
            && match self.action {
                SnapAction::Skipped | SnapAction::Refused | SnapAction::Failed => {
                    self.reason.is_some()
                }
                _ => self.reason.is_none(),
            }
            && (self.action != SnapAction::Refused
                || self.reason.is_some_and(|word| word.refusal().is_some()))
    }
}

impl Removal {
    fn fits(&self, discard: Option<&str>) -> bool {
        let ours = |reference: &str| {
            discard.is_some_and(|id| {
                valid_wip_ref(reference) && reference.ends_with(&format!("-{id}"))
            })
        };
        valid_folder(&self.folder)
            && self.repo.as_deref().is_none_or(valid_label)
            && self.holding.len() <= MAX_FOLDER_REPOS
            && self
                .holding
                .iter()
                .all(|held| valid_label(&held.repo) && held.unpushed <= MAX_COUNT)
            && self.backed_up.len() <= MAX_FOLDER_REPOS
            && self
                .backed_up
                .iter()
                .all(|backup| valid_label(&backup.repo) && ours(&backup.reference))
            && (self.outcome != Outcome::Kept || self.why.is_some())
            && (self.outcome != Outcome::Removed || self.why.is_none())
    }
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
            moved: 0,
            why: None,
            fetched: Vec::new(),
            snapshots: Vec::new(),
            pushes: Vec::new(),
            removals: Vec::new(),
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
        // A write cut short says what it did; anything else that says why
        // nothing could be done says only that.
        let cut_short = asked.op.writes() && self.condition == Condition::Timeout;
        if self.condition != Condition::Ok && !cut_short {
            return Err(self.condition);
        }
        if self.op != asked.op {
            return Err(Condition::BadReply);
        }
        if asked.op.writes() {
            return self.check_write(asked, cut_short);
        }
        let census_only = self.moved == 0
            && self.why.is_none()
            && self.fetched.is_empty()
            && self.snapshots.is_empty()
            && self.pushes.is_empty()
            && self.removals.is_empty();
        if !census_only {
            return Err(Condition::BadReply);
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

impl Answer {
    /// A write's answer: only its own part, every field in line.
    fn check_write(mut self, asked: &Asked, cut_short: bool) -> Result<Answer, Condition> {
        let mut parts = 0;
        parts += usize::from(!self.fetched.is_empty() || self.moved > 0);
        parts += usize::from(!self.snapshots.is_empty());
        parts += usize::from(!self.pushes.is_empty());
        parts += usize::from(!self.removals.is_empty());
        let own = match asked.op {
            Op::Refresh => {
                self.snapshots.is_empty() && self.pushes.is_empty() && self.removals.is_empty()
            }
            Op::Snapshot => {
                self.fetched.is_empty()
                    && self.moved == 0
                    && self.pushes.is_empty()
                    && self.removals.is_empty()
            }
            Op::Push => {
                self.fetched.is_empty()
                    && self.moved == 0
                    && self.snapshots.is_empty()
                    && self.removals.is_empty()
            }
            Op::Remove => {
                self.fetched.is_empty()
                    && self.moved == 0
                    && self.snapshots.is_empty()
                    && self.pushes.is_empty()
            }
            Op::Census => false,
        };
        let folders: BTreeSet<&str> = self.removals.iter().map(|r| r.folder.as_str()).collect();
        let fits = own
            && parts <= 1
            && self.repos.is_empty()
            && self.folders.is_empty()
            && self.loose.is_empty()
            && self.moved <= MAX_COUNT
            && self.fetched.len() <= MAX_REPOS
            && self.fetched.iter().all(|fetched| {
                valid_name(&fetched.repo) && !(fetched.ok && fetched.failure.is_some())
            })
            && self.snapshots.len() <= MAX_WORKTREES
            && self.snapshots.iter().all(Snapshot::fits)
            && self.pushes.len() <= MAX_FOLDER_REPOS
            && self
                .pushes
                .iter()
                .all(|pushed| valid_label(&pushed.repo) && pushed.pushed <= MAX_COUNT)
            && folders.len() == self.removals.len()
            && self.removals.iter().all(|removal| {
                asked.folders.contains(&removal.folder) && removal.fits(asked.discard)
            });
        if !fits {
            return Err(Condition::BadReply);
        }
        if cut_short {
            self.complete = false;
        }
        Ok(self)
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
        folders: &[],
        discard: None,
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
            folders: &[],
            discard: None,
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
