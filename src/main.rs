use anyhow::Result;
use clap::{Parser, Subcommand};
use nucleate::workspace::Workspace;
use nucleate::{config, login, mcp};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Multi-repo session workspaces for coding agents.
#[derive(Parser)]
#[command(name = "nucleate", version, about)]
struct Cli {
    /// Workspace root (the folder holding .repos and .worktrees).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a nucleus under --root (default: the current folder).
    Init,
    /// Make a repo available: a bare clone that fetches into remote-tracking refs.
    Clone {
        url: String,
        /// The repo's name in the nucleus; taken from the URL unless given.
        #[arg(long)]
        name: Option<String>,
    },
    /// List the repos that can be added to a session.
    Available,
    /// Fetch every bare repo and drop branches deleted on the remote.
    Refresh,
    /// Create an empty session, optionally adding repos straight away.
    CreateSession { name: String, repos: Vec<String> },
    /// Add a repo to a session as a git worktree.
    Add {
        repo: String,
        branch: Option<String>,
        /// Session name or path; defaults to the session the current folder is in.
        #[arg(long)]
        session: Option<String>,
    },
    /// Show a session's repos and what is unsaved in each.
    Status {
        #[arg(long)]
        session: Option<String>,
    },
    /// Remove one repo from a session.
    Remove {
        repo: String,
        /// Remove even if it holds uncommitted or unpushed work.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        session: Option<String>,
    },
    /// Remove a whole session; refuses if any repo in it holds unsaved work.
    RemoveSession { session: Option<String> },
    /// Claude Code WorktreeCreate hook (reads the event on stdin).
    HookCreate { repos: Vec<String> },
    /// Claude Code WorktreeRemove hook (reads the event on stdin).
    HookRemove,
    /// Run the MCP server on stdio.
    Mcp,
    /// Sign this box in to Nucarium Watch.
    Login {
        /// The Watch API to sign in to.
        #[arg(long, value_name = "URL")]
        api_url: Option<String>,
        /// The machine name to suggest; you can change it when you approve.
        #[arg(long)]
        name: Option<String>,
        /// Store a token read from standard input instead of signing in.
        #[arg(long)]
        token_stdin: bool,
    },
    /// Revoke this box's Watch token and delete it.
    Logout,
    /// Show the machine and account this box reports as, and whether its token works.
    Whoami,
}

fn print(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print a command's JSON and turn its exit code into the process's.
fn finish(output: login::Output) -> Result<u8> {
    print(&output.json)?;
    Ok(output.code)
}

/// The commands that need no nucleus.
fn run_without_workspace(cli: &Cli) -> Option<Result<u8>> {
    let mut ui = std::io::stderr();
    Some(match &cli.command {
        Command::Init => config::init(cli.root.clone()).and_then(|out| print(&out).map(|()| 0)),
        Command::Login {
            api_url,
            name,
            token_stdin,
        } => {
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            // Ctrl-C ends the wait cleanly: a line saying nothing was stored,
            // rather than a prompt left hanging mid-poll.
            let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
            let options = login::LoginOptions {
                api_url: api_url.clone(),
                name: name.clone(),
                token_stdin: *token_stdin,
            };
            login::login(&options, &mut ui, &cancel).and_then(finish)
        }
        Command::Logout => login::logout(&mut ui).and_then(finish),
        Command::Whoami => login::whoami(&mut ui).and_then(finish),
        _ => return None,
    })
}

fn run(cli: Cli) -> Result<u8> {
    if let Some(result) = run_without_workspace(&cli) {
        return result;
    }
    let workspace = Workspace::new(config::load(cli.root)?);
    let session = |given: &Option<String>| -> Result<PathBuf> {
        match given {
            Some(value) => workspace.session_dir(value),
            None => workspace.find_session_root(&std::env::current_dir()?),
        }
    };
    match &cli.command {
        Command::Init | Command::Login { .. } | Command::Logout | Command::Whoami => {
            unreachable!("handled before the workspace is loaded")
        }
        Command::Clone { url, name } => print(&serde_json::to_value(
            workspace.clone(url, name.as_deref())?,
        )?)?,
        Command::Available => print(&serde_json::to_value(workspace.available()?)?)?,
        Command::Refresh => {
            let results = workspace.refresh()?;
            print(&results)?;
            let all_ok = results
                .as_array()
                .is_some_and(|items| items.iter().all(|item| item["ok"] == true));
            return Ok(u8::from(!all_ok));
        }
        Command::CreateSession { name, repos } => {
            let path = workspace.create_session(name, repos)?;
            print(&serde_json::json!({ "session": name, "path": path }))?;
        }
        Command::Add {
            repo,
            branch,
            session: given,
        } => {
            print(&workspace.add_repo(&session(given)?, repo, branch.as_deref())?)?;
        }
        Command::Status { session: given } => print(&workspace.status(&session(given)?)?)?,
        Command::Remove {
            repo,
            force,
            session: given,
        } => {
            print(&workspace.remove_repo(&session(given)?, repo, *force)?)?;
        }
        Command::RemoveSession { session: given } => {
            print(&workspace.remove_session(&session(given)?)?)?;
        }
        Command::HookCreate { repos } => println!("{}", workspace.hook_create(repos)?.display()),
        Command::HookRemove => workspace.hook_remove()?,
        Command::Mcp => mcp::serve(&workspace)?,
    }
    Ok(0)
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("nucleate: {error:#}");
            ExitCode::FAILURE
        }
    }
}
