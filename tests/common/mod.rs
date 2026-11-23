//! A local stand-in for the Watch API, and helpers for driving the binaries
//! in a home folder of their own.
#![allow(dead_code)]

pub mod gate;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tempfile::TempDir;

/// One request the mock received.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// How many requests this path had received before this one.
    pub nth: usize,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// A form-encoded body, decoded.
    pub fn form(&self) -> HashMap<String, String> {
        self.body
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (decode(key), decode(value)))
            .collect()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("a JSON body")
    }
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 2;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// What the mock answers with.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    /// Wait this long before answering: a slow API.
    pub delay: Duration,
}

impl Reply {
    pub fn json(status: u16, body: serde_json::Value) -> Reply {
        Reply {
            status,
            body: body.to_string(),
            content_type: "application/json",
            headers: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    /// An RFC 7807 problem, as the API's JSON endpoints answer.
    pub fn problem(status: u16, code: &str) -> Reply {
        let mut reply = Reply::json(
            status,
            serde_json::json!({
                "type": format!("https://nucarium.dev/problems/{}", code.replace('_', "-")),
                "title": code, "status": status, "code": code,
            }),
        );
        reply.content_type = "application/problem+json";
        reply
    }

    /// An OAuth error, as the token endpoint answers.
    pub fn oauth(error: &str) -> Reply {
        Reply::json(400, serde_json::json!({ "error": error }))
    }

    pub fn header(mut self, name: &'static str, value: &str) -> Reply {
        self.headers.push((name, value.to_string()));
        self
    }

    pub fn after(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }
}

pub struct Mock {
    pub url: String,
    /// The fields ahead of the contract that this stand-in says it takes, and
    /// so may be sent: names from [`contract::AHEAD`]. None, unless whoever
    /// started it says otherwise, as for Watch today.
    pub ahead: &'static [&'static str],
    requests: Arc<Mutex<Vec<Request>>>,
    server: Arc<tiny_http::Server>,
    thread: Option<JoinHandle<()>>,
}

impl Mock {
    /// Start a server on a free local port that answers each request with
    /// whatever `handler` returns.
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + 'static) -> Mock {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::<Request>::new()));
        let (seen, listener) = (requests.clone(), server.clone());
        let thread = std::thread::spawn(move || {
            for mut incoming in listener.incoming_requests() {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(incoming.as_reader(), &mut body);
                let path = incoming.url().to_string();
                let request = {
                    let mut seen = seen.lock().unwrap();
                    let request = Request {
                        method: incoming.method().to_string(),
                        nth: seen.iter().filter(|r| r.path == path).count(),
                        path,
                        headers: incoming
                            .headers()
                            .iter()
                            .map(|h| (h.field.to_string(), h.value.to_string()))
                            .collect(),
                        body,
                    };
                    seen.push(request.clone());
                    request
                };
                let reply = handler(&request);
                std::thread::sleep(reply.delay);
                let mut response = tiny_http::Response::from_string(reply.body)
                    .with_status_code(reply.status)
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", reply.content_type).unwrap(),
                    );
                for (name, value) in &reply.headers {
                    response
                        .add_header(tiny_http::Header::from_bytes(*name, value.as_str()).unwrap());
                }
                let _ = incoming.respond(response);
            }
        });
        Mock {
            url: format!("http://127.0.0.1:{port}"),
            ahead: &[],
            requests,
            server,
            thread: Some(thread),
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn to(&self, path: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.path == path)
            .collect()
    }

    /// Everything the mock was sent, headers and bodies, as one string.
    pub fn everything(&self) -> String {
        self.requests()
            .iter()
            .map(|r| format!("{} {}\n{:?}\n{}\n", r.method, r.path, r.headers, r.body))
            .collect()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A stand-in for the GitHub API. It answers the one call nucleate makes,
/// "list pull requests associated with a commit", with whatever `pulls`
/// returns for the `owner/name` and the commit asked about.
pub fn github(pulls: impl Fn(&str, &str) -> Reply + Send + 'static) -> Mock {
    Mock::start(move |request: &Request| {
        let parts: Vec<&str> = request.path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["repos", owner, name, "commits", sha, "pulls"] if request.method == "GET" => {
                pulls(&format!("{owner}/{name}"), sha)
            }
            _ => Reply::json(404, serde_json::json!({ "message": "Not Found" })),
        }
    })
}

/// A folder of stand-in programs, each a shell script, to go first on a
/// command's `PATH`. Returns the `PATH` to use.
pub fn path_with(home: &Home, programs: &[(&str, &str)]) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.dir.join("stand-ins");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, script) in programs {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).unwrap()
}

/// The address of an API that is not there: a port nothing listens on.
pub fn dead_api() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

/// A home folder for one test, with the paths the binaries use inside it.
pub struct Home {
    _tmp: TempDir,
    pub dir: PathBuf,
}

impl Home {
    pub fn new() -> Home {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        Home { _tmp: tmp, dir }
    }

    pub fn token_file(&self) -> PathBuf {
        self.dir.join(".config/nucleated/token")
    }

    pub fn login_file(&self) -> PathBuf {
        self.dir.join(".config/nucleated/login.json")
    }

    pub fn config_file(&self) -> PathBuf {
        self.dir.join(".config/nucleated/config.toml")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.join(".local/state/nucleated")
    }

    /// Where the daemon keeps what may hold content, outside the state folder.
    pub fn cache_dir(&self) -> PathBuf {
        self.dir.join(".cache/nucleated")
    }

    /// Where a test plays the box recipe, writing each environment's setup.
    /// Nothing is there unless a test puts it there.
    pub fn setup_dir(&self) -> PathBuf {
        self.dir.join("environments")
    }

    /// The test user's own global git config. Nothing writes it unless a
    /// test asks; a missing file is read as an empty one.
    pub fn gitconfig(&self) -> PathBuf {
        self.dir.join(".gitconfig")
    }

    pub fn write_config(&self, text: &str) {
        let path = self.config_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.dir)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_GLOBAL", self.gitconfig())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        command
    }

    /// Run git as the test user, with their global config, in `dir`.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn nucleate(&self) -> Command {
        self.command(env!("CARGO_BIN_EXE_nucleate"))
    }

    pub fn nucleated(&self) -> Command {
        self.command(env!("CARGO_BIN_EXE_nucleated"))
    }
}

/// Run a command with `stdin` on its standard input and collect what it prints.
pub fn run(command: &mut Command, stdin: &str) -> Output {
    use std::io::Write;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A program is free to finish without reading its input, as a hook does
    // for an event it has no use for. Writing to it then fails with a broken
    // pipe, which says nothing about the program.
    match child.stdin.take().unwrap().write_all(stdin.as_bytes()) {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => panic!("{error}"),
        _ => {}
    }
    child.wait_with_output().unwrap()
}

pub fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON: {:?}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

pub fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Run git with a fixed identity and no user or system config.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A program that records that it ran: it leaves the marker file, the
/// second path returned. Called as a filter, with nothing on its command
/// line, it passes its input through unchanged; called any other way, it
/// fails at once.
pub fn marker_program(home: &Home) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let marker = home.dir.join("a-program-ran");
    let program = home.dir.join("planted.sh");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ntouch '{}'\n[ $# -eq 0 ] && exec cat\nexit 1\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    (program, marker)
}

/// A box with one environment, `client-a`: an upstream repo, a nucleus made
/// with `nucleate init` and `nucleate clone`, and a daemon config whose every
/// path is inside the test's own folder.
pub struct Rig {
    pub home: Home,
    pub root: PathBuf,
    pub upstream: PathBuf,
}

pub const TOKEN: &str = "nuc_4m7q1x8z5b6n2f9k_3Yt0v1c2SECRETSECRET";

impl Rig {
    pub fn new() -> Rig {
        Rig::under("")
    }

    /// As `new`, with the nucleus and the upstream below a folder of this
    /// name, for tests about what a path may give away.
    pub fn under(parent: &str) -> Rig {
        let home = Home::new();
        let base = home.dir.join(parent);
        let upstream = base.join("upstream").join("web");
        let root = base.join("client-a");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q", "-b", "main"]);
        std::fs::write(upstream.join("README.md"), "web\n").unwrap();
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "-q", "-m", "init"]);
        let rig = Rig {
            home,
            root,
            upstream,
        };
        let made = run(rig.nucleate().arg("init"), "");
        assert!(made.status.success(), "{}", stderr(&made));
        let cloned = run(
            rig.nucleate()
                .args(["clone", rig.upstream.to_str().unwrap()]),
            "",
        );
        assert!(cloned.status.success(), "{}", stderr(&cloned));
        rig.configure("");
        rig
    }

    /// Write the daemon's config: one environment whose agent server is
    /// always alive, plus whatever `extra` adds.
    pub fn configure(&self, extra: &str) {
        self.configure_with("check = \"true\"", extra);
    }

    /// `environment` is added to the environment's table; `top` holds
    /// top-level keys and `tables` whole tables.
    pub fn configure_with(&self, environment: &str, extra: &str) {
        let (top, tables) = match extra.find('[') {
            Some(at) => extra.split_at(at),
            None => (extra, ""),
        };
        self.home.write_config(&format!(
            "machine = \"agentbox\"\nstate_dir = \"{state}\"\ncache_dir = \"{cache}\"\n\
             setup_dir = \"{setup}\"\n{top}\n\
             [[environment]]\nname = \"client-a\"\nroot = \"{root}\"\n{environment}\n\n{tables}\n",
            state = self.home.state_dir().display(),
            cache = self.home.cache_dir().display(),
            setup = self.home.setup_dir().display(),
            root = self.root.display(),
        ));
    }

    pub fn settings(&self) -> nucleate::settings::Settings {
        let mut settings = nucleate::settings::load_from(&self.home.config_file()).unwrap();
        settings.token_file = self.home.token_file();
        settings
    }

    /// Sign the box in to the API at `api_url`, as `nucleate login` leaves it.
    pub fn sign_in(&self, api_url: &str) {
        let out = run(
            self.home
                .nucleate()
                .args(["login", "--token-stdin", "--api-url", api_url]),
            TOKEN,
        );
        assert!(out.status.success(), "{}", stderr(&out));
    }

    pub fn nucleate(&self) -> Command {
        let mut command = self.home.nucleate();
        command.arg("--root").arg(&self.root);
        command
    }

    pub fn nucleated(&self) -> Command {
        self.home.nucleated()
    }

    /// Make the `web` repo's remote read as one on GitHub, so that the repo
    /// has an owner and its pull requests have addresses, while git still
    /// fetches from and pushes to the upstream folder. Nothing here reaches
    /// GitHub: a test that uses this points nucleate at a stand-in. The
    /// rewrite is the test user's own, as a box's git settings are, and not
    /// the repo's: a repo's config holds nothing of the kind.
    pub fn as_github(&self, slug: &str) {
        let bare = self.root.join(".repos/web.git");
        let url = format!("https://github.com/{slug}.git");
        let rewrite = format!("url.{}.insteadOf", self.upstream.display());
        let global = self.home.gitconfig();
        git(
            &bare,
            &["config", "--file", global.to_str().unwrap(), &rewrite, &url],
        );
        git(&bare, &["config", "remote.origin.url", &url]);
    }

    /// Push a worktree's branch and make it the one it tracks, as an agent's
    /// `git push -u` does, with the test user's git settings.
    pub fn push(&self, worktree: &Path) {
        self.home
            .git(worktree, &["push", "-q", "-u", "origin", "HEAD"]);
    }

    /// Create a nucleus session holding the `web` repo; returns its worktree.
    pub fn session(&self, name: &str) -> PathBuf {
        let out = run(self.nucleate().args(["create-session", name, "web"]), "");
        assert!(out.status.success(), "{}", stderr(&out));
        self.root.join(".worktrees").join(name).join("web")
    }

    /// Call the hook entry point as the agent would.
    pub fn hook(&self, event: &str, payload: &serde_json::Value) -> Output {
        run(self.nucleated().args(["hook", event]), &payload.to_string())
    }

    pub fn spool_dir(&self) -> PathBuf {
        self.home.state_dir().join("spool")
    }

    /// The events waiting in the spool, oldest first.
    pub fn spooled(&self) -> Vec<serde_json::Value> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.spool_dir())
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.retain(|path| path.extension().is_some_and(|ext| ext == "json"));
        files.sort();
        files
            .iter()
            .map(|path| serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap())
            .collect()
    }
}

/// Every file under a folder, as (path, contents).
pub fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            found.push((path, bytes));
        }
    }
    found
}

/// The two ingest requests as the Watch contract (0.5.0) defines them, held as
/// strictly as the API holds them: a field the contract does not name, or a
/// value outside its pattern or range, is refused. The stand-in APIs answer
/// through this, so a daemon that sends what Watch would refuse fails here
/// first.
///
/// Five fields of a heartbeat's environment are ahead of 0.5.0: `driver`,
/// `driver_ref`, `config_commit`, `config_modified` and `config_state`. Watch
/// as it is refuses each of them, and so does this, unless a stand-in is
/// started as a later Watch that names them in `accepts` in its answer to a
/// heartbeat: [`Mock::ahead`]. A box sends one only to a Watch that has named
/// it.
pub mod contract {
    use super::{Mock, Reply, Request};
    use serde_json::{Map, Value};

    pub const HEARTBEATS: &str = "/v1/heartbeats:ingest";
    pub const EVENTS: &str = "/v1/events:ingest";

    type Check = Result<(), String>;

    fn chars(value: &str, min: usize, max: usize, allowed: impl Fn(char) -> bool) -> bool {
        (min..=max).contains(&value.chars().count()) && value.chars().all(allowed)
    }

    fn word(c: char) -> bool {
        c.is_ascii_alphanumeric() || "._-".contains(c)
    }

    /// `^[a-z0-9][a-z0-9._-]{0,63}$`
    fn env_key(value: &str) -> bool {
        let lower = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        value.chars().next().is_some_and(lower)
            && chars(value, 1, 64, |c| lower(c) || "._-".contains(c))
    }

    /// `^([0-9a-f]{40}|[0-9a-f]{64})$`
    fn commit(value: &str) -> bool {
        [40, 64].contains(&value.len())
            && value
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }

    /// The fields of a heartbeat's environment that are ahead of 0.5.0.
    pub const AHEAD: [&str; 6] = [
        "driver",
        "driver_ref",
        "config_commit",
        "config_modified",
        "config_state",
        "setup",
    ];

    const NETWORKS: [&str; 3] = ["none", "limited", "full"];
    const SCRIPT_STATES: [&str; 3] = ["running", "ok", "failed"];

    /// The fixed words for how a config sync stands.
    const CONFIG_STATES: [&str; 6] = ["ok", "pending", "held", "unreachable", "rejected", "failed"];

    /// `^[0-9A-Za-z.+-]{1,32}$`
    fn version(value: &str) -> bool {
        chars(value, 1, 32, |c| {
            c.is_ascii_alphanumeric() || ".+-".contains(c)
        })
    }

    /// `^[A-Za-z0-9._-]{1,100}(/[A-Za-z0-9._-]{1,100})?$`
    fn repo(value: &str) -> bool {
        let parts: Vec<&str> = value.split('/').collect();
        parts.len() <= 2 && parts.iter().all(|part| chars(part, 1, 100, word))
    }

    /// `^https://github\.com/<owner>/<name>/pull/[1-9][0-9]{0,9}$`, at most
    /// 240 characters. Answers with the `owner/name` it names.
    fn pull_request(value: &str) -> Option<String> {
        let rest = value.strip_prefix("https://github.com/")?;
        let parts: Vec<&str> = rest.split('/').collect();
        let [owner, name, "pull", number] = parts.as_slice() else {
            return None;
        };
        let numbered = chars(number, 1, 10, |c| c.is_ascii_digit()) && !number.starts_with('0');
        (value.len() <= 240 && chars(owner, 1, 100, word) && chars(name, 1, 100, word) && numbered)
            .then(|| format!("{owner}/{name}"))
    }

    /// RFC 3339: `2026-11-19T12:00:00Z`, with or without a fraction, with `Z`
    /// or an offset.
    fn date_time(value: &str) -> bool {
        let Some((date, time)) = value.split_once(['T', 't']) else {
            return false;
        };
        let digits =
            |text: &str, len: usize| text.len() == len && text.bytes().all(|c| c.is_ascii_digit());
        let number = |text: &str| text.parse::<u32>().unwrap_or(u32::MAX);
        let date: Vec<&str> = date.split('-').collect();
        let [year, month, day] = date.as_slice() else {
            return false;
        };
        let (clock, zone) = match time.find(['Z', 'z', '+', '-']) {
            Some(at) => time.split_at(at),
            None => return false,
        };
        let (clock, fraction) = clock.split_once('.').unwrap_or((clock, "0"));
        let clock: Vec<&str> = clock.split(':').collect();
        let [hour, minute, second] = clock.as_slice() else {
            return false;
        };
        let zone_ok = zone.eq_ignore_ascii_case("z")
            || zone[1..].split_once(':').is_some_and(|(h, m)| {
                digits(h, 2) && digits(m, 2) && number(h) < 24 && number(m) < 60
            });
        digits(year, 4)
            && digits(month, 2)
            && (1..=12).contains(&number(month))
            && digits(day, 2)
            && (1..=31).contains(&number(day))
            && digits(hour, 2)
            && number(hour) < 24
            && digits(minute, 2)
            && number(minute) < 60
            && digits(second, 2)
            && number(second) <= 60
            && !fraction.is_empty()
            && fraction.bytes().all(|c| c.is_ascii_digit())
            && zone_ok
    }

    fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
        value.as_object().ok_or(format!("{what} is not an object"))
    }

    /// `additionalProperties: false`, and every required property present.
    fn closed(map: &Map<String, Value>, what: &str, required: &[&str], optional: &[&str]) -> Check {
        for key in map.keys() {
            if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
                return Err(format!(
                    "{what} has a field the contract does not name: {key}"
                ));
            }
        }
        match required.iter().find(|key| !map.contains_key(**key)) {
            Some(missing) => Err(format!("{what} lacks {missing}")),
            None => Ok(()),
        }
    }

    /// Check one property when it is present.
    fn field(
        map: &Map<String, Value>,
        what: &str,
        key: &str,
        fits: impl Fn(&Value) -> bool,
    ) -> Check {
        match map.get(key) {
            Some(value) if !fits(value) => Err(format!("{what}.{key} does not fit: {value}")),
            _ => Ok(()),
        }
    }

    fn integer(min: u64, max: u64) -> impl Fn(&Value) -> bool {
        move |value| value.as_u64().is_some_and(|n| (min..=max).contains(&n))
    }

    fn number(min: f64, max: f64) -> impl Fn(&Value) -> bool {
        move |value| value.as_f64().is_some_and(|n| (min..=max).contains(&n))
    }

    fn string(fits: impl Fn(&str) -> bool) -> impl Fn(&Value) -> bool {
        move |value| value.as_str().is_some_and(&fits)
    }

    fn boolean(value: &Value) -> bool {
        value.is_boolean()
    }

    /// `HeartbeatRequest`, as 0.5.0 has it.
    pub fn heartbeat(body: &Value) -> Check {
        heartbeat_taking(body, &[])
    }

    /// `HeartbeatRequest`, as a Watch that also takes the `ahead` fields
    /// would have it.
    pub fn heartbeat_taking(body: &Value, ahead: &[&str]) -> Check {
        let top = object(body, "heartbeat")?;
        closed(
            top,
            "heartbeat",
            &["machine", "environments", "client_ts"],
            &["daemon_version", "activity_beat_s", "paused_until"],
        )?;
        field(top, "heartbeat", "daemon_version", string(version))?;
        field(top, "heartbeat", "activity_beat_s", integer(60, 3_600))?;
        field(top, "heartbeat", "paused_until", string(date_time))?;
        field(top, "heartbeat", "client_ts", string(date_time))?;

        let machine = object(&top["machine"], "machine")?;
        closed(
            machine,
            "machine",
            &["disk_used_pct", "load_1m", "uptime_s"],
            &[
                "memory_pressure_pct",
                "on_battery",
                "reboot_required",
                "uncommitted_repos",
                "unpushed_commits",
                "last_snapshot_at",
            ],
        )?;
        field(machine, "machine", "disk_used_pct", number(0.0, 100.0))?;
        field(machine, "machine", "load_1m", number(0.0, 100_000.0))?;
        field(machine, "machine", "uptime_s", integer(0, u64::MAX))?;
        field(
            machine,
            "machine",
            "memory_pressure_pct",
            number(0.0, 100.0),
        )?;
        field(machine, "machine", "on_battery", boolean)?;
        field(machine, "machine", "reboot_required", boolean)?;
        field(machine, "machine", "uncommitted_repos", integer(0, 10_000))?;
        field(
            machine,
            "machine",
            "unpushed_commits",
            integer(0, 1_000_000),
        )?;
        field(machine, "machine", "last_snapshot_at", string(date_time))?;

        let environments = top["environments"]
            .as_array()
            .filter(|list| list.len() <= 40)
            .ok_or("environments is not a list of at most 40")?;
        let mut seen = Vec::new();
        for entry in environments {
            let environment = object(entry, "environment")?;
            let mut optional = vec![
                "server_restarts",
                "agent_version",
                "login_expires_at",
                "last_snapshot_at",
                "unsaved",
            ];
            optional.extend(AHEAD.iter().filter(|field| ahead.contains(field)));
            closed(
                environment,
                "environment",
                &["env_id", "server_alive"],
                &optional,
            )?;
            field(
                environment,
                "environment",
                "driver",
                string(|v| v.len() <= 32 && env_key(v)),
            )?;
            field(
                environment,
                "environment",
                "driver_ref",
                string(|v| chars(v, 1, 128, |c| word(c) || c == ':')),
            )?;
            field(environment, "environment", "config_commit", string(commit))?;
            field(environment, "environment", "config_modified", boolean)?;
            field(
                environment,
                "environment",
                "config_state",
                string(|v| CONFIG_STATES.contains(&v)),
            )?;
            for (needs, beside) in [
                ("driver_ref", "driver"),
                ("config_modified", "config_commit"),
            ] {
                if environment.contains_key(needs) && !environment.contains_key(beside) {
                    return Err(format!("environment has {needs} and no {beside}"));
                }
            }
            field(environment, "environment", "env_id", string(env_key))?;
            field(environment, "environment", "server_alive", boolean)?;
            field(
                environment,
                "environment",
                "server_restarts",
                integer(0, 2_147_483_647),
            )?;
            field(environment, "environment", "agent_version", string(version))?;
            field(
                environment,
                "environment",
                "login_expires_at",
                string(date_time),
            )?;
            field(
                environment,
                "environment",
                "last_snapshot_at",
                string(date_time),
            )?;
            if let Some(setup) = environment.get("setup") {
                setup_object(setup, &top["client_ts"])?;
            }
            if let Some(unsaved) = environment.get("unsaved") {
                let unsaved = object(unsaved, "unsaved")?;
                closed(
                    unsaved,
                    "unsaved",
                    &["uncommitted_repos", "unpushed_commits", "counted_at"],
                    &[],
                )?;
                field(unsaved, "unsaved", "uncommitted_repos", integer(0, 10_000))?;
                field(
                    unsaved,
                    "unsaved",
                    "unpushed_commits",
                    integer(0, 1_000_000),
                )?;
                field(unsaved, "unsaved", "counted_at", string(date_time))?;
            }
            if seen.contains(&&environment["env_id"]) {
                return Err(format!("{} is named twice", environment["env_id"]));
            }
            seen.push(&environment["env_id"]);
        }
        Ok(())
    }

    /// `EnvironmentSetup`: `additionalProperties: false` at both levels, and
    /// a run's times in the past of the heartbeat that carries them, its end
    /// not before its start.
    fn setup_object(value: &Value, client_ts: &Value) -> Check {
        let setup = object(value, "setup")?;
        closed(
            setup,
            "setup",
            &["network", "allowed_hosts", "repos", "variables", "secrets"],
            &["script"],
        )?;
        field(setup, "setup", "network", string(|v| NETWORKS.contains(&v)))?;
        field(setup, "setup", "allowed_hosts", integer(0, 10_000))?;
        field(setup, "setup", "variables", integer(0, 1_000))?;
        field(setup, "setup", "secrets", integer(0, 1_000))?;
        field(setup, "setup", "repos", |repos| {
            repos
                .as_array()
                .is_some_and(|list| list.len() <= 32 && list.iter().all(string(repo)))
        })?;
        let Some(script) = setup.get("script") else {
            return Ok(());
        };
        let script = object(script, "setup.script")?;
        let state = script.get("state").and_then(Value::as_str).unwrap_or("");
        let mut optional = Vec::new();
        if state != "running" {
            optional.push("finished_at");
        }
        if state == "failed" {
            optional.push("exit_code");
        }
        let required: &[&str] = match state {
            "running" => &["state", "started_at"],
            _ => &["state", "started_at", "finished_at"],
        };
        closed(script, "setup.script", required, &optional)?;
        field(
            script,
            "setup.script",
            "state",
            string(|v| SCRIPT_STATES.contains(&v)),
        )?;
        field(script, "setup.script", "started_at", string(date_time))?;
        field(script, "setup.script", "finished_at", string(date_time))?;
        field(script, "setup.script", "exit_code", integer(0, 255))?;
        // This program writes every time in one form, so the strings order as
        // the times do.
        let time = |key: &str| script.get(key).and_then(Value::as_str);
        let sent = client_ts.as_str().unwrap_or("");
        if [time("started_at"), time("finished_at")]
            .into_iter()
            .flatten()
            .any(|at| at > sent)
        {
            return Err("setup.script has a time ahead of client_ts".to_string());
        }
        if let (Some(started), Some(finished)) = (time("started_at"), time("finished_at")) {
            if finished < started {
                return Err("setup.script finished before it started".to_string());
            }
        }
        Ok(())
    }

    const TYPES: [&str; 10] = [
        "session_start",
        "user_prompt_submit",
        "activity",
        "permission_prompt",
        "input_prompt",
        "elicitation_prompt",
        "stop",
        "stop_failure",
        "session_end",
        "outcome",
    ];

    /// `EventEnvelope`. Every field is held to its shape whatever the `type`.
    fn envelope(value: &Value) -> Check {
        let event = object(value, "event")?;
        closed(
            event,
            "event",
            &["event_id", "env_id", "session_id", "type", "client_ts"],
            &[
                "bridge_session_id",
                "repos",
                "branch",
                "error_kind",
                "limit_resets_at",
                "commits",
                "uncommitted_repos",
                "unpushed_commits",
                "pull_requests",
            ],
        )?;
        let id = |c: char| c.is_ascii_alphanumeric() || "_-".contains(c);
        let session = |value: &str| chars(value, 1, 128, |c| word(c) || c == ':');
        let lower = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c);
        field(event, "event", "event_id", string(|v| chars(v, 8, 64, id)))?;
        field(event, "event", "env_id", string(env_key))?;
        field(event, "event", "session_id", string(session))?;
        field(event, "event", "bridge_session_id", string(session))?;
        field(event, "event", "type", string(|v| TYPES.contains(&v)))?;
        field(event, "event", "repos", |repos| {
            repos
                .as_array()
                .is_some_and(|list| list.len() <= 32 && list.iter().all(string(repo)))
        })?;
        field(
            event,
            "event",
            "branch",
            string(|v| chars(v, 1, 255, |c| word(c) || "/@+=#".contains(c))),
        )?;
        field(
            event,
            "event",
            "error_kind",
            string(|v| chars(v, 1, 64, lower)),
        )?;
        field(event, "event", "limit_resets_at", string(date_time))?;
        field(event, "event", "commits", integer(0, 100_000))?;
        field(event, "event", "uncommitted_repos", integer(0, 32))?;
        field(event, "event", "unpushed_commits", integer(0, 100_000))?;
        field(event, "event", "client_ts", string(date_time))?;
        if let Some(pulls) = event.get("pull_requests") {
            let pulls = pulls
                .as_array()
                .filter(|list| list.len() <= 16)
                .ok_or("event.pull_requests is not a list of at most 16")?;
            let repos: Vec<String> = event
                .get("repos")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_lowercase)
                        .collect()
                })
                .unwrap_or_default();
            for pull in pulls {
                let named = pull
                    .as_str()
                    .and_then(pull_request)
                    .ok_or(format!("event.pull_requests does not fit: {pull}"))?;
                if !repos.contains(&named.to_lowercase()) {
                    return Err(format!("{pull} is on a repo the event does not name"));
                }
            }
        }
        Ok(())
    }

    /// `EventsRequest`: one to a hundred envelopes.
    pub fn events(body: &Value) -> Check {
        let top = object(body, "events request")?;
        closed(top, "events request", &["events"], &[])?;
        let events = top["events"]
            .as_array()
            .filter(|list| (1..=100).contains(&list.len()))
            .ok_or("events is not a list of 1 to 100")?;
        events.iter().try_for_each(envelope)
    }

    /// What the contract, as 0.5.0 has it, has against a request to an
    /// ingest route, if anything.
    pub fn violation(request: &Request) -> Option<String> {
        violation_taking(request, &[])
    }

    /// As [`violation`], for a Watch that also takes the `ahead` fields.
    pub fn violation_taking(request: &Request, ahead: &[&str]) -> Option<String> {
        let limit = match request.path.as_str() {
            HEARTBEATS => 32 * 1024,
            EVENTS => 256 * 1000,
            _ => return None,
        };
        if request.body.len() > limit {
            return Some(format!("{} is over its {limit} byte limit", request.path));
        }
        match serde_json::from_str::<Value>(&request.body) {
            Ok(body) if request.path == HEARTBEATS => heartbeat_taking(&body, ahead).err(),
            Ok(body) => events(&body).err(),
            Err(_) => Some(format!("{} was not sent JSON", request.path)),
        }
    }

    /// The API's answer to a request the contract refuses.
    pub fn refusal(request: &Request) -> Option<Reply> {
        violation(request).map(|_| Reply::problem(400, "validation_failed"))
    }

    /// Everything the mock was sent that the Watch it stands in for would
    /// have refused: Watch as it is, unless the mock says it takes more.
    pub fn violations(mock: &Mock) -> Vec<String> {
        mock.requests()
            .iter()
            .filter_map(|request| violation_taking(request, mock.ahead))
            .collect()
    }
}
