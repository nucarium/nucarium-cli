use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use nucleate::driver::inside;
use nucleate::envelope::rfc3339;
use nucleate::settings::{self, Settings};
use nucleate::{agent, config, config_sync, daemon, hook, housekeeping, pause, status};
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The box daemon: heartbeat, session events, housekeeping, local status.
#[derive(Parser)]
#[command(name = "nucleated", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground. This is what the systemd unit starts.
    Run,
    /// The hook entry point: reads an agent hook's JSON on standard input.
    Hook {
        /// The agent's own event name, such as SessionStart or Notification.
        event: String,
        /// The environment this hook belongs to.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// The agent whose hook this is. Default: the environment's agent,
        /// else claude-code.
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
    },
    /// One look at an environment's driver, from inside its container: what
    /// the daemon runs there through `podman exec`. Prints one answer.
    #[command(hide = true)]
    Look {
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// How long the look may take, in milliseconds.
        #[arg(long, value_name = "MS")]
        within: Option<u64>,
        /// The daemon's clock, in Unix milliseconds.
        #[arg(long, value_name = "MS")]
        now: Option<u64>,
        /// Tells this look from another, for `--end`.
        #[arg(long)]
        token: Option<String>,
        /// What the last look left, as the daemon keeps it.
        #[arg(long, value_name = "JSON")]
        previous: Option<String>,
        /// End the look started with this token, and what it started.
        #[arg(long, value_name = "TOKEN")]
        end: Option<String>,
    },
    /// Print what Watch would show, read from this box.
    Status {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Redraw every five seconds.
        #[arg(long)]
        watch: bool,
    },
    /// Check the config, the token file and each environment; when signed in, send one heartbeat.
    Check,
    /// Write a first config file with one environment.
    Init {
        /// The environment's name. Default: the nucleus folder's name.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// The environment's nucleus. Default: nucleate's root, else the current folder.
        #[arg(long, value_name = "PATH")]
        root: Option<PathBuf>,
        /// The systemd user unit that runs the agent server.
        #[arg(long)]
        unit: Option<String>,
        /// That login's agent config folder.
        #[arg(long, value_name = "DIR")]
        config_dir: Option<PathBuf>,
    },
    /// Hook registration for an agent's settings.
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    /// Housekeeping jobs.
    Housekeeping {
        #[command(subcommand)]
        command: HousekeepingCommand,
    },
    /// Agent config kept current from a repository of your own.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Print the systemd user unit.
    Unit,
    /// Announce maintenance: until the time is up, Watch holds its alerts for
    /// this box and its agent servers going dark. It touches no agent.
    Pause {
        /// How long, at most a day: "30m", "2h", "24h".
        duration: String,
    },
    /// End a maintenance pause now.
    Resume,
}

#[derive(Subcommand)]
enum HooksCommand {
    /// Print the hooks block to merge into that login's agent settings.
    Print {
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// The agent to print for. Default: the environment's agent, else
        /// claude-code.
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Sync now: fetch the pinned branch and apply it to each environment's
    /// config folder.
    Sync {
        /// Only this environment. Default: all of them.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// Print what would change, and change nothing. Nothing is applied to
        /// an environment for the first time until this has been run for it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum HousekeepingCommand {
    /// Run one job now: refresh, cleanup or snapshot.
    Run {
        job: String,
        /// Only this environment. Default: all of them.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// Report what cleanup or snapshot would do, and do nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

const UNIT: &str = include_str!("../../contrib/systemd/nucleated.service");

fn print(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn stop_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    stop
}

/// A name Watch accepts, made from a folder name.
fn env_name_from(root: &std::path::Path) -> String {
    let folder = root
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name: String = folder
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name
        .trim_start_matches(['.', '_', '-'])
        .chars()
        .take(64)
        .collect::<String>();
    if name.is_empty() {
        "default".to_string()
    } else {
        name
    }
}

fn init(
    env: Option<String>,
    root: Option<PathBuf>,
    unit: Option<String>,
    config_dir: Option<PathBuf>,
) -> Result<serde_json::Value> {
    let path = settings::config_path()?;
    if path.exists() {
        return Ok(serde_json::json!({ "config": path, "created": false }));
    }
    let root = match root {
        Some(root) => root,
        None => match config::load(None) {
            Ok(config) => config.root,
            Err(_) => std::env::current_dir().context("reading the current directory")?,
        },
    };
    let root = root.canonicalize().unwrap_or(root);
    let name = env.unwrap_or_else(|| env_name_from(&root));
    let quoted = |text: &str| toml::Value::String(text.to_string()).to_string();
    let mut text = String::new();
    if let Some(machine) = settings::hostname() {
        text.push_str(&format!("machine = {}\n\n", quoted(&machine)));
    }
    text.push_str(&format!(
        "[[environment]]\nname = {}\nroot = {}\n",
        quoted(&name),
        quoted(&root.to_string_lossy())
    ));
    if let Some(unit) = unit {
        text.push_str(&format!("unit = {}\n", quoted(&unit)));
    }
    if let Some(config_dir) = config_dir {
        text.push_str(&format!(
            "config_dir = {}\n",
            quoted(&config_dir.to_string_lossy())
        ));
    }
    // Refuse to write a file the daemon could not read back.
    let parsed: toml::Table = toml::from_str(&text)?;
    drop(parsed);
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no folder", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
    settings::load_from(&path)?;
    Ok(serde_json::json!({ "config": path, "created": true }))
}

fn show_status(settings: &Settings, json: bool) -> Result<()> {
    let view = status::view(settings);
    if json {
        print(&view)
    } else {
        print!("{}", status::render(&view));
        Ok(())
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Hook { .. } | Command::Look { .. } => {
            unreachable!("handled before anything can fail")
        }
        Command::Run => daemon::run(stop_flag())?,
        Command::Status { json, watch } => {
            if !watch {
                show_status(&settings::load()?, json)?;
                return Ok(0);
            }
            let stop = stop_flag();
            while !stop.load(Ordering::SeqCst) {
                // Clear the screen and start again from the top.
                print!("\x1b[2J\x1b[H");
                show_status(&settings::load()?, json)?;
                for _ in 0..50 {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Command::Check => {
            let (report, ok) = match settings::load() {
                Ok(settings) => status::check(&settings),
                Err(error) => (
                    serde_json::json!({
                        "ok": false,
                        "checks": [{ "name": "config", "ok": false, "detail": format!("{error:#}") }],
                    }),
                    false,
                ),
            };
            print(&report)?;
            return Ok(u8::from(!ok));
        }
        Command::Init {
            env,
            root,
            unit,
            config_dir,
        } => print(&init(env, root, unit, config_dir)?)?,
        Command::Hooks {
            command: HooksCommand::Print { env, agent },
        } => {
            let settings = settings::load()?;
            let of_environment = match &env {
                Some(name) => Some(
                    settings
                        .environment(name)
                        .map(|environment| environment.agent.clone())
                        .ok_or_else(|| {
                            anyhow!("no environment named {name} in {}", settings.path.display())
                        })?,
                ),
                None => None,
            };
            let agent_name = agent
                .or(of_environment)
                .unwrap_or_else(|| agent::DEFAULT.to_string());
            let agent = agent::by_name(&agent_name).ok_or_else(|| {
                anyhow!(
                    "no such agent: {agent_name}; the agents are {}",
                    agent::NAMES.join(", ")
                )
            })?;
            print(&agent.hooks_block(env.as_deref()))?;
        }
        Command::Config {
            command: ConfigCommand::Sync { env, dry_run },
        } => {
            let settings = settings::load()?;
            let (report, ok) = config_sync::run_now(&settings, env.as_deref(), dry_run)?;
            print(&report)?;
            return Ok(u8::from(!ok));
        }
        Command::Housekeeping {
            command: HousekeepingCommand::Run { job, env, dry_run },
        } => {
            let settings = settings::load()?;
            let (report, ok) = housekeeping::run_now(&settings, &job, env.as_deref(), dry_run)?;
            print(&report)?;
            return Ok(u8::from(!ok));
        }
        Command::Unit => print!("{UNIT}"),
        Command::Pause { duration } => {
            let settings = settings::load()?;
            let now = SystemTime::now();
            let until = settings::parse_duration(&duration)
                .and_then(|length| pause::set(&settings.state_dir, length, now));
            match until {
                // The running daemon sees the file and says so to Watch at once.
                Ok(until) => print(&serde_json::json!({
                    "paused": true,
                    "until": rfc3339(until),
                }))?,
                // Nothing was paused, and the command line is why.
                Err(error) => {
                    eprintln!("nucleated: {error:#}");
                    return Ok(2);
                }
            }
        }
        Command::Resume => {
            let settings = settings::load()?;
            let was_paused = pause::until(&settings.state_dir, SystemTime::now()).is_some();
            pause::clear(&settings.state_dir)?;
            print(&serde_json::json!({ "paused": false, "was_paused": was_paused }))?;
        }
    }
    Ok(0)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The hook entry point comes first and stands apart: whatever happens in
    // it, the agent's session sees success.
    if let Command::Hook { event, env, agent } = &cli.command {
        hook::run(event, env.as_deref(), agent.as_deref());
        return ExitCode::SUCCESS;
    }
    // So does a look: it always prints one answer and leaves with success.
    if let Command::Look {
        env,
        within,
        now,
        token: _,
        previous,
        end,
    } = &cli.command
    {
        match end {
            Some(token) => inside::end(token),
            None => inside::run(&inside::Asked {
                env: env.as_deref(),
                within_ms: *within,
                now_ms: *now,
                previous: previous.as_deref(),
            }),
        }
        return ExitCode::SUCCESS;
    }
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("nucleated: {error:#}");
            ExitCode::FAILURE
        }
    }
}
