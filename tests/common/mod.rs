//! A local stand-in for the Watch API, and helpers for driving the binaries
//! in a home folder of their own.
#![allow(dead_code)]

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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        command
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
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
            "machine = \"agentbox\"\nstate_dir = \"{state}\"\n{top}\n\
             [[environment]]\nname = \"client-a\"\nroot = \"{root}\"\n{environment}\n\n{tables}\n",
            state = self.home.state_dir().display(),
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
