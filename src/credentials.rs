//! The Watch credential on the box.
//!
//! Two files, both mode 600, side by side:
//!
//! - `token`: the machine token and nothing else;
//! - `login.json`: no secret: which API issued the token, and the machine and
//!   account it belongs to.
//!
//! `nucleate login` writes them; `nucleated` reads them.

use crate::paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// A machine token. It has no `Display`, and `Debug` hides it, so it cannot be
/// printed or logged by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Accept a token only if it looks like one: printable, unbroken, and of a
    /// sensible length. This keeps a stray newline or a pasted sentence out of
    /// an `Authorization` header.
    pub fn parse(raw: &str) -> Result<Token> {
        let value = raw.trim();
        let allowed = |c: u8| {
            c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'~' | b'+' | b'/' | b'=')
        };
        if !(16..=512).contains(&value.len()) || !value.bytes().all(allowed) {
            bail!("that does not look like a machine token");
        }
        Ok(Token(value.to_string()))
    }

    /// The secret itself, for an `Authorization` header or a form field.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(redacted)")
    }
}

/// What `login.json` holds. Nothing in it is secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    pub api_url: String,
    #[serde(default)]
    pub machine_id: Option<String>,
    #[serde(default)]
    pub machine_name: Option<String>,
    #[serde(default)]
    pub account_name: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub logged_in_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub token: Token,
    /// Missing when the token file was put in place by hand.
    pub login: Option<Login>,
}

#[derive(Debug)]
pub enum Stored {
    /// The box is not signed in.
    Missing,
    /// The token file can be read by its group or by others, so it is not used.
    Insecure {
        mode: u32,
    },
    Present(Credential),
}

/// `login.json` lives beside the token file.
pub fn login_file(token_file: &Path) -> PathBuf {
    token_file.with_file_name("login.json")
}

fn read_login(token_file: &Path) -> Option<Login> {
    let text = fs::read_to_string(login_file(token_file)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read the credential, refusing a token file that others can read.
pub fn read(token_file: &Path) -> Result<Stored> {
    if !token_file.is_file() {
        return Ok(Stored::Missing);
    }
    if let Some(mode) = paths::mode(token_file) {
        if mode & 0o077 != 0 {
            return Ok(Stored::Insecure { mode });
        }
    }
    read_any(token_file)
}

/// Read the credential whatever its permissions. Only for removing it.
pub fn read_any(token_file: &Path) -> Result<Stored> {
    if !token_file.is_file() {
        return Ok(Stored::Missing);
    }
    let text = fs::read_to_string(token_file)
        .with_context(|| format!("reading {}", token_file.display()))?;
    let token = Token::parse(&text)
        .with_context(|| format!("{} does not hold a machine token", token_file.display()))?;
    Ok(Stored::Present(Credential {
        token,
        login: read_login(token_file),
    }))
}

/// Store the token and what is known about it. The token is written first:
/// a box with a token and stale details can still report.
pub fn store(token_file: &Path, token: &Token, login: &Login) -> Result<()> {
    paths::write_private(token_file, format!("{}\n", token.expose()).as_bytes())?;
    let text = serde_json::to_string_pretty(login)? + "\n";
    paths::write_private(&login_file(token_file), text.as_bytes())
}

/// Delete both files. True if there was a token to delete.
pub fn remove(token_file: &Path) -> Result<bool> {
    let existed = token_file.exists();
    for path in [token_file.to_path_buf(), login_file(token_file)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("deleting {}", path.display()))
            }
        }
    }
    Ok(existed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_never_shown_by_debug() {
        let token = Token::parse("nuc_4m7q1x8z5b6n2f9k_3Yt0v1c2").unwrap();
        assert!(!format!("{token:?}").contains("4m7q"));
        assert!(!format!("{:?}", Credential { token, login: None }).contains("4m7q"));
    }

    #[test]
    fn only_token_shaped_text_is_accepted() {
        assert!(Token::parse("  nuc_4m7q1x8z5b6n2f9k_3Yt0v1c2\n").is_ok());
        assert!(Token::parse("").is_err());
        assert!(Token::parse("short").is_err());
        assert!(Token::parse("nuc_4m7q1x8z5b6n2f9k with spaces").is_err());
        assert!(Token::parse("nuc_4m7q1x8z5b6n2f9k\nsecond-line").is_err());
    }
}
