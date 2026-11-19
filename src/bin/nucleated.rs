use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use nucleate::envelope::rfc3339;
use nucleate::settings::{self, Settings};
use nucleate::{agent, config, daemon, hook, housekeeping, pause, status};
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
    /// Print the hooks block to merge into that login's settings.json.
    Print {
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
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
        Command::Hook { .. } => unreachable!("handled before anything can fail"),
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
            command: HooksCommand::Print { env },
        } => {
            let settings = settings::load()?;
            let agent_name = match &env {
                Some(name) => settings
                    .environment(name)
                    .map(|environment| environment.agent.clone())
                    .ok_or_else(|| {
                        anyhow!("no environment named {name} in {}", settings.path.display())
                    })?,
                None => "claude-code".to_string(),
            };
            let agent = agent::by_name(&agent_name)
                .ok_or_else(|| anyhow!("no such agent: {agent_name}"))?;
            print(&agent.hooks_block(env.as_deref()))?;
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
    if let Command::Hook { event, env } = &cli.command {
        hook::run(event, env.as_deref());
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
