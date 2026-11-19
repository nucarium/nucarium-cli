//! Thin wrappers around the `git` command line.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run git and return its result whether or not it succeeded.
pub fn try_git<I, S>(args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new("git")
        .args(args)
        .output()
        .context("running git (is it installed?)")?;
    Ok(Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Run git and fail with its error output if it did not succeed.
pub fn git<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let out = try_git(&args)?;
    if !out.ok {
        let shown: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
        bail!("git {} failed: {}", shown.join(" "), out.stderr);
    }
    Ok(out.stdout)
}

fn with_prefix(flag: &str, dir: &Path, args: &[&str]) -> Vec<std::ffi::OsString> {
    let mut all = vec![flag.into(), dir.as_os_str().to_os_string()];
    all.extend(args.iter().map(|a| a.into()));
    all
}

/// `git --git-dir <bare> ...`
pub fn bare(dir: &Path, args: &[&str]) -> Result<String> {
    git(with_prefix("--git-dir", dir, args))
}

pub fn try_bare(dir: &Path, args: &[&str]) -> Result<Output> {
    try_git(with_prefix("--git-dir", dir, args))
}

/// `git -C <worktree> ...`
pub fn tree(dir: &Path, args: &[&str]) -> Result<String> {
    git(with_prefix("-C", dir, args))
}

pub fn try_tree(dir: &Path, args: &[&str]) -> Result<Output> {
    try_git(with_prefix("-C", dir, args))
}

/// Run git in a worktree with extra environment variables and return its
/// output untrimmed, for output where leading spaces and NULs matter.
pub fn tree_raw(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &OsStr)],
) -> Result<(bool, Vec<u8>, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .context("running git (is it installed?)")?;
    Ok((
        out.status.success(),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}
