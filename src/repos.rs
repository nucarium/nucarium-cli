//! Git for the repositories a container writes runs inside that container.
//!
//! On the host, git reads what a repository's own files say: a swapped
//! config can have it run a program, and `objects/info/alternates` or a
//! linked ref can lead it out of the environment. So for an environment in
//! a container, and for one on the host that shares its nucleus with one,
//! the daemon runs no git at all. It runs the box's own `nucleated repos`
//! inside the container through one capped `podman exec`, as it takes a
//! driver's look ([`crate::driver::Runner`]), holds the answer to fixed
//! shapes and caps ([`answer`]), and reads no path the answer names. GitHub
//! is still asked from the host, with the host's own token, about the
//! commits the answer names.
//!
//! [`Nucleus`] is how every caller gets at an environment's repositories,
//! so none can run git on the host for a container's by mistake: the
//! container's side has no git to run, and its host side runs git as
//! [`git::Git::Elsewhere`], which fails before anything is started.

pub mod answer;
pub mod inside;
pub mod transfer;

use crate::driver::{Call, Condition, Runner};
use crate::git;
use crate::settings::Settings;
use crate::workspace::Workspace;
use answer::{Answer, Asked, Op};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How long the host gives a census, and how long the census gives itself.
pub const CENSUS_LIMITS: (Duration, Duration) = (Duration::from_secs(60), Duration::from_secs(55));

/// What the host says, wherever it says why, when an environment's network
/// does not reach GitHub.
pub const NO_NETWORK: &str =
    "The environment's network does not reach GitHub, so nothing was sent.";

/// What the host says when a container could not be asked, or its answer
/// could not be used: nothing about the repositories is known.
pub fn could_not_know(condition: Condition) -> &'static str {
    match condition {
        Condition::Absent => "The environment is not running, so nothing was done.",
        Condition::Timeout => "The environment took too long to answer, so nothing was done.",
        _ => "The environment gave no answer that could be used, so nothing was done.",
    }
}

/// An environment's repositories, as the daemon may get at them.
pub enum Nucleus {
    /// On the host, where nothing else writes them: git runs here, guarded.
    Host(Workspace),
    /// Written by a container: git runs in there.
    Contained(Contained),
}

impl Nucleus {
    /// Leave everything under these folders alone, as
    /// [`Workspace::leaving_alone`].
    pub fn leaving_alone(self, folders: Vec<PathBuf>) -> Nucleus {
        match self {
            Nucleus::Host(workspace) => Nucleus::Host(workspace.leaving_alone(folders)),
            Nucleus::Contained(contained) => Nucleus::Contained(contained),
        }
    }

    /// Remember GitHub's answers, as [`Workspace::remembering_merges`].
    pub fn remembering_merges(self, file: PathBuf) -> Nucleus {
        match self {
            Nucleus::Host(workspace) => Nucleus::Host(workspace.remembering_merges(file)),
            Nucleus::Contained(mut contained) => {
                contained.host = contained.host.remembering_merges(file);
                Nucleus::Contained(contained)
            }
        }
    }

    /// Ask a stand-in for GitHub, as [`Workspace::asking_github_at`].
    pub fn asking_github_at(self, url: &str) -> Nucleus {
        match self {
            Nucleus::Host(workspace) => Nucleus::Host(workspace.asking_github_at(url)),
            Nucleus::Contained(mut contained) => {
                contained.host = contained.host.asking_github_at(url);
                Nucleus::Contained(contained)
            }
        }
    }
}

/// The repositories of a nucleus a container writes.
pub struct Contained {
    /// The container git runs in.
    pub container: String,
    /// The environment it runs for there, by its config name: the one whose
    /// container it is, which for a host environment sharing the nucleus is
    /// another.
    pub env: String,
    /// The daemon's vetting mode, which the container's config does not
    /// hold.
    pub vetting: git::Vetting,
    /// The host's side: its GitHub sign-in and what GitHub said before. Its
    /// git is [`git::Git::Elsewhere`].
    pub host: Workspace,
}

impl Contained {
    /// True when a repository git is not to be run in is not run in, and
    /// what it holds is then not known.
    pub fn enforcing(&self) -> bool {
        self.vetting == git::Vetting::Enforce
    }

    fn vetting_word(&self) -> &'static str {
        match self.vetting {
            git::Vetting::Enforce => "enforce",
            git::Vetting::Report => "report",
        }
    }

    /// Run `nucleated repos --do <op>` inside the container, with `more`,
    /// and take its answer as an answer to `asked`, or say why there is
    /// none.
    fn ask(
        &self,
        inside: &Inside,
        settings: &Settings,
        more: Vec<String>,
        asked: &Asked,
    ) -> Result<Answer, Condition> {
        let mut args = vec![
            "--do".to_string(),
            asked.op.as_str().to_string(),
            "--vetting".to_string(),
            self.vetting_word().to_string(),
        ];
        args.extend(more);
        let call = Call {
            container: Some(&self.container),
            env: &self.env,
            word: "repos",
            more: &args,
            limits: inside.limits(asked.op),
            max: answer::MAX_ANSWER,
        };
        let heard = inside
            .runner
            .inside(settings, call, SystemTime::now(), |out| answer::read(out));
        heard.map_err(Condition::of)?.check(asked)
    }

    /// Read every worktree of the nucleus inside its container, or the one
    /// session folder `folder`, or, with `driver`, the worktrees of the
    /// driver of the environment. Nothing is changed and GitHub is not
    /// asked. The answer as the host takes it, or why there is none.
    pub fn census(
        &self,
        inside: &Inside,
        settings: &Settings,
        folder: Option<&str>,
        driver: bool,
    ) -> Result<Answer, Condition> {
        let mut more = Vec::new();
        if let Some(folder) = folder {
            more.extend(["--folder".to_string(), folder.to_string()]);
        }
        if driver {
            more.push("--driver".to_string());
        }
        let asked = Asked {
            folder,
            driver,
            ..Asked::census()
        };
        self.ask(inside, settings, more, &asked)
    }

    /// Move the nucleus's repos to their owner folders, unless `dry_run`,
    /// then, unless `move_only`, fetch each one.
    pub fn refresh(
        &self,
        inside: &Inside,
        settings: &Settings,
        dry_run: bool,
        move_only: bool,
    ) -> Result<Answer, Condition> {
        let mut more = Vec::new();
        if dry_run {
            more.push("--dry-run".to_string());
        }
        if move_only {
            more.push("--move-only".to_string());
        }
        let asked = Asked {
            op: Op::Refresh,
            ..Asked::census()
        };
        self.ask(inside, settings, more, &asked)
    }

    /// Push what each worktree has not committed to its hidden ref, but not
    /// a tree in `pushed`, which the host has recorded as pushed already.
    pub fn snapshot(
        &self,
        inside: &Inside,
        settings: &Settings,
        dry_run: bool,
        pushed: &[String],
    ) -> Result<Answer, Condition> {
        let mut more = Vec::new();
        if dry_run {
            more.push("--dry-run".to_string());
        }
        for tree in pushed {
            more.extend(["--pushed".to_string(), tree.clone()]);
        }
        let asked = Asked {
            op: Op::Snapshot,
            ..Asked::census()
        };
        self.ask(inside, settings, more, &asked)
    }

    /// Push the branches of the session folder `folder`. `merged` names the
    /// commits GitHub says were merged.
    pub fn push(
        &self,
        inside: &Inside,
        settings: &Settings,
        folder: &str,
        merged: &[String],
    ) -> Result<Answer, Condition> {
        let mut more = vec!["--folder".to_string(), folder.to_string()];
        for sha in merged {
            more.extend(["--merged".to_string(), sha.clone()]);
        }
        let asked = Asked {
            op: Op::Push,
            folder: Some(folder),
            ..Asked::census()
        };
        self.ask(inside, settings, more, &asked)
    }

    /// Remove each of `folders`, each only if it still stands as its stamp
    /// says, by the rule that keeps unsaved work: `merged` names the commits
    /// GitHub says were merged. With `discard`, the id of a Clean up that
    /// may drop unsaved work, each repo's is pushed to a ref of that
    /// action's own first.
    pub fn remove(
        &self,
        inside: &Inside,
        settings: &Settings,
        folders: &[(String, String)],
        merged: &[String],
        discard: Option<&str>,
    ) -> Result<Answer, Condition> {
        let mut more = Vec::new();
        for (folder, stamp) in folders {
            more.extend([
                "--folder".to_string(),
                folder.clone(),
                "--expect".to_string(),
                stamp.clone(),
            ]);
        }
        for sha in merged {
            more.extend(["--merged".to_string(), sha.clone()]);
        }
        if let Some(id) = discard {
            more.extend(["--discard".to_string(), "--id".to_string(), id.to_string()]);
        }
        let names: Vec<String> = folders.iter().map(|(folder, _)| folder.clone()).collect();
        let asked = Asked {
            op: Op::Remove,
            folders: &names,
            discard,
            ..Asked::census()
        };
        self.ask(inside, settings, more, &asked)
    }

    /// The commits among `facts` that GitHub says were merged, of those whose
    /// verdict needs it: what a write inside is told, as it asks no one.
    pub fn merged_among<'a>(
        &self,
        facts: impl IntoIterator<Item = &'a answer::Facts>,
    ) -> Vec<String> {
        let mut merged: Vec<String> = facts
            .into_iter()
            .filter(|facts| !facts.tracked && !facts.on_remote)
            .filter(|facts| {
                self.host
                    .merged_on_github(facts.slug.as_deref(), &facts.head)
            })
            .map(|facts| facts.head.clone())
            .collect();
        merged.sort();
        merged.dedup();
        merged
    }
}

/// How the host runs `nucleated repos`: the box's own program and `podman`,
/// and, for tests, a shorter time than the design's.
#[derive(Debug, Clone)]
pub struct Inside {
    pub runner: Runner,
    /// Every call is given this long, and told a little less, in place of
    /// its own limits.
    pub patience: Option<Duration>,
}

impl Inside {
    /// This program and the box's `podman`, with each call's own limits.
    pub fn of_this_box() -> Option<Inside> {
        Runner::of_this_box().map(|runner| Inside {
            runner,
            patience: None,
        })
    }

    /// How long the host gives a call, and how long the call gives itself:
    /// a census a minute; a refresh or a snapshot, which fetch and push
    /// every repo, ten; a Push or a removal five.
    pub fn limits(&self, op: Op) -> (Duration, Duration) {
        if let Some(outer) = self.patience {
            return (outer, outer.mul_f64(11.0 / 12.0));
        }
        match op {
            Op::Census => CENSUS_LIMITS,
            Op::Refresh | Op::Snapshot => (Duration::from_secs(600), Duration::from_secs(590)),
            Op::Push | Op::Remove => (Duration::from_secs(300), Duration::from_secs(290)),
        }
    }
}
