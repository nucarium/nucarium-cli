//! The one question nucleate asks GitHub: was this commit merged?

use serde_json::Value;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

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

/// True when a merged pull request's head is exactly this commit.
///
/// A squash merge rewrites the commits and the branch is then deleted, so
/// asking GitHub is the only way to recognise finished work. Any failure to
/// get an answer counts as "no": nucleate never deletes on doubt.
pub fn merged(remote_url: &str, sha: &str, token_file: Option<&Path>) -> bool {
    let Some((owner, repo)) = slug(remote_url) else {
        return false;
    };
    let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{sha}/pulls");
    let mut request = ureq::get(&url)
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "nucleate");
    if let Some(token) = token(token_file) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let Ok(mut response) = request.call() else {
        return false;
    };
    let Ok(Value::Array(pulls)) = response.body_mut().read_json::<Value>() else {
        return false;
    };
    pulls
        .iter()
        .any(|pull| !pull["merged_at"].is_null() && pull["head"]["sha"].as_str() == Some(sha))
}

#[cfg(test)]
mod tests {
    use super::{merged, slug};

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
        assert!(merged(remote, &head, None));
        assert!(!merged(remote, &"0".repeat(40), None));
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
}
