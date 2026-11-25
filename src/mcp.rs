//! A small Model Context Protocol server over stdio.
//!
//! It exposes the session commands as tools so an agent can expand its own
//! session. The server is started with the session folder as its working
//! directory and finds the session from there on every call.

use crate::workspace::Workspace;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn tools() -> Value {
    json!([
        {
            "name": "list_repos",
            "description": "List the repos already on this box and those in this session. Any other GitHub repo the box can read can be added by owner/name.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "add_repo",
            "description": "Add a repo to this session as a git worktree. A GitHub repo not yet on this box is fetched first when named owner/name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Name like 'demo' for a repo already on this box, or owner/name like 'octo/demo' for any GitHub repo." },
                    "branch": { "type": "string", "description": "Optional; defaults to the session branch." }
                },
                "required": ["repo"]
            }
        },
        {
            "name": "remove_repo",
            "description": "Remove a repo worktree from this session. Refuses if it holds uncommitted or unpushed work unless force is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string" },
                    "force": { "type": "boolean" }
                },
                "required": ["repo"]
            }
        },
        {
            "name": "workspace_status",
            "description": "Status of every repo materialised in this session.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

fn call(workspace: &Workspace, name: &str, args: &Value) -> Result<Value> {
    let session = workspace.find_session_root(&std::env::current_dir()?)?;
    let repo = || {
        args["repo"]
            .as_str()
            .ok_or_else(|| anyhow!("missing required argument: repo"))
    };
    match name {
        "list_repos" | "workspace_status" => workspace.status(&session),
        "add_repo" => workspace.add_repo(&session, repo()?, args["branch"].as_str()),
        "remove_repo" => {
            workspace.remove_repo(&session, repo()?, args["force"].as_bool().unwrap_or(false))
        }
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn handle(workspace: &Workspace, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "nucleate", "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let (text, is_error) = match call(workspace, name, &params["arguments"]) {
                Ok(value) => (
                    serde_json::to_string_pretty(&value).unwrap_or_default(),
                    false,
                ),
                Err(error) => (format!("{error:#}"), true),
            };
            Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

pub fn serve(workspace: &Workspace) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Messages without an id are notifications: nothing to answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message["method"].as_str().unwrap_or_default();
        let reply = match handle(workspace, method, &message["params"]) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, text)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": text } })
            }
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
}
