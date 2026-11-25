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
        let bare = fixture.root.join(".repos").join("_").join("demo.git");
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

    /// The demo repo's bare clone, in whichever owner folder it is.
    fn bare(&self) -> PathBuf {
        let repos = self.root.join(".repos");
        fs::read_dir(&repos)
            .unwrap()
            .flatten()
            .map(|owner| owner.path().join("demo.git"))
            .find(|bare| bare.is_dir())
            .unwrap_or_else(|| repos.join("_").join("demo.git"))
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

    /// Where "GitHub" keeps `owner`'s repos on this machine.
    fn github_owner(&self, owner: &str) -> PathBuf {
        let base = self.upstream.parent().unwrap();
        match owner {
            "octo" => base.to_path_buf(),
            other => base.parent().unwrap().join(other),
        }
    }

    /// Have the test user's git fetch `https://github.com/<owner>/<name>`,
    /// for owners `octo` and `other`, from folders on this machine, so
    /// nothing reaches GitHub, and make the demo repo octo's: it moves to
    /// octo's folder, where cloning it from GitHub would have put it.
    fn github_on_disk(&self) {
        let mut config = String::new();
        for owner in ["octo", "other"] {
            config.push_str(&format!(
                "[url \"{}/\"]\n\tinsteadOf = https://github.com/{owner}/\n",
                self.github_owner(owner).display()
            ));
        }
        fs::write(self.home.join(".gitconfig"), config).unwrap();
        let bare = self.bare();
        self.git(
            &bare,
            &[
                "config",
                "remote.origin.url",
                "https://github.com/octo/demo",
            ],
        );
        let octo = self.root.join(".repos").join("octo");
        fs::create_dir_all(&octo).unwrap();
        fs::rename(&bare, octo.join("demo.git")).unwrap();
    }

    /// A repo on "GitHub" that the box has not fetched: `octo/<name>`.
    fn on_github(&self, name: &str) {
        self.on_github_as("octo", name);
    }

    /// A repo on "GitHub" that the box has not fetched: `<owner>/<name>`.
    fn on_github_as(&self, owner: &str, name: &str) {
        let dir = self.github_owner(owner).join(format!("{name}.git"));
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

    /// A repo on this machine named `name`, with one commit, made available
    /// with `nucleate clone`.
    fn local_repo(&self, name: &str) -> PathBuf {
        let dir = self
            .upstream
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("local")
            .join(name);
        fs::create_dir_all(&dir).unwrap();
        self.git(&dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("README.md"), format!("{name}\n")).unwrap();
        self.git(&dir, &["add", "."]);
        self.git(&dir, &["commit", "-q", "-m", "init"]);
        self.ok(&self.root, &["clone", dir.to_str().unwrap()]);
        dir
    }

    /// Put the nucleus back as a nucleate before owner folders left it: every
    /// bare repo at `.repos/<name>.git`, its worktrees pointing there, and
    /// every session's manifest naming it there.
    fn flatten(&self) {
        let repos = self.root.join(".repos");
        let mut moved = Vec::new();
        for owner in fs::read_dir(&repos).unwrap().flatten() {
            if !owner.path().is_dir() || owner.file_name().to_string_lossy().ends_with(".git") {
                continue;
            }
            for repo in fs::read_dir(owner.path()).unwrap().flatten() {
                let flat = repos.join(repo.file_name());
                fs::rename(repo.path(), &flat).unwrap();
                self.git(&flat, &["--git-dir", ".", "worktree", "repair"]);
                moved.push((repo.path(), flat));
            }
            fs::remove_dir(owner.path()).unwrap();
        }
        for session in fs::read_dir(self.root.join(".worktrees"))
            .unwrap()
            .flatten()
        {
            let file = session.path().join(".workspace.json");
            let Ok(mut text) = fs::read_to_string(&file) else {
                continue;
            };
            for (nested, flat) in &moved {
                text = text.replace(nested.to_str().unwrap(), flat.to_str().unwrap());
            }
            fs::write(&file, text).unwrap();
        }
    }

    /// The bare repo a worktree belongs to, as git finds it.
    fn common_dir(&self, worktree: &Path) -> PathBuf {
        PathBuf::from(self.git(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ))
        .canonicalize()
        .unwrap()
    }

    /// The `bare` a session's manifest names for one of its repos.
    fn manifest_bare(&self, session: &str, repo: &str) -> PathBuf {
        let file = self.session(session).join(".workspace.json");
        let manifest: Value = serde_json::from_str(&fs::read_to_string(file).unwrap()).unwrap();
        PathBuf::from(manifest["repos"][repo]["bare"].as_str().unwrap())
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
    assert!(!f.root.join(".repos/octo/missing.git").exists());
    // From an owner the box has nothing from, not even the owner's folder.
    let said = f.fails(&f.session("s1"), &["add", "nobody/missing"]);
    assert!(said.contains("cannot fetch nobody/missing"), "{said}");
    assert!(!f.root.join(".repos/nobody").exists());
}

#[test]
fn a_github_repo_is_cloned_into_its_owners_folder_once() {
    let f = Fixture::new();
    f.github_on_disk();
    f.on_github("other");
    let cloned = f.ok(&f.root, &["clone", "https://github.com/octo/other.git"]);
    let bare = f.root.join(".repos/octo/other.git");
    assert_eq!(cloned["bare"], bare.to_str().unwrap());
    assert_eq!(cloned["label"], "octo/other");
    assert_eq!(cloned["repo"], "other");
    assert_eq!(cloned["already_present"], false);
    // Again, spelled differently: the same repo, found and left as it is.
    let again = f.ok(&f.root, &["clone", "https://github.com/OCTO/other"]);
    assert_eq!(again["already_present"], true);
    assert_eq!(again["bare"], bare.to_str().unwrap());
    let owners: Vec<String> = fs::read_dir(f.root.join(".repos"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.eq_ignore_ascii_case("octo"))
        .collect();
    assert_eq!(owners, ["octo"]);
}

#[test]
fn a_repo_with_no_github_owner_is_cloned_into_the_folder_for_none() {
    let f = Fixture::new();
    f.local_repo("tool");
    assert!(f.root.join(".repos/_/tool.git").is_dir());
    let labels: Vec<Value> = f.ok(&f.root, &["available"]).as_array().unwrap().clone();
    assert!(labels
        .iter()
        .any(|a| a["label"] == "tool" && a["repo"] == "tool"));
}

#[test]
fn a_repo_from_any_owner_is_fetched_into_its_own_folder() {
    let f = Fixture::new();
    f.github_on_disk();
    f.on_github_as("other", "demo");
    f.ok(&f.root, &["create-session", "s1"]);
    let added = f.ok(&f.session("s1"), &["add", "other/demo"]);
    assert_eq!(added["label"], "other/demo");
    assert!(f.root.join(".repos/other/demo.git").is_dir());
    assert!(f.root.join(".repos/octo/demo.git").is_dir());

    // Two owners have a demo: the name alone no longer says which.
    f.ok(&f.root, &["create-session", "s2"]);
    let said = f.fails(&f.session("s2"), &["add", "demo"]);
    assert!(
        said.contains("octo/demo") && said.contains("other/demo"),
        "{said}"
    );
    let added = f.ok(&f.session("s2"), &["add", "octo/demo"]);
    assert_eq!(added["label"], "octo/demo");

    // One session holds one demo: the other cannot be added beside it.
    let said = f.fails(&f.session("s1"), &["add", "octo/demo"]);
    assert!(
        said.contains("other/demo") && said.contains("octo/demo"),
        "{said}"
    );
    let again = f.ok(&f.session("s1"), &["add", "OTHER/demo"]);
    assert_eq!(again["already_present"], true);
}

#[test]
fn a_repo_is_removed_from_a_session_by_the_name_the_session_has_it_under() {
    let f = Fixture::new();
    f.github_on_disk();
    f.on_github_as("other", "demo");
    f.ok(&f.root, &["clone", "https://github.com/other/demo.git"]);
    f.ok(&f.root, &["create-session", "s1", "octo/demo"]);
    let said = f.fails(&f.session("s1"), &["remove", "other/demo"]);
    assert!(said.contains("not in session"), "{said}");
    f.ok(&f.session("s1"), &["remove", "demo"]);
    assert!(!f.session("s1").join("demo").exists());
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

// ---- moving a nucleus made before owner folders ---------------------------

#[test]
fn a_flat_nucleus_with_a_live_session_moves_to_owner_folders() {
    let f = Fixture::new();
    f.github_on_disk();
    f.local_repo("tool");
    f.ok(&f.root, &["create-session", "s1", "demo", "tool"]);
    let demo = f.session("s1").join("demo");
    let tool = f.session("s1").join("tool");
    fs::write(demo.join("work.txt"), "work\n").unwrap();
    f.git(&demo, &["add", "."]);
    f.git(&demo, &["commit", "-q", "-m", "work"]);
    let payload = format!(r#"{{"name":"helper","cwd":"{}"}}"#, demo.display());
    let out = f.with_stdin(&f.root, &["hook-create"], &payload);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let helper = f.session("s1").join(".subworktrees").join("helper-demo");

    f.flatten();
    let repos = f.root.join(".repos");
    assert_eq!(f.common_dir(&demo), repos.join("demo.git"));
    assert_eq!(f.manifest_bare("s1", "demo"), repos.join("demo.git"));

    // Run from inside another repo's worktree, which must not be touched.
    let out = f.run(&tool, &["refresh"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("moved"));

    assert!(!repos.join("demo.git").exists());
    assert!(!repos.join("tool.git").exists());
    let octo = repos.join("octo/demo.git");
    let none = repos.join("_/tool.git");
    assert_eq!(f.common_dir(&demo), octo);
    assert_eq!(f.common_dir(&helper), octo);
    assert_eq!(f.common_dir(&tool), none);
    assert_eq!(f.manifest_bare("s1", "demo"), octo);
    assert_eq!(f.manifest_bare("s1", "tool"), none);

    // The session works as before.
    fs::write(demo.join("more.txt"), "more\n").unwrap();
    f.git(&demo, &["add", "."]);
    f.git(&demo, &["commit", "-q", "-m", "more"]);
    let status = f.ok(&f.session("s1"), &["status"]);
    assert_eq!(status["repos"]["demo"]["dirty"], false);
    assert_eq!(status["repos"]["tool"]["dirty"], false);
    let labels: Vec<Value> = status["available"].as_array().unwrap().clone();
    assert!(labels.iter().any(|a| a["label"] == "octo/demo"));
    f.ok(&f.session("s1"), &["remove", "tool"]);
    assert!(!tool.exists());

    // A second run finds nothing to do.
    let out = f.run(&f.root, &["refresh"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(f.common_dir(&demo), octo);
}

#[test]
fn a_move_that_stopped_part_way_is_finished() {
    let f = Fixture::new();
    f.github_on_disk();
    let demo = f.session_with_demo("s1").join("demo");
    f.flatten();
    let repos = f.root.join(".repos");
    let octo = repos.join("octo/demo.git");
    fs::create_dir_all(octo.parent().unwrap()).unwrap();

    // Stopped after the move and its repair, before the manifests: reading
    // the manifest finds the repo where it is now.
    fs::rename(repos.join("demo.git"), &octo).unwrap();
    f.git(&octo, &["--git-dir", ".", "worktree", "repair"]);
    let status = f.ok(&f.session("s1"), &["status"]);
    assert_eq!(status["repos"]["demo"]["bare"], octo.to_str().unwrap());

    // Stopped straight after the move: the worktree still points at the
    // old place, and the next run repairs it and the manifest.
    f.flatten();
    fs::create_dir_all(octo.parent().unwrap()).unwrap();
    fs::rename(repos.join("demo.git"), &octo).unwrap();
    let out = f.run(&f.root, &["refresh"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.common_dir(&demo), octo);
    assert_eq!(f.manifest_bare("s1", "demo"), octo);
    f.ok(&f.session("s1"), &["status"]);
}

#[test]
fn a_flat_repo_whose_new_place_is_taken_stays_and_the_rest_move() {
    let f = Fixture::new();
    f.github_on_disk();
    f.local_repo("tool");
    f.flatten();
    let repos = f.root.join(".repos");
    f.git(
        &f.root,
        &[
            "clone",
            "-q",
            "--bare",
            f.upstream.to_str().unwrap(),
            repos.join("octo/demo.git").to_str().unwrap(),
        ],
    );
    let out = f.run(&f.root, &["refresh"]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains(&format!("left {}", repos.join("demo.git").display())),
        "{said}"
    );
    assert!(
        said.contains(repos.join("octo/demo.git").to_str().unwrap()),
        "{said}"
    );
    assert!(repos.join("demo.git").is_dir());
    assert!(repos.join("_/tool.git").is_dir());
    assert!(!repos.join("tool.git").exists());
}

#[test]
fn a_refused_flat_repo_stays_where_it_is_and_is_still_listed() {
    let f = Fixture::new();
    f.local_repo("tool");
    f.flatten();
    let repos = f.root.join(".repos");
    f.git(&repos.join("demo.git"), &["config", "pull.rebase", "true"]);
    let out = f.run(&f.root, &["refresh"]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(said.contains("refused"), "{said}");
    assert!(repos.join("demo.git").is_dir());
    assert!(repos.join("_/tool.git").is_dir());
    let listed = f.ok(&f.root, &["available"]);
    assert!(listed
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["repo"] == "demo" && a["label"] == "demo"));
}
