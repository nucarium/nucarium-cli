//! Every transfer between a nucleus's repositories and their remote: a
//! fetch, and a push. Each one goes through here, for a nucleus on the host
//! and inside a container alike, so that one way of moving objects could
//! take the place of another (a relay through the host, say) without any
//! caller changing.
//!
//! Why a transfer failed is said with one fixed [`Word`], read from git's
//! complaints and never passed on: they may name the remote's address, and
//! through it a token.

use super::answer::Word;
use crate::git::{Git, Output};
use anyhow::Result;
use std::path::Path;
use std::time::Duration;

/// Fetch the remote of the bare repo `bare`, dropping branches deleted on
/// it, and record its default branch.
pub fn fetch(git: &Git, bare: &Path) -> Result<Result<(), Word>> {
    let fetched = git.try_bare(bare, &["fetch", "-q", "--prune", "origin"])?;
    let _ = git.try_bare(bare, &["remote", "set-head", "origin", "--auto"]);
    Ok(match fetched.ok {
        true => Ok(()),
        false => Err(word(&fetched.stderr)),
    })
}

/// Run `git push` with `args` in the worktree `worktree`, stopped after
/// `within` when it is given. What git said, for a caller that turns it into
/// a [`Word`] or a sentence of its own.
pub fn push(git: &Git, worktree: &Path, args: &[&str], within: Option<Duration>) -> Result<Output> {
    let mut all = vec!["push"];
    all.extend_from_slice(args);
    match within {
        Some(within) => git.try_tree_within(worktree, &all, within),
        None => git.try_tree(worktree, &all),
    }
}

/// Why a transfer failed, from git's own words, as one fixed word.
pub fn word(stderr: &str) -> Word {
    let said = stderr.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|word| said.contains(word));
    if said == "timed out" {
        Word::TimedOut
    } else if any(&[
        "connect tunnel failed, response 403",
        "received http code 403 from proxy after connect",
    ]) {
        // The environment's proxy refused to take it to GitHub: its network
        // does not reach there. A proxy that failed any other way is GitHub
        // not being reached.
        Word::NoNetwork
    } else if any(&[
        "from proxy after connect",
        "connect tunnel failed",
        "proxy connect aborted",
    ]) {
        Word::Unreachable
    } else if any(&["non-fast-forward", "[rejected]", "fetch first"]) {
        Word::Rejected
    } else if any(&[
        "authentication failed",
        "could not read username",
        "permission",
        "denied",
        "403",
    ]) {
        Word::SignIn
    } else if any(&[
        "could not resolve host",
        "unable to access",
        "connection",
        "timed out",
    ]) {
        Word::Unreachable
    } else if any(&["protected branch", "declined"]) {
        Word::Refused
    } else {
        Word::Git
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_that_will_not_go_to_github_is_told_apart_from_github_being_away() {
        for said in [
            "fatal: unable to access 'https://github.com/a/b.git/': CONNECT tunnel failed, response 403",
            "fatal: unable to access 'https://github.com/a/b.git/': Received HTTP code 403 from proxy after CONNECT",
        ] {
            assert_eq!(word(said), Word::NoNetwork, "{said}");
        }
        assert_eq!(
            word("fatal: unable to access 'https://github.com/a/b.git/': Could not resolve host: github.com"),
            Word::Unreachable
        );
        // A proxy that fails some other way does not say the network is
        // closed.
        for said in [
            "fatal: unable to access 'https://github.com/a/b.git/': CONNECT tunnel failed, response 502",
            "fatal: unable to access 'https://github.com/a/b.git/': Received HTTP code 503 from proxy after CONNECT",
            "fatal: unable to access 'https://github.com/a/b.git/': Proxy CONNECT aborted",
        ] {
            assert_eq!(word(said), Word::Unreachable, "{said}");
        }
        assert_eq!(word("timed out"), Word::TimedOut);
        assert_eq!(
            word(" ! [rejected] main -> main (fetch first)"),
            Word::Rejected
        );
        assert_eq!(word("remote: Permission to a/b.git denied"), Word::SignIn);
        assert_eq!(
            word("remote: error: GH006: Protected branch update failed"),
            Word::Refused
        );
        assert_eq!(word("something else"), Word::Git);
    }
}
