//! The two questions nucleate asks GitHub about a commit: was it merged, and
//! which open pull requests is it the head of? Both start from one documented
//! call, "list pull requests associated with a commit", and five of its
//! fields. A merged pull request whose head is some later commit is then
//! checked against "list commits on a pull request", for the commit ids alone.
//! The addresses of a merged pull request that holds a commit and of an open
//! one whose head it is come from the same answer.

use crate::envelope::pull_request_repo;
use crate::watch::normalize_api_url;
use serde::de::IgnoredAny;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

pub const API_URL: &str = "https://api.github.com";
/// Moves the GitHub API to a stand-in on this machine, for tests.
pub const API_URL_ENV: &str = "NUCLEATE_GITHUB_API_URL";

/// An address for a stand-in GitHub API, accepted only when it is plain http
/// on this machine. The token goes wherever this points, so nothing else,
/// and nothing across a network, may take GitHub's place.
pub fn stand_in(url: &str) -> Option<String> {
    normalize_api_url(url)
        .ok()
        .filter(|url| url.starts_with("http://"))
}

/// The GitHub API to ask: GitHub's, unless the environment names a stand-in.
pub fn api_url() -> String {
    env::var(API_URL_ENV)
        .ok()
        .and_then(|url| stand_in(&url))
        .unwrap_or_else(|| API_URL.to_string())
}

/// GitHub's git host for the API at `api`: github.com for GitHub's own, and
/// the stand-in itself, scheme and address only, for one on this machine.
pub fn git_origin(api: &str) -> String {
    stand_in(api)
        .and_then(|url| {
            let (scheme, rest) = url.split_once("://")?;
            let authority = rest.split('/').next()?;
            Some(format!("{scheme}://{authority}"))
        })
        .unwrap_or_else(|| crate::git::GITHUB_ORIGIN.to_string())
}

/// Extract `(owner, repo)` from a GitHub remote URL, or `None` for any other remote.
pub fn slug(url: &str) -> Option<(String, String)> {
    let rest = url.split("github.com").nth(1)?;
    let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('/'))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// A remote address for the GitHub repo `slug`, `owner/name`, that
/// [`slug`] reads back; an empty one, which is no GitHub repo's, for none.
pub fn remote_of(slug: Option<&str>) -> String {
    slug.map(|slug| format!("{}/{slug}.git", crate::git::GITHUB_ORIGIN))
        .unwrap_or_default()
}

fn token(token_file: Option<&Path>) -> Option<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = env::var(var) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    let text = fs::read_to_string(token_file?).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The fields read from each pull request GitHub lists. Its title, its body,
/// its branch names and its author are skipped by the parser and never held.
#[derive(Deserialize)]
struct Pull {
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// Read for whether it is there, not for what it says.
    #[serde(default)]
    merged_at: Option<IgnoredAny>,
    #[serde(default)]
    head: Option<Head>,
}

#[derive(Deserialize)]
struct Head {
    #[serde(default)]
    sha: Option<String>,
}

impl Pull {
    fn head_is(&self, sha: &str) -> bool {
        self.head.as_ref().and_then(|head| head.sha.as_deref()) == Some(sha)
    }
}

/// One commit of a pull request, read for its id and nothing else.
#[derive(Deserialize)]
struct Commit {
    #[serde(default)]
    sha: Option<String>,
}

fn hex(sha: &str) -> bool {
    !sha.is_empty() && sha.bytes().all(|c| c.is_ascii_hexdigit())
}

/// One answer from the GitHub API, or nothing when there was none.
fn get<T: serde::de::DeserializeOwned>(url: &str, token_file: Option<&Path>) -> Option<T> {
    let mut request = ureq::get(url)
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "nucleate");
    if let Some(token) = token(token_file) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    request.call().ok()?.body_mut().read_json().ok()
}

/// The pull requests GitHub associates with a commit, or nothing when there
/// was no answer. For a commit that is not on the default branch, as the
/// commits of a squash-merged branch never are, that is every pull request
/// whose commits include it, merged or open.
fn pulls(
    api: &str,
    owner: &str,
    repo: &str,
    sha: &str,
    token_file: Option<&Path>,
) -> Option<Vec<Pull>> {
    // A commit id goes into the address, so it has to be one.
    if !hex(sha) {
        return None;
    }
    get(
        &format!("{api}/repos/{owner}/{repo}/commits/{sha}/pulls"),
        token_file,
    )
}

/// GitHub lists at most this many commits of one pull request.
const PULL_COMMITS_MAX: usize = 250;
const PER_PAGE: usize = 100;

/// Whether `sha` is one of the commits of pull request `number`. Any failure
/// to get an answer, and a commit beyond the most GitHub lists, is "no".
fn among_commits(
    api: &str,
    owner: &str,
    repo: &str,
    number: u64,
    sha: &str,
    token_file: Option<&Path>,
) -> bool {
    for page in 1..=PULL_COMMITS_MAX.div_ceil(PER_PAGE) {
        let url = format!(
            "{api}/repos/{owner}/{repo}/pulls/{number}/commits?per_page={PER_PAGE}&page={page}"
        );
        let Some(commits) = get::<Vec<Commit>>(&url, token_file) else {
            return false;
        };
        if commits.iter().any(|commit| {
            commit
                .sha
                .as_deref()
                .is_some_and(|id| id.eq_ignore_ascii_case(sha))
        }) {
            return true;
        }
        if commits.len() < PER_PAGE {
            return false;
        }
    }
    false
}

/// What GitHub says about one commit: whether a merged pull request holds
/// it, and the addresses of that pull request and of an open one whose head
/// it is. An address is kept only when it is a pull request on this same
/// repo, so nothing else GitHub might put in the field is passed on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lookup {
    pub merged: bool,
    pub merged_pull: Option<String>,
    pub open_pull: Option<String>,
}

/// Ask GitHub about one commit. Nothing when there was no answer, or when
/// the remote is not on GitHub.
///
/// A commit is merged when it is one of the commits of a merged pull
/// request: its head, or one before it. A commit made after the last one a
/// pull request held is in none, however that pull request ended.
pub fn lookup(api: &str, remote_url: &str, sha: &str, token_file: Option<&Path>) -> Option<Lookup> {
    let (owner, repo) = slug(remote_url)?;
    let here = format!("{owner}/{repo}");
    let address = |pull: &Pull| {
        pull.html_url.clone().filter(|url| {
            pull_request_repo(url).is_some_and(|named| named.eq_ignore_ascii_case(&here))
        })
    };
    let pulls = pulls(api, &owner, &repo, sha, token_file)?;
    let merged = pulls
        .iter()
        .filter(|pull| pull.merged_at.is_some())
        .find(|pull| pull.head_is(sha))
        .or_else(|| {
            pulls
                .iter()
                .filter(|pull| pull.merged_at.is_some() && !pull.head_is(sha))
                .find(|pull| {
                    pull.number.is_some_and(|number| {
                        among_commits(api, &owner, &repo, number, sha, token_file)
                    })
                })
        });
    let open = pulls
        .iter()
        .find(|pull| pull.state.as_deref() == Some("open") && pull.head_is(sha));
    Some(Lookup {
        merged: merged.is_some(),
        merged_pull: merged.and_then(address),
        open_pull: open.and_then(address),
    })
}

/// True when this commit is one of the commits of a merged pull request, by
/// the rule [`lookup`] gives.
///
/// A squash merge rewrites the commits and the branch is then deleted, so
/// asking GitHub is the only way to recognise finished work. Any failure to
/// get an answer counts as "no": nucleate never deletes on doubt.
pub fn merged(api: &str, remote_url: &str, sha: &str, token_file: Option<&Path>) -> bool {
    lookup(api, remote_url, sha, token_file).is_some_and(|found| found.merged)
}

/// The addresses of the open pull requests whose head is exactly this commit.
///
/// Nothing when GitHub gave no answer, which is not the same as none. A
/// remote that is not GitHub has none this program can know of. An address is
/// kept only when it is a pull request on this same repo, so nothing else
/// GitHub might put in the field is passed on.
pub fn open_pulls(
    api: &str,
    remote_url: &str,
    sha: &str,
    token_file: Option<&Path>,
) -> Option<Vec<String>> {
    let Some((owner, repo)) = slug(remote_url) else {
        return Some(Vec::new());
    };
    let here = format!("{owner}/{repo}");
    Some(
        pulls(api, &owner, &repo, sha, token_file)?
            .into_iter()
            .filter(|pull| pull.state.as_deref() == Some("open") && pull.head_is(sha))
            .filter_map(|pull| pull.html_url)
            .filter(|url| {
                pull_request_repo(url).is_some_and(|named| named.eq_ignore_ascii_case(&here))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{merged, slug, stand_in, Pull, API_URL};

    /// Talks to the real GitHub API, so it only runs when asked for:
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn merged_lookup_against_github() {
        let remote = "https://github.com/git/git-scm.com.git";
        let pulls: serde_json::Value = ureq::get(
            "https://api.github.com/repos/git/git-scm.com/pulls?state=closed&per_page=30",
        )
        .header("User-Agent", "nucleate-tests")
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
        let head = pulls
            .as_array()
            .unwrap()
            .iter()
            .find(|pull| !pull["merged_at"].is_null())
            .map(|pull| pull["head"]["sha"].as_str().unwrap().to_string())
            .expect("a merged pull request in the sample");
        assert!(merged(API_URL, remote, &head, None));
        assert!(!merged(API_URL, remote, &"0".repeat(40), None));
    }

    #[test]
    fn recognises_github_remotes() {
        let want = Some(("octo".to_string(), "demo".to_string()));
        assert_eq!(slug("https://github.com/octo/demo.git"), want);
        assert_eq!(slug("https://github.com/octo/demo"), want);
        assert_eq!(slug("git@github.com:octo/demo.git"), want);
        assert_eq!(slug("ssh://git@github.com/octo/demo.git/"), want);
    }

    #[test]
    fn ignores_other_remotes() {
        assert_eq!(slug("/srv/git/demo.git"), None);
        assert_eq!(slug("https://example.com/octo/demo.git"), None);
        assert_eq!(slug("https://github.com/octo"), None);
    }

    #[test]
    fn only_this_machine_can_stand_in_for_github() {
        assert_eq!(
            stand_in("http://127.0.0.1:4010/").as_deref(),
            Some("http://127.0.0.1:4010")
        );
        assert!(stand_in("http://localhost:4010").is_some());
        assert!(stand_in("http://[::1]:4010").is_some());
        assert_eq!(stand_in("https://api.github.com"), None);
        assert_eq!(stand_in("https://github.example.test"), None);
        assert_eq!(stand_in("http://github.example.test"), None);
        assert_eq!(stand_in("http://127.0.0.1.example.test"), None);
        assert_eq!(stand_in("not an address"), None);
    }

    #[test]
    fn a_pull_request_is_read_for_five_fields_and_nothing_else() {
        let listed = r#"[{
            "number": 7,
            "html_url": "https://github.com/octo/demo/pull/7",
            "state": "open",
            "merged_at": null,
            "title": "Fix the login page",
            "body": "A long explanation.",
            "user": { "login": "someone" },
            "head": { "sha": "abc123", "ref": "fix/login", "label": "octo:fix/login" }
        }, {
            "state": "closed",
            "merged_at": "2026-11-19T12:00:00Z",
            "head": { "sha": "def456" }
        }, {}]"#;
        let pulls: Vec<Pull> = serde_json::from_str(listed).unwrap();
        assert!(pulls[0].head_is("abc123") && pulls[0].merged_at.is_none());
        assert_eq!(pulls[0].number, Some(7));
        assert_eq!(pulls[0].state.as_deref(), Some("open"));
        assert!(pulls[1].head_is("def456") && pulls[1].merged_at.is_some());
        assert!(!pulls[2].head_is("abc123"));
    }
}
