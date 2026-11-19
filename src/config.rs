//! Where the workspace lives and how to reach GitHub.
//!
//! The workspace root is found, in order, from `--root`, `NUCLEATE_ROOT`, the
//! config file, or by walking up from the current directory to a folder that
//! contains `.repos`.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub github_token_file: Option<PathBuf>,
    pub branch_prefix: String,
    /// The GitHub API to ask. GitHub's own, unless a stand-in on this machine
    /// is named for a test.
    pub github_api: String,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    root: Option<PathBuf>,
    github_token_file: Option<PathBuf>,
    branch_prefix: Option<String>,
}

/// The config file: `NUCLEATE_CONFIG`, or `~/.config/nucleate/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = env::var_os("NUCLEATE_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("nucleate").join("config.toml"))
}

fn read_file_config() -> Result<FileConfig> {
    let Some(path) = config_path() else {
        return Ok(FileConfig::default());
    };
    if !path.is_file() {
        return Ok(FileConfig::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn discover_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".repos").is_dir())
        .map(Path::to_path_buf)
}

pub fn load(cli_root: Option<PathBuf>) -> Result<Config> {
    let file = read_file_config()?;
    let cwd = env::current_dir().context("reading the current directory")?;
    let root = cli_root
        .or_else(|| env::var_os("NUCLEATE_ROOT").map(PathBuf::from))
        .or(file.root)
        .or_else(|| discover_root(&cwd))
        .ok_or_else(|| {
            anyhow!(
                "no workspace root: pass --root, set NUCLEATE_ROOT, set `root` in the config \
                 file, or run inside a folder that contains .repos"
            )
        })?;
    if !root.is_dir() {
        bail!("workspace root is not a directory: {}", root.display());
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let github_token_file = env::var_os("NUCLEATE_GITHUB_TOKEN_FILE")
        .map(PathBuf::from)
        .or(file.github_token_file);
    Ok(Config {
        root,
        github_token_file,
        branch_prefix: file
            .branch_prefix
            .unwrap_or_else(|| "workspace".to_string()),
        github_api: crate::github::api_url(),
    })
}

/// The settings for one nucleus named outright, as the daemon names each
/// environment's. Anything not given falls back to the config file.
pub fn for_root(
    root: &Path,
    github_token_file: Option<PathBuf>,
    branch_prefix: Option<String>,
) -> Config {
    let file = read_file_config().unwrap_or_default();
    Config {
        root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
        github_token_file: github_token_file
            .or_else(|| env::var_os("NUCLEATE_GITHUB_TOKEN_FILE").map(PathBuf::from))
            .or(file.github_token_file),
        branch_prefix: branch_prefix
            .or(file.branch_prefix)
            .unwrap_or_else(|| "workspace".to_string()),
        github_api: crate::github::api_url(),
    }
}

/// `nucleate init`: create a nucleus under `root` (default: the current
/// folder) and name it in the config file if the file names none yet.
/// Running it again changes nothing.
pub fn init(cli_root: Option<PathBuf>) -> Result<serde_json::Value> {
    let root = match cli_root {
        Some(root) => root,
        None => env::current_dir().context("reading the current directory")?,
    };
    let mut created = false;
    for folder in [".repos", ".worktrees", ".seeds"] {
        let path = root.join(folder);
        if !path.is_dir() {
            fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
            created = true;
        }
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let path = config_path().ok_or_else(|| anyhow!("no config folder: HOME is not set"))?;
    // A root already named in the file is left alone: that nucleus stays the
    // default, and this one is reached with --root or from inside it.
    if read_file_config()?.root.is_none() {
        let line = format!(
            "root = {}\n",
            toml::Value::String(root.to_string_lossy().to_string())
        );
        let rest = fs::read_to_string(&path).unwrap_or_default();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        // Top-level keys come before any table, so the line goes first.
        fs::write(&path, line + &rest).with_context(|| format!("writing {}", path.display()))?;
        created = true;
    }
    Ok(serde_json::json!({ "root": root, "config": path, "created": created }))
}
