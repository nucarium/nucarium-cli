//! The reporter: the part of the daemon that runs git to learn what a
//! heartbeat and an outcome event say about a session's work.
//!
//! It has a thread of its own, so neither a heartbeat nor an event ever waits
//! on git, and it does two things:
//!
//! - **Counts unsaved work**, once per nucleus, on a schedule (ten minutes
//!   unless the config says otherwise), and writes the counts to
//!   `unsaved.json`. A heartbeat reads that file and runs nothing.
//! - **Reports what a session left behind.** When a session in a nucleus
//!   stops or ends, the daemon's loop leaves a marker. Ten seconds later, and
//!   at most once a minute for one session, the reporter looks at that
//!   session's repos and writes an `outcome` event to the spool.
//!
//! Everything it writes down is a count, a time or the address of a pull
//! request. It never asks git for a commit's subject or a file's contents.
//! `git status` names the files that changed, and that answer is read for
//! one thing, whether it is empty, and then dropped. A pull request's title,
//! body, branch and author are skipped by the parser and never held.

use crate::envelope::{unix_ms, valid_env_key, valid_session_id, Draft, Envelope, EventType};
use crate::housekeeping;
use crate::sessions::{Session, Store};
use crate::settings::{self, Environment, Settings};
use crate::spool::{Record, Spool};
use crate::workspace::{RepoInfo, Workspace};
use crate::{driver, git, github, paths};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

/// A session that has just stopped may still be pushing. It is looked at
/// this long after.
pub const SETTLE: Duration = Duration::from_secs(10);
/// One session's repos are looked at no more often than this, however often
/// it stops.
pub const PER_SESSION: Duration = Duration::from_secs(60);

// ---- unsaved work, counted on a schedule ----------------------------------

/// How much of one nucleus exists only on the box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counted {
    /// Worktrees with changes that are not committed.
    pub uncommitted_repos: u64,
    /// Commits that are on no remote.
    pub unpushed_commits: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nucleus {
    /// The environments whose nucleus this is. They report the same counts.
    pub environments: Vec<String>,
    /// Nothing when a worktree could not be read: the count is then not
    /// known, and no number is put in its place.
    #[serde(default)]
    pub unsaved: Option<Counted>,
    /// When the snapshot job last finished here with nothing failed.
    #[serde(default)]
    pub last_snapshot_ms: Option<u64>,
    /// True for the worktrees of a driver that owns them, which is no
    /// nucleus: its unsaved work is counted, and it is never snapshotted.
    #[serde(default)]
    pub driver: bool,
    /// The repos the daemon would not run git in at this count, by name,
    /// each with a fixed word for why. When enforcing, any of them makes
    /// `unsaved` unknown. Shown on the box only; a heartbeat does not carry
    /// it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<RefusedRepo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedRepo {
    pub repo: String,
    pub reason: git::Reason,
}

/// What one count found: the numbers, the repos that were refused, and
/// whether a refusal left something out.
#[derive(Default)]
struct Tally {
    counted: Counted,
    refused: Vec<RefusedRepo>,
    left_out: bool,
}

impl Tally {
    /// Note a refusal, once per repo. True when git is not to be run there:
    /// what it holds is then not known, and neither is the count.
    fn refuses(&mut self, git: &git::Git, refused: Option<git::Refused>) -> bool {
        let Some(refused) = refused else {
            return false;
        };
        if !self.refused.iter().any(|seen| seen.repo == refused.repo) {
            self.refused.push(RefusedRepo {
                repo: refused.repo,
                reason: refused.reason,
            });
        }
        self.left_out |= git.enforcing();
        git.enforcing()
    }

    /// The count, when nothing was left out of it.
    fn finish(self, read: Option<()>) -> (Option<Counted>, Vec<RefusedRepo>) {
        let counted = read.filter(|_| !self.left_out).map(|_| self.counted);
        (counted, self.refused)
    }

    fn add(&mut self, unsaved: crate::workspace::Unsaved) {
        self.counted.uncommitted_repos += u64::from(unsaved.uncommitted);
        self.counted.unpushed_commits += u64::from(unsaved.unpushed);
    }
}

/// What `unsaved.json` holds: the last count, for the heartbeat to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub counted_at_ms: u64,
    pub nuclei: Vec<Nucleus>,
}

impl Counts {
    pub fn nucleus(&self, environment: &str) -> Option<&Nucleus> {
        self.nuclei
            .iter()
            .find(|nucleus| nucleus.environments.iter().any(|name| name == environment))
    }
}

pub fn counts_file(state_dir: &Path) -> PathBuf {
    state_dir.join("unsaved.json")
}

/// The last count, or nothing when there has been none or the report is off.
pub fn read_counts(settings: &Settings) -> Option<Counts> {
    settings.report_unsaved?;
    serde_json::from_str(&fs::read_to_string(counts_file(&settings.state_dir)).ok()?).ok()
}

// ---- what a session left behind -------------------------------------------

fn asks_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("outcomes")
}

/// Ask for a session's outcome to be reported. This is all the daemon's loop
/// does about it: one small file, and no git.
pub fn ask(state_dir: &Path, env: &str, session_id: &str, now: SystemTime) {
    if !valid_env_key(env) || !valid_session_id(session_id) {
        return;
    }
    let path = asks_dir(state_dir)
        .join(env)
        .join(format!("{session_id}.json"));
    let _ = paths::write_private(&path, unix_ms(now).to_string().as_bytes());
}

struct Ask {
    env: String,
    session_id: String,
    asked_ms: u64,
    path: PathBuf,
}

fn asks(state_dir: &Path) -> Vec<Ask> {
    let mut found = Vec::new();
    let Ok(environments) = fs::read_dir(asks_dir(state_dir)) else {
        return found;
    };
    for environment in environments.flatten() {
        let env = environment.file_name().to_string_lossy().to_string();
        let Ok(entries) = fs::read_dir(environment.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(session_id) = name.strip_suffix(".json") else {
                continue;
            };
            // A marker that says nothing readable is taken as asked long ago.
            let asked_ms = fs::read_to_string(entry.path())
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0);
            found.push(Ask {
                env: env.clone(),
                session_id: session_id.to_string(),
                asked_ms,
                path: entry.path(),
            });
        }
    }
    found
}

/// A commit id as a manifest records one. It goes to git as an argument, so
/// it has to be one.
fn commit_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|c| c.is_ascii_hexdigit())
}

/// How many commits a session made in one repo: those on its branch since
/// the commit its worktree started from.
fn commits_made(workspace: &Workspace, info: &RepoInfo) -> Option<u32> {
    if !commit_id(&info.base) {
        return None;
    }
    let made = workspace
        .git()
        .try_tree(
            &info.path,
            &["rev-list", "--count", &format!("{}..HEAD", info.base)],
        )
        .ok()
        .filter(|out| out.ok)?;
    made.stdout.parse().ok()
}

/// The open pull requests for one repo of a session, or nothing when that
/// could not be found out.
fn open_pulls(workspace: &Workspace, info: &RepoInfo, made: Option<u32>) -> Option<Vec<String>> {
    // A session that has committed nothing has opened nothing.
    if made == Some(0) {
        return Some(Vec::new());
    }
    let on_remote = workspace
        .git()
        .try_tree(&info.path, &["branch", "-r", "--contains", "HEAD"])
        .ok()
        .filter(|out| out.ok)?;
    // A pull request's head is a commit GitHub has. One that is on no remote
    // branch is the head of none, so there is nothing to ask. That is not
    // the same as there being none: a session that pushed, opened a pull
    // request and has committed again since still has one. Saying nothing
    // leaves Watch with what it last heard.
    if on_remote.stdout.is_empty() {
        return None;
    }
    let head = workspace
        .git()
        .tree(&info.path, &["rev-parse", "HEAD"])
        .ok()?;
    github::open_pulls(
        workspace.github_api(),
        &workspace.remote_url(&info.bare),
        &head,
        workspace.github_token_file(),
    )
}

/// The reporter's state between one look and the next.
#[derive(Debug, Default)]
pub struct Reporter {
    /// When each session's outcome was last reported, in Unix milliseconds.
    reported: BTreeMap<(String, String), u64>,
    /// The last count, read from disk once and kept since.
    counts: Option<Counts>,
    loaded: bool,
    /// A stand-in for GitHub on this machine, for tests.
    github: Option<String>,
}

impl Reporter {
    /// Ask a stand-in on this machine in place of GitHub. Any other address
    /// is ignored.
    pub fn asking_github_at(mut self, url: &str) -> Reporter {
        self.github = github::stand_in(url);
        self
    }

    fn workspace(&self, settings: &Settings, environment: &Environment) -> Workspace {
        let workspace = housekeeping::workspace(settings, environment)
            .remembering_merges(settings.state_dir.join("merged.json"));
        match &self.github {
            Some(url) => workspace.asking_github_at(url),
            None => workspace,
        }
    }

    /// One look: report the outcomes that are due, then count unsaved work if
    /// it is time.
    pub fn tick(&mut self, settings: &Settings, now: SystemTime) {
        self.report_outcomes(settings, now);
        self.count_if_due(settings, now);
    }

    /// The unsaved work in one nucleus, or nothing when a worktree could not
    /// be read, and the repos that were refused. When enforcing, git is not
    /// run in a refused repo or a refused worktree of one, and the count is
    /// then not known: a number that left them out would read as all there
    /// is. The other repos are still looked at, for the refusals they hold.
    fn count_nucleus(
        &self,
        settings: &Settings,
        environment: &Environment,
    ) -> (Option<Counted>, Vec<RefusedRepo>) {
        let mut tally = Tally::default();
        if !environment.root.is_dir() {
            return (None, tally.refused);
        }
        let workspace = self
            .workspace(settings, environment)
            .leaving_alone(driver::protected_roots(settings));
        let git = workspace.git();
        let counted = (|| -> Option<()> {
            for item in workspace.available().ok()? {
                if tally.refuses(git, git.vet_bare(&item.bare)) {
                    continue;
                }
                for worktree in workspace.worktrees(&item.bare).ok()? {
                    // A worktree in a driver's own folders is counted once,
                    // under the driver. Any other worktree of the nucleus's
                    // repos is counted here, the nucleus's own or not:
                    // counting only reads.
                    if !worktree.is_dir() || workspace.under_a_driver(&worktree) {
                        continue;
                    }
                    let refused = git.vet_tree(&worktree).map(|refused| git::Refused {
                        repo: item.repo.clone(),
                        ..refused
                    });
                    if tally.refuses(git, refused) {
                        continue;
                    }
                    tally.add(workspace.unsaved(&worktree).ok()?);
                }
            }
            Some(())
        })();
        tally.finish(counted)
    }

    /// The unsaved work in the worktrees a driver owns, by the same rule as
    /// a nucleus's: the driver's adapter says where they are, and
    /// [`Workspace::unsaved`] says what each holds. It only reads them.
    /// Nothing when a folder cannot be read, or while the driver itself
    /// lists a worktree that is in none of them.
    fn count_driver(
        &self,
        settings: &Settings,
        environment: &Environment,
    ) -> (Option<Counted>, Vec<RefusedRepo>) {
        let mut tally = Tally::default();
        let Some(adapter) = environment.driver_adapter() else {
            return (None, tally.refused);
        };
        let Some(worktrees) = adapter.worktrees(environment) else {
            return (None, tally.refused);
        };
        // The driver's own last answer listed a worktree that looking in
        // its folders does not find. A number that leaves it out would read
        // as all there is, so there is none until its folder is named.
        let left_out = driver::read_memory(&settings.state_dir, &environment.name)
            .filter(|memory| memory.driver == adapter.name())
            .is_some_and(|memory| memory.uncounted_worktrees > 0);
        if left_out {
            return (None, tally.refused);
        }
        let workspace = self.workspace(settings, environment);
        let git = workspace.git();
        let counted = (|| -> Option<()> {
            for worktree in worktrees.iter().filter(|worktree| worktree.is_dir()) {
                if tally.refuses(git, git.vet_tree(worktree)) {
                    continue;
                }
                tally.add(workspace.unsaved(worktree).ok()?);
            }
            Some(())
        })();
        tally.finish(counted)
    }

    /// Count every nucleus now, each once however many environments share
    /// it, and then the worktrees of each driver that owns its own.
    pub fn count(&self, settings: &Settings, now: SystemTime) -> Counts {
        let ledger = housekeeping::read_ledger(&settings.state_dir);
        let owned = settings
            .environments
            .iter()
            .filter(|environment| environment.driver_owns_worktrees())
            .map(|environment| {
                let (unsaved, refused) = self.count_driver(settings, environment);
                Nucleus {
                    environments: vec![environment.name.clone()],
                    unsaved,
                    last_snapshot_ms: None,
                    driver: true,
                    refused,
                }
            });
        let nuclei = housekeeping::one_per_nucleus(settings)
            .into_iter()
            .map(|environment| {
                let (unsaved, refused) = self.count_nucleus(settings, environment);
                Nucleus {
                    environments: housekeeping::sharing_nucleus(settings, environment),
                    unsaved,
                    // Housekeeping runs a nucleus's jobs under the first of
                    // the environments that share it, which is this one.
                    last_snapshot_ms: ledger
                        .get(&format!("{}/snapshot", environment.name))
                        .and_then(|last| last.good_at_s)
                        .map(|seconds| seconds.saturating_mul(1_000)),
                    driver: false,
                    refused,
                }
            })
            .chain(owned)
            .collect();
        Counts {
            counted_at_ms: unix_ms(now),
            nuclei,
        }
    }

    fn count_if_due(&mut self, settings: &Settings, now: SystemTime) {
        let file = counts_file(&settings.state_dir);
        let Some(every) = settings.report_unsaved else {
            // Off: a count from before must not go on being reported.
            if self.counts.take().is_some() || !self.loaded {
                let _ = fs::remove_file(&file);
            }
            self.loaded = true;
            return;
        };
        if !self.loaded {
            self.counts = read_counts(settings);
            self.loaded = true;
        }
        let now_ms = unix_ms(now);
        let fresh = self.counts.as_ref().is_some_and(|counts| {
            // A count from a clock that has since moved back is not fresh,
            // and neither is one that misses an environment added since.
            now_ms
                .checked_sub(counts.counted_at_ms)
                .is_some_and(|age| age < every.as_millis() as u64)
                && settings
                    .environments
                    .iter()
                    .all(|environment| counts.nucleus(&environment.name).is_some())
        });
        if fresh {
            return;
        }
        let counts = self.count(settings, now);
        if let Ok(text) = serde_json::to_string(&counts) {
            let _ = paths::write_private(&file, text.as_bytes());
        }
        self.counts = Some(counts);
    }

    /// What a session left behind, or nothing when it cannot be found out.
    pub fn outcome(
        &self,
        settings: &Settings,
        environment: &Environment,
        session: &Session,
    ) -> Option<Draft> {
        let mut draft = Draft {
            env_id: session.env.clone(),
            session_id: session.session_id.clone(),
            bridge_session_id: session.bridge_session_id.clone(),
            repos: session.repos.clone(),
            branch: session.branch.clone(),
            ..Draft::default()
        };
        // The folder's name came from a spool file. It names one folder
        // under the nucleus and never a path out of it.
        let name = session.workspace.as_deref().filter(|name| {
            !name.is_empty() && !name.contains(['/', '\\']) && !matches!(*name, "." | "..")
        })?;
        let sessions = environment.root.join(".worktrees");
        // A nucleus that is not there, on a disk that is not mounted say,
        // tells nothing about what a session in it holds.
        if !sessions.is_dir() {
            return None;
        }
        let folder = sessions.join(name);
        if !folder.exists() {
            // The session's folder is gone, and with it anything that
            // existed only there. What else it left is no longer knowable.
            draft.uncommitted_repos = Some(0);
            draft.unpushed_commits = Some(0);
            return Some(draft);
        }
        let manifest = Workspace::load_manifest(&folder).ok()?;
        let workspace = self.workspace(settings, environment);
        let mut commits = Some(0u32);
        let mut unsaved = Some((0u32, 0u32));
        let mut pulls = Some(Vec::new());
        for info in manifest.repos.values().filter(|info| info.path.is_dir()) {
            let made = commits_made(&workspace, info);
            commits = commits
                .zip(made)
                .map(|(sum, made)| sum.saturating_add(made));
            unsaved = unsaved.zip(workspace.unsaved(&info.path).ok()).map(
                |((uncommitted, unpushed), repo)| {
                    (
                        uncommitted + u32::from(repo.uncommitted),
                        unpushed.saturating_add(repo.unpushed),
                    )
                },
            );
            pulls = pulls.zip(open_pulls(&workspace, info, made)).map(
                |(mut all, more): (Vec<String>, _)| {
                    all.extend(more);
                    all
                },
            );
        }
        draft.repos = manifest
            .repos
            .values()
            .map(|info| info.label.clone())
            .collect();
        draft.branch = Some(manifest.branch);
        draft.commits = commits;
        draft.uncommitted_repos = unsaved.map(|(uncommitted, _)| uncommitted);
        draft.unpushed_commits = unsaved.map(|(_, unpushed)| unpushed);
        draft.pull_requests = pulls;
        Some(draft)
    }

    fn report_outcomes(&mut self, settings: &Settings, now: SystemTime) {
        let now_ms = unix_ms(now);
        let since = |then: u64| now_ms.saturating_sub(then);
        let per_session = PER_SESSION.as_millis() as u64;
        self.reported.retain(|_, at| since(*at) < per_session);
        let store = Store::new(&settings.state_dir);
        let spool = Spool::new(&settings.spool_dir);
        for ask in asks(&settings.state_dir) {
            // A marker dated ahead of the clock was written before the clock
            // moved back. It has waited long enough.
            if ask.asked_ms <= now_ms && since(ask.asked_ms) < SETTLE.as_millis() as u64 {
                continue;
            }
            let key = (ask.env.clone(), ask.session_id.clone());
            if self.reported.contains_key(&key) {
                continue;
            }
            // Removed before the look, not after: a stop that comes during
            // the look leaves a new marker, and is looked at in its turn.
            let _ = fs::remove_file(&ask.path);
            let (Some(environment), Some(session)) = (
                settings.environment(&ask.env),
                store.load(&ask.env, &ask.session_id),
            ) else {
                continue;
            };
            self.reported.insert(key, now_ms);
            // A spool nobody is emptying is not allowed to fill the disk.
            if spool.holds_at_least(settings.spool_max_events.saturating_mul(2)) {
                continue;
            }
            let event = self
                .outcome(settings, environment, &session)
                .and_then(|draft| Envelope::new(EventType::Outcome, draft, now));
            if let Some(event) = event {
                let _ = spool.write(&Record {
                    event,
                    at_ms: now_ms,
                    workspace: None,
                    process: None,
                });
            }
        }
    }
}

/// The reporter's thread: ten seconds after start, then every two seconds,
/// look. The config is read afresh each time, as housekeeping reads it.
pub fn run_schedule(config: &Path, stop: &AtomicBool) {
    if housekeeping::wait(stop, Duration::from_secs(10)) {
        return;
    }
    let mut reporter = Reporter::default();
    loop {
        if let Ok(settings) = settings::load_from(config) {
            reporter.tick(&settings, SystemTime::now());
        }
        if housekeeping::wait(stop, Duration::from_secs(2)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_id_is_hex_and_nothing_that_reads_as_an_option() {
        assert!(commit_id(&"a1".repeat(20)));
        assert!(commit_id(&"0".repeat(64)));
        assert!(!commit_id("--output=/tmp/x"));
        assert!(!commit_id("main"));
        assert!(!commit_id(&"g".repeat(40)));
        assert!(!commit_id(""));
    }

    #[test]
    fn a_marker_is_one_small_file_named_for_the_session_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let now = std::time::UNIX_EPOCH + Duration::from_millis(1_795_089_600_000);
        ask(dir.path(), "client-a", "s1", now);
        ask(dir.path(), "client-a", "s1", now + Duration::from_secs(5));
        ask(dir.path(), "../etc", "s1", now);
        ask(dir.path(), "client-a", "../../x", now);
        let found = asks(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].env, "client-a");
        assert_eq!(found[0].session_id, "s1");
        assert_eq!(found[0].asked_ms, 1_795_089_605_000);
    }

    #[test]
    fn environments_that_share_a_nucleus_read_the_same_count() {
        let counts = Counts {
            counted_at_ms: 1,
            nuclei: vec![
                Nucleus {
                    environments: vec!["client-a".into(), "client-b".into()],
                    unsaved: Some(Counted {
                        uncommitted_repos: 2,
                        unpushed_commits: 5,
                    }),
                    last_snapshot_ms: None,
                    driver: false,
                    refused: Vec::new(),
                },
                Nucleus {
                    environments: vec!["ops".into()],
                    ..Nucleus::default()
                },
            ],
        };
        assert_eq!(counts.nucleus("client-a"), counts.nucleus("client-b"));
        assert_eq!(counts.nucleus("ops").unwrap().unsaved, None);
        assert_eq!(counts.nucleus("personal"), None);
    }
}
