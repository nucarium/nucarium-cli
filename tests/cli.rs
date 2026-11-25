//! Behaviour tests: each one builds a real upstream repository and a real
//! workspace in a temporary folder and drives the compiled binary.

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    home: PathBuf,
    upstream: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let home = base.join("home");
        let upstream = base.join("upstream").join("demo");
        let root = base.join("ws");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&upstream).unwrap();
        fs::create_dir_all(root.join(".repos")).unwrap();
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        let fixture = Fixture {
            _tmp: tmp,
            home,
            upstream,
            root,
        };
        fixture.git(&fixture.upstream, &["init", "-q", "-b", "main"]);
        fs::write(fixture.upstream.join("README.md"), "demo\n").unwrap();
        fs::write(fixture.upstream.join(".worktreeinclude"), ".env\n").unwrap();
        fs::write(fixture.upstream.join(".gitignore"), ".env\n").unwrap();
        fixture.git(&fixture.upstream, &["add", "."]);
        fixture.git(&fixture.upstream, &["commit", "-q", "-m", "init"]);
        let bare = fixture.bare();
        fixture.git(
            &fixture.root,
            &[
                "clone",
                "-q",
                "--bare",
                fixture.upstream.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        fixture.git(
            &bare,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        fixture.git(&bare, &["fetch", "-q", "origin"]);
        fixture.git(&bare, &["remote", "set-head", "origin", "--auto"]);
        fixture
    }

    fn bare(&self) -> PathBuf {
        self.root.join(".repos").join("demo.git")
    }

    fn session(&self, name: &str) -> PathBuf {
        self.root.join(".worktrees").join(name)
    }

    fn isolate(&self, command: &mut Command) {
        command
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", self.global_config())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("NUCLEATE_CONFIG")
            .env_remove("NUCLEATE_GITHUB_TOKEN_FILE");
    }

    /// The test user's git settings: none, until a test gives them some.
    fn global_config(&self) -> PathBuf {
        let file = self.home.join(".gitconfig");
        if file.is_file() {
            file
        } else {
            PathBuf::from("/dev/null")
        }
    }

    /// Have the test user's git fetch `https://github.com/octo/<name>` from
    /// `upstream/<name>` on this machine, so nothing reaches GitHub, and make
    /// the demo repo octo's.
    fn github_on_disk(&self) {
        let base = self.upstream.parent().unwrap();
        fs::write(
            self.home.join(".gitconfig"),
            format!(
                "[url \"{}/\"]\n\tinsteadOf = https://github.com/octo/\n",
                base.display()
            ),
        )
        .unwrap();
        self.git(
            &self.bare(),
            &[
                "config",
                "remote.origin.url",
                "https://github.com/octo/demo",
            ],
        );
    }

    /// A repo on "GitHub" that the box has not fetched: `octo/<name>`.
    fn on_github(&self, name: &str) {
        let dir = self.upstream.parent().unwrap().join(format!("{name}.git"));
        fs::create_dir_all(&dir).unwrap();
        self.git(&dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("README.md"), format!("{name}\n")).unwrap();
        self.git(&dir, &["add", "."]);
        self.git(&dir, &["commit", "-q", "-m", "init"]);
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.current_dir(dir).args(args);
        self.isolate(&mut command);
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nucleate"));
        command
            .current_dir(dir)
            .args(args)
            .env("NUCLEATE_ROOT", &self.root);
        self.isolate(&mut command);
        command
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }

    /// Run and expect success; returns the JSON it printed.
    fn ok(&self, dir: &Path, args: &[&str]) -> Value {
        let out = self.run(dir, args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    /// Run and expect failure; returns what it said.
    fn fails(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run(dir, args);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).to_string()
    }

    fn with_stdin(&self, dir: &Path, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// A session named `name` with the demo repo added.
    fn session_with_demo(&self, name: &str) -> PathBuf {
        self.ok(&self.root, &["create-session", name, "demo"]);
        self.session(name)
    }
}

#[test]
fn lists_available_repos() {
    let f = Fixture::new();
    let available = f.ok(&f.root, &["available"]);
    assert_eq!(available[0]["repo"], "demo");
}

#[test]
fn finds_the_workspace_root_from_inside_it() {
    let f = Fixture::new();
    let mut command = f.command(&f.root.join(".worktrees"), &["available"]);
    command.env_remove("NUCLEATE_ROOT");
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let available: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(available[0]["repo"], "demo");
}

#[test]
fn a_new_session_gets_a_worktree_on_its_own_branch() {
    let f = Fixture::new();
    f.ok(&f.root, &["create-session", "s1"]);
    let session = f.session("s1");
    // The session is inferred from the current folder.
    let added = f.ok(&session, &["add", "demo"]);
    assert_eq!(added["branch"], "workspace/s1");
    assert_eq!(
        f.git(&session.join("demo"), &["branch", "--show-current"]),
        "workspace/s1"
    );
    assert!(session.join("demo").join("README.md").is_file());

    let status = f.ok(&session.join("demo"), &["status"]);
    assert_eq!(status["session"], "s1");
    assert_eq!(status["repos"]["demo"]["dirty"], false);
    assert!(fs::read_to_string(session.join("AGENTS.md"))
        .unwrap()
        .contains("demo/"));
}

#[test]
fn adding_the_same_repo_twice_is_harmless() {
    let f = Fixture::new();
    let session = f.session_with_demo("s1");
    let again = f.ok(&session, &["add", "demo"]);
    assert_eq!(again["already_present"], true);
}

#[test]
fn unknown_repos_are_refused() {
    let f = Fixture::new();
    f.ok(&f.root, &["create-session", "s1"]);
    assert!(f
        .fails(&f.session("s1"), &["add", "nope"])
        .contains("not available"));
}

#[test]
fn a_github_repo_not_on_the_box_is_fetched_and_added() {
    let f = Fixture::new();
    f.github_on_disk();
    f.on_github("other");
    f.ok(&f.root, &["create-session", "s1"]);
    let added = f.ok(&f.session("s1"), &["add", "octo/other"]);
    assert_eq!(added["repo"], "other");
    assert!(f.session("s1").join("other").join("README.md").is_file());
    let labels: Vec<String> = f
        .ok(&f.root, &["available"])
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["label"].as_str().unwrap().to_string())
        .collect();
    assert!(labels.contains(&"octo/other".to_string()), "{labels:?}");

    // A second session finds it on the box.
    f.ok(&f.root, &["create-session", "s2"]);
    f.ok(&f.session("s2"), &["add", "OCTO/other"]);
    assert!(f.session("s2").join("other").join("README.md").is_file());
}

#[test]
fn a_repo_github_does_not_have_is_refused_and_leaves_nothing() {
    let f = Fixture::new();
    f.github_on_disk();
    f.ok(&f.root, &["create-session", "s1"]);
    let said = f.fails(&f.session("s1"), &["add", "octo/missing"]);
    assert!(said.contains("cannot fetch octo/missing"), "{said}");
    assert!(!f.root.join(".repos").join("missing.git").exists());
}

#[test]
fn a_repo_from_an_owner_the_box_has_nothing_from_is_refused() {
    let f = Fixture::new();
    f.github_on_disk();
    f.ok(&f.root, &["create-session", "s1"]);
    let said = f.fails(&f.session("s1"), &["add", "someone/other"]);
    assert!(said.contains("no repos from someone"), "{said}");
    assert!(!f.root.join(".repos").join("other.git").exists());
}

#[test]
fn another_owners_repo_under_a_name_already_here_is_refused() {
    let f = Fixture::new();
    f.github_on_disk();
    f.git(
        &f.bare(),
        &[
            "config",
            "remote.origin.url",
            "https://github.com/octo/demo",
        ],
    );
    f.ok(&f.root, &["create-session", "s1"]);
    let said = f.fails(&f.session("s1"), &["add", "someone/demo"]);
    assert!(said.contains("octo/demo is already here"), "{said}");
    f.ok(&f.session("s1"), &["add", "octo/demo"]);
    assert!(f.session("s1").join("demo").join("README.md").is_file());
}

#[test]
fn seed_files_named_by_the_repo_are_copied_in() {
    let f = Fixture::new();
    let seeds = f.root.join(".seeds").join("demo");
    fs::create_dir_all(&seeds).unwrap();
    fs::write(seeds.join(".env"), "SECRET=1\n").unwrap();
    fs::write(seeds.join("not-listed.txt"), "x\n").unwrap();
    let session = f.session_with_demo("s1");
    assert_eq!(
        fs::read_to_string(session.join("demo").join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert!(!session.join("demo").join("not-listed.txt").exists());
}

#[test]
fn a_clean_session_is_removed_with_its_branch() {
    let f = Fixture::new();
    let session = f.session_with_demo("s1");
    f.ok(&f.root, &["remove-session", "s1"]);
    assert!(!session.exists());
    assert_eq!(f.git(&f.bare(), &["branch", "--list", "workspace/s1"]), "");
}

#[test]
fn uncommitted_work_blocks_removal() {
    let f = Fixture::new();
    let session = f.session_with_demo("s1");
    fs::write(session.join("demo").join("scratch.txt"), "wip\n").unwrap();
    assert!(f
        .fails(&session, &["remove-session"])
        .contains("uncommitted changes"));
    assert!(session.join("demo").join("scratch.txt").is_file());
}

#[test]
fn unpushed_commits_block_removal() {
    let f = Fixture::new();
    let session = f.session_with_demo("s1");
    let demo = session.join("demo");
    fs::write(demo.join("new.txt"), "work\n").unwrap();
    f.git(&demo, &["add", "."]);
    f.git(&demo, &["commit", "-q", "-m", "work"]);
    assert!(f
        .fails(&session, &["remove-session"])
        .contains("refusing to remove session"));
    assert!(demo.join("new.txt").is_file());
}

#[test]
fn pushed_work_can_be_removed() {
    let f = Fixture::new();
    let session = f.session_with_demo("s1");
    let demo = session.join("demo");
    fs::write(demo.join("new.txt"), "work\n").unwrap();
    f.git(&demo, &["add", "."]);
    f.git(&demo, &["commit", "-q", "-m", "work"]);
    f.git(&demo, &["push", "-q", "-u", "origin", "workspace/s1"]);
    f.ok(&session, &["remove-session"]);
    assert!(!session.exists());
}

#[test]
fn force_removes_one_repo_despite_unsaved_work() {
    let f = Fixture::new();
    let session = f.session_with_demo("s1");
    fs::write(session.join("demo").join("scratch.txt"), "wip\n").unwrap();
    assert!(f
        .fails(&session, &["remove", "demo"])
        .contains("uncommitted changes"));
    f.ok(&session, &["remove", "demo", "--force"]);
    assert!(!session.join("demo").exists());
    assert!(session.is_dir());
}

#[test]
fn a_branch_that_exists_only_on_the_remote_is_resumed() {
    let f = Fixture::new();
    f.git(&f.upstream, &["checkout", "-q", "-b", "workspace/old"]);
    fs::write(f.upstream.join("old.txt"), "earlier work\n").unwrap();
    f.git(&f.upstream, &["add", "."]);
    f.git(&f.upstream, &["commit", "-q", "-m", "earlier"]);
    f.git(&f.upstream, &["checkout", "-q", "main"]);

    f.ok(&f.root, &["create-session", "s2"]);
    f.ok(
        &f.root,
        &["add", "demo", "workspace/old", "--session", "s2"],
    );
    assert!(f.session("s2").join("demo").join("old.txt").is_file());
}

#[test]
fn refresh_drops_branches_deleted_on_the_remote() {
    let f = Fixture::new();
    f.git(&f.upstream, &["branch", "gone"]);
    f.ok(&f.root, &["refresh"]);
    assert!(f.git(&f.bare(), &["branch", "-r"]).contains("origin/gone"));
    f.git(&f.upstream, &["branch", "-D", "gone"]);
    let results = f.ok(&f.root, &["refresh"]);
    assert_eq!(results[0]["ok"], true);
    assert!(!f.git(&f.bare(), &["branch", "-r"]).contains("origin/gone"));
}

#[test]
fn the_create_hook_prints_only_the_new_session_path() {
    let f = Fixture::new();
    let payload = format!(r#"{{"name":"hooked","cwd":"{}"}}"#, f.root.display());
    let out = f.with_stdin(&f.root, &["hook-create", "demo"], &payload);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(PathBuf::from(&printed), f.session("hooked"));
    assert!(f.session("hooked").join("demo").join("README.md").is_file());
}

#[test]
fn the_create_hook_inside_a_session_checks_out_a_sibling_at_the_sessions_commit() {
    let f = Fixture::new();
    let checkout = f.session_with_demo("s1").join("demo");
    fs::write(checkout.join("work.txt"), "work\n").unwrap();
    f.git(&checkout, &["add", "."]);
    f.git(&checkout, &["commit", "-q", "-m", "work"]);
    let inside = checkout.join("src");
    fs::create_dir_all(&inside).unwrap();

    let payload = format!(r#"{{"name":"helper","cwd":"{}"}}"#, inside.display());
    let out = f.with_stdin(&f.root, &["hook-create"], &payload);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sibling = f.session("s1").join(".subworktrees").join("helper-demo");
    assert_eq!(
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()),
        sibling
    );
    assert!(sibling.join("work.txt").is_file());
    assert_eq!(
        f.git(&sibling, &["rev-parse", "HEAD"]),
        f.git(&checkout, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        f.git(&sibling, &["branch", "--show-current"]),
        "workspace/helper"
    );
}

#[test]
fn the_remove_hook_leaves_a_session_with_unsaved_work() {
    let f = Fixture::new();
    let session = f.session_with_demo("s1");
    fs::write(session.join("demo").join("scratch.txt"), "wip\n").unwrap();
    let payload = format!(r#"{{"worktree_path":"{}"}}"#, session.display());
    let out = f.with_stdin(&f.root, &["hook-remove"], &payload);
    assert!(!out.status.success());
    assert!(session.join("demo").join("scratch.txt").is_file());

    fs::remove_file(session.join("demo").join("scratch.txt")).unwrap();
    let out = f.with_stdin(&f.root, &["hook-remove"], &payload);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!session.exists());
}

#[test]
fn the_mcp_server_lists_its_tools_and_adds_a_repo() {
    let f = Fixture::new();
    f.ok(&f.root, &["create-session", "s1"]);
    let session = f.session("s1");
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add_repo","arguments":{"repo":"demo"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"add_repo","arguments":{"repo":"nope"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":5,"method":"no/such"}"#,
        "\n",
    );
    let out = f.with_stdin(&session, &["mcp"], input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        replies.len(),
        5,
        "one reply per request, none for the notification"
    );
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "nucleate");
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["list_repos", "add_repo", "remove_repo", "workspace_status"]
    );
    assert_eq!(replies[2]["result"]["isError"], false);
    assert!(session.join("demo").join("README.md").is_file());
    assert_eq!(replies[3]["result"]["isError"], true);
    assert_eq!(replies[4]["error"]["code"], -32601);
}
