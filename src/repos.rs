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

use crate::driver::{Call, Condition, Runner};
use crate::git;
use crate::settings::Settings;
use crate::workspace::Workspace;
use answer::{Answer, Asked, Op};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How long the host gives a census, and how long the census gives itself.
pub const CENSUS_LIMITS: (Duration, Duration) = (Duration::from_secs(60), Duration::from_secs(55));

/// What a caller says when a job for an environment in a container is one
/// that is not built yet. Nothing is done on the host in its place.
pub const NOT_YET: &str = "this is not done yet for an environment in a container";

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

    /// Read every worktree of the nucleus inside its container, or the one
    /// session folder `folder`, or, with `driver`, the worktrees of the
    /// driver of the environment. Nothing is changed and GitHub is not
    /// asked. The answer as the host takes it, or why there is none.
    pub fn census(
        &self,
        runner: &Runner,
        settings: &Settings,
        folder: Option<&str>,
        driver: bool,
        limits: (Duration, Duration),
    ) -> Result<Answer, Condition> {
        let mut more = vec![
            "--do".to_string(),
            Op::Census.as_str().to_string(),
            "--vetting".to_string(),
            match self.vetting {
                git::Vetting::Enforce => "enforce",
                git::Vetting::Report => "report",
            }
            .to_string(),
        ];
        if let Some(folder) = folder {
            more.extend(["--folder".to_string(), folder.to_string()]);
        }
        if driver {
            more.push("--driver".to_string());
        }
        let call = Call {
            container: Some(&self.container),
            env: &self.env,
            word: "repos",
            more: &more,
            limits,
            max: answer::MAX_ANSWER,
        };
        let heard = runner.inside(settings, call, SystemTime::now(), |out| answer::read(out));
        heard.map_err(Condition::of)?.check(&Asked {
            op: Op::Census,
            folder,
            driver,
        })
    }
}
