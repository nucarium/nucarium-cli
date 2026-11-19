//! `nucleate login`, `logout` and `whoami`: signing a box in to Watch.
//!
//! A box has no browser, so sign-in is the OAuth 2.0 device authorization
//! grant (RFC 8628): the box shows a short code, the owner approves it on
//! another device, and the box collects a machine token. The token is stored
//! and never printed.

use crate::credentials::{self, Login, Stored, Token};
use crate::envelope::rfc3339;
use crate::settings::{self, Settings};
use crate::watch::{
    printable, ApiError, Client, DeviceAuthorization, Grant, TokenPoll, TokenStatus,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

pub const EXIT_OK: u8 = 0;
pub const EXIT_DENIED: u8 = 3;
pub const EXIT_EXPIRED: u8 = 4;
pub const EXIT_NOT_LOGGED_IN: u8 = 5;
pub const EXIT_TOKEN_REJECTED: u8 = 6;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_INTERRUPTED: u8 = 130;

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// RFC 8628 §3.5: the interval to assume when the server names none, and the
/// amount to add each time it says `slow_down`.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const MAX_UNREACHABLE_WAIT: Duration = Duration::from_secs(60);

/// What a command prints on standard output, and how it exits.
#[derive(Debug)]
pub struct Output {
    pub json: Value,
    pub code: u8,
}

/// Time, so the sign-in can be tested without waiting for it.
pub trait Clock {
    fn now(&self) -> Instant;
    /// Wait. May return early when the user cancels.
    fn sleep(&self, duration: Duration);
    fn cancelled(&self) -> bool;
}

/// The real clock, which stops waiting as soon as `cancel` is set.
pub struct SystemClock<'a> {
    pub cancel: &'a AtomicBool,
}

impl Clock for SystemClock<'_> {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        let until = Instant::now() + duration;
        while !self.cancelled() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// How a sign-in ended.
#[derive(Debug)]
pub enum Flow {
    Granted(Grant),
    Denied,
    Expired,
    Cancelled,
}

/// Run the device flow: ask for a code, show it, and poll until the owner
/// approves or denies it, it expires, or the user cancels.
///
/// An API that cannot be reached at the start is an error. One that goes quiet
/// during the wait is polled less and less often until the code expires.
pub fn device_flow(
    client: &Client,
    machine_name: Option<&str>,
    ui: &mut dyn Write,
    clock: &dyn Clock,
) -> Result<Flow, ApiError> {
    let authorization = client.device_authorization(machine_name)?;
    let _ = show_code(ui, &authorization);
    let deadline = clock.now() + Duration::from_secs(authorization.expires_in);
    let mut interval = authorization
        .interval
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_INTERVAL)
        .max(Duration::from_secs(1));
    let mut wait = interval;
    let mut last_failure = None;
    loop {
        let left = deadline.saturating_duration_since(clock.now());
        clock.sleep(wait.min(left));
        if clock.cancelled() {
            return Ok(Flow::Cancelled);
        }
        if clock.now() >= deadline {
            return match last_failure {
                Some(failure) => Err(failure),
                None => Ok(Flow::Expired),
            };
        }
        match client.token(&authorization.device_code) {
            Ok(TokenPoll::Granted(grant)) => return Ok(Flow::Granted(grant)),
            Ok(TokenPoll::Denied) => return Ok(Flow::Denied),
            Ok(TokenPoll::Expired) => return Ok(Flow::Expired),
            Ok(TokenPoll::Pending) => {
                last_failure = None;
                wait = interval;
            }
            Ok(TokenPoll::SlowDown) => {
                last_failure = None;
                interval += SLOW_DOWN_STEP;
                wait = interval;
            }
            Err(ApiError::RateLimited { retry_after }) => {
                interval += SLOW_DOWN_STEP;
                wait = interval.max(retry_after.unwrap_or_default());
            }
            Err(failure) if failure.is_transient() => {
                last_failure = Some(failure);
                wait = (wait * 2).clamp(interval, MAX_UNREACHABLE_WAIT.max(interval));
            }
            Err(failure) => return Err(failure),
        }
    }
}

/// Five-row letters, so the code can be read across a room. One string per
/// glyph: five rows of five cells, `#` for ink.
const GLYPHS: &[(char, &str)] = &[
    ('A', " ### #   #######   ##   #"),
    ('B', "#### #   ##### #   ##### "),
    ('C', " #####    #    #     ####"),
    ('D', "#### #   ##   ##   ##### "),
    ('E', "######    #### #    #####"),
    ('F', "######    #### #    #    "),
    ('G', " #####    #  ###   # ####"),
    ('H', "#   ##   #######   ##   #"),
    ('I', "#####  #    #    #  #####"),
    ('J', "  ###    #    ##   # ### "),
    ('K', "#   ##  # ###  #  # #   #"),
    ('L', "#    #    #    #    #####"),
    ('M', "#   ### ### # ##   ##   #"),
    ('N', "#   ###  ## # ##  ###   #"),
    ('O', " ### #   ##   ##   # ### "),
    ('P', "#### #   ##### #    #    "),
    ('Q', " ### #   ## # ##  #  ## #"),
    ('R', "#### #   ##### #  # #   #"),
    ('S', " #####     ###     ##### "),
    ('T', "#####  #    #    #    #  "),
    ('U', "#   ##   ##   ##   # ### "),
    ('V', "#   ##   ##   # # #   #  "),
    ('W', "#   ##   ## # ### ###   #"),
    ('X', "#   # # #   #   # # #   #"),
    ('Y', "#   # # #   #    #    #  "),
    ('Z', "#####   #   #   #   #####"),
    ('0', " ### #  ### # ###  # ### "),
    ('1', "  #   ##    #    #   ### "),
    ('2', " ### #   #  ##  #   #####"),
    ('3', "####     # ###     ##### "),
    ('4', "#   ##   ######    #    #"),
    ('5', "######    ####     ##### "),
    ('6', " ### #    #### #   # ### "),
    ('7', "#####    #   #   #    #  "),
    ('8', " ### #   # ### #   # ### "),
    ('9', " ### #   # ####    # ### "),
    ('-', "          #####          "),
];

/// The code in five-row letters, or nothing if it holds a character there is
/// no glyph for or would not fit on an 80-column terminal.
pub fn large(code: &str, ink: char) -> Option<Vec<String>> {
    let glyphs: Vec<&str> = code
        .chars()
        .map(|c| {
            GLYPHS
                .iter()
                .find(|(known, _)| *known == c.to_ascii_uppercase())
                .map(|(_, glyph)| *glyph)
        })
        .collect::<Option<_>>()?;
    if glyphs.is_empty() || glyphs.len() > 12 {
        return None;
    }
    Some(
        (0..5)
            .map(|row| {
                glyphs
                    .iter()
                    .map(|glyph| {
                        glyph[row * 5..row * 5 + 5]
                            .chars()
                            .map(|cell| if cell == '#' { ink } else { ' ' })
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim_end()
                    .to_string()
            })
            .collect(),
    )
}

fn utf8_terminal() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
        .is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        })
}

fn minutes(seconds: u64) -> String {
    match seconds.div_ceil(60) {
        1 => "1 minute".to_string(),
        n => format!("{n} minutes"),
    }
}

/// Show the code and where to enter it. The reader is at a terminal on the
/// box and will type the code into a phone, so it is printed large.
pub fn show_code(ui: &mut dyn Write, authorization: &DeviceAuthorization) -> std::io::Result<()> {
    let code = printable(&authorization.user_code);
    let ink = if utf8_terminal() { '█' } else { '#' };
    writeln!(ui)?;
    writeln!(ui, "  Sign this box in to Nucarium Watch")?;
    writeln!(ui)?;
    writeln!(ui, "  On your phone or laptop, open")?;
    writeln!(ui)?;
    writeln!(ui, "      {}", printable(&authorization.verification_uri))?;
    writeln!(ui)?;
    writeln!(ui, "  and enter this code:")?;
    writeln!(ui)?;
    if let Some(rows) = large(&code, ink) {
        for row in rows {
            writeln!(ui, "      {row}")?;
        }
        writeln!(ui)?;
    }
    writeln!(ui, "      {code}")?;
    writeln!(ui)?;
    if let Some(complete) = &authorization.verification_uri_complete {
        writeln!(ui, "  Or open this link, which already carries the code:")?;
        writeln!(ui)?;
        writeln!(ui, "      {}", printable(complete))?;
        writeln!(ui)?;
    }
    writeln!(
        ui,
        "  Waiting for you to approve. The code lasts {}. Ctrl-C cancels.",
        minutes(authorization.expires_in)
    )?;
    writeln!(ui)?;
    ui.flush()
}

#[derive(Debug, Default)]
pub struct LoginOptions {
    pub api_url: Option<String>,
    pub name: Option<String>,
    pub token_stdin: bool,
}

fn suggested_name(options: &LoginOptions, settings: &Settings) -> Option<String> {
    let name = options
        .name
        .clone()
        .or_else(|| settings::machine_name(settings))?;
    let name: String = printable(name.trim()).chars().take(64).collect();
    (!name.is_empty()).then_some(name)
}

fn signed_in(
    token_file: &std::path::Path,
    login: &Login,
    replaced: bool,
    previous_revoked: Option<bool>,
) -> Value {
    let mut out = json!({
        "logged_in": true,
        "api_url": login.api_url,
        "machine_id": login.machine_id,
        "machine_name": login.machine_name,
        "account_name": login.account_name,
        "scope": login.scope,
        "token_file": token_file,
        "replaced": replaced,
    });
    if let Some(revoked) = previous_revoked {
        out["previous_revoked"] = json!(revoked);
    }
    out
}

/// `nucleate login`.
pub fn login(options: &LoginOptions, ui: &mut dyn Write, cancel: &AtomicBool) -> Result<Output> {
    let settings = settings::load()?;
    let api_url =
        crate::watch::normalize_api_url(&settings.login_api_url(options.api_url.as_deref()))?;
    let token_file = settings.token_file.clone();
    let previous = match credentials::read_any(&token_file) {
        Ok(Stored::Present(credential)) => Some(credential),
        _ => None,
    };

    if options.token_stdin {
        let mut raw = String::new();
        std::io::stdin()
            .take(4096)
            .read_to_string(&mut raw)
            .context("reading the token from standard input")?;
        let token = Token::parse(&raw)?;
        let login = Login {
            api_url,
            scope: Some(crate::watch::SCOPE.to_string()),
            logged_in_at: Some(rfc3339(SystemTime::now())),
            ..Login::default()
        };
        credentials::store(&token_file, &token, &login)?;
        let _ = writeln!(
            ui,
            "Token stored. This machine can now report to Watch. It can read nothing."
        );
        let replaced = previous.is_some_and(|old| old.token != token);
        return Ok(Output {
            json: signed_in(&token_file, &login, replaced, None),
            code: EXIT_OK,
        });
    }

    let client = Client::new(&api_url, HTTP_TIMEOUT)?;
    let clock = SystemClock { cancel };
    let name = suggested_name(options, &settings);
    let flow = device_flow(&client, name.as_deref(), ui, &clock)
        .map_err(|failure| anyhow!("{failure}. Nothing was stored."))?;
    let (reason, line, code) = match flow {
        Flow::Granted(grant) => {
            let login = Login {
                api_url: api_url.clone(),
                machine_id: grant.machine_id.clone(),
                machine_name: grant.machine_name.clone(),
                account_name: grant.account_name.clone(),
                scope: grant.scope.clone().or(Some(crate::watch::SCOPE.to_string())),
                logged_in_at: Some(rfc3339(SystemTime::now())),
            };
            credentials::store(&token_file, &grant.token, &login)?;
            let machine = login.machine_name.as_deref().map(printable);
            let account = login.account_name.as_deref().map(printable);
            let _ = writeln!(
                ui,
                "{} can now report to {}. It can read nothing.",
                machine.as_deref().unwrap_or("This machine"),
                account.as_deref().unwrap_or("your Watch account"),
            );
            // Rotation: the token this one replaces is revoked, best effort,
            // at the API that issued it.
            let mut previous_revoked = None;
            let replaced = previous.as_ref().is_some_and(|old| old.token != grant.token);
            if let Some(old) = previous.filter(|_| replaced) {
                let old_api = old.login.map(|l| l.api_url).unwrap_or_else(|| api_url.clone());
                let revoked = Client::new(&old_api, HTTP_TIMEOUT)
                    .is_ok_and(|client| client.revoke(&old.token).is_ok());
                if !revoked {
                    let _ = writeln!(
                        ui,
                        "The token this replaces could not be revoked. Revoke it in the console, \
                         under the machine's tokens."
                    );
                }
                previous_revoked = Some(revoked);
            }
            return Ok(Output {
                json: signed_in(&token_file, &login, replaced, previous_revoked),
                code: EXIT_OK,
            });
        }
        Flow::Denied => (
            "access_denied",
            "The request was denied in the console. Nothing was stored.",
            EXIT_DENIED,
        ),
        Flow::Expired => (
            "expired_token",
            "The code expired before it was approved. Nothing was stored. Run nucleate login again.",
            EXIT_EXPIRED,
        ),
        Flow::Cancelled => ("cancelled", "Cancelled. Nothing was stored.", EXIT_INTERRUPTED),
    };
    let _ = writeln!(ui, "{line}");
    Ok(Output {
        json: json!({ "logged_in": false, "reason": reason, "api_url": api_url }),
        code,
    })
}

/// `nucleate logout`: revoke at Watch, then delete from the box. The token is
/// deleted whether or not Watch could be told.
pub fn logout(ui: &mut dyn Write) -> Result<Output> {
    let settings = settings::load()?;
    let token_file = settings.token_file.clone();
    let credential = match credentials::read_any(&token_file) {
        Ok(Stored::Present(credential)) => credential,
        Ok(_) => {
            credentials::remove(&token_file)?;
            return Ok(Output {
                json: json!({ "logged_out": false, "reason": "not_logged_in", "token_file": token_file }),
                code: EXIT_OK,
            });
        }
        Err(_) => {
            // A token file that does not hold a token: nothing to revoke.
            credentials::remove(&token_file)?;
            let _ = writeln!(ui, "The token file did not hold a token. It is deleted.");
            return Ok(Output {
                json: json!({ "logged_out": true, "revoked": false, "token_file": token_file }),
                code: EXIT_OK,
            });
        }
    };
    let api_url = credential
        .login
        .as_ref()
        .map(|login| login.api_url.clone())
        .unwrap_or_else(|| settings.login_api_url(None));
    let revoked = Client::new(&api_url, HTTP_TIMEOUT)
        .map_err(|error| error.to_string())
        .and_then(|client| client.revoke(&credential.token).map_err(|e| e.to_string()));
    credentials::remove(&token_file)?;
    match &revoked {
        Ok(()) => {
            let _ = writeln!(
                ui,
                "Logged out. The token is revoked and deleted from this box."
            );
        }
        Err(why) => {
            let _ = writeln!(
                ui,
                "The token is deleted from this box, but Watch could not be told: {why}.\n\
                 It still works until you revoke it in the console, under the machine's tokens."
            );
        }
    }
    Ok(Output {
        json: json!({
            "logged_out": true,
            "revoked": revoked.is_ok(),
            "api_url": api_url,
            "token_file": token_file,
        }),
        code: EXIT_OK,
    })
}

/// `nucleate whoami`: the machine, the account, the API, and whether the
/// token still works.
pub fn whoami(ui: &mut dyn Write) -> Result<Output> {
    let settings = settings::load()?;
    let token_file = settings.token_file.clone();
    let credential = match credentials::read(&token_file)? {
        Stored::Present(credential) => credential,
        Stored::Missing => {
            let _ = writeln!(ui, "This box is not signed in. Run nucleate login.");
            return Ok(Output {
                json: json!({ "logged_in": false, "token_file": token_file }),
                code: EXIT_NOT_LOGGED_IN,
            });
        }
        Stored::Insecure { mode } => {
            let _ = writeln!(
                ui,
                "{} is readable by others (mode {mode:o}), so it is not used. \
                 Make it 600, or run nucleate login.",
                token_file.display()
            );
            return Ok(Output {
                json: json!({
                    "logged_in": false,
                    "reason": "token_file_permissions",
                    "token_file": token_file,
                }),
                code: EXIT_NOT_LOGGED_IN,
            });
        }
    };
    let login = credential.login.clone().unwrap_or_else(|| Login {
        api_url: settings.login_api_url(None),
        ..Login::default()
    });
    let status = match Client::new(&login.api_url, HTTP_TIMEOUT) {
        Ok(client) => client.token_status(&credential.token),
        Err(error) => bail!("{error:#}"),
    };
    let (word, code) = match &status {
        TokenStatus::Valid => ("valid", EXIT_OK),
        TokenStatus::Rejected => {
            let _ = writeln!(
                ui,
                "Watch no longer accepts this box's token. Run nucleate login."
            );
            ("rejected", EXIT_TOKEN_REJECTED)
        }
        TokenStatus::Unknown(why) => {
            let _ = writeln!(ui, "Could not check the token: {why}.");
            ("unknown", EXIT_ERROR)
        }
    };
    Ok(Output {
        json: json!({
            "logged_in": true,
            "api_url": login.api_url,
            "machine_id": login.machine_id,
            "machine_name": login.machine_name,
            "account_name": login.account_name,
            "scope": login.scope,
            "logged_in_at": login.logged_in_at,
            "token": word,
            "token_file": token_file,
        }),
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_is_five_rows_of_five() {
        for (name, glyph) in GLYPHS {
            assert_eq!(glyph.len(), 25, "glyph {name}");
            assert!(glyph.chars().all(|c| c == '#' || c == ' '), "glyph {name}");
        }
    }

    #[test]
    fn a_code_is_drawn_large() {
        let rows = large("WDJB-MJHT", '#').unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|row| row.len() <= 9 * 6));
        assert_eq!(large("wdjb", '#'), large("WDJB", '#'));
        // Nothing is drawn for a code with a character there is no glyph for.
        assert!(large("WD_JB", '#').is_none());
        assert!(large("", '#').is_none());
    }

    #[test]
    fn the_prompt_shows_the_code_the_address_and_the_link() {
        let authorization = DeviceAuthorization {
            device_code: "device-code-not-shown".into(),
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://app.nucarium.com/activate".into(),
            verification_uri_complete: Some(
                "https://app.nucarium.com/activate?user_code=WDJB-MJHT".into(),
            ),
            expires_in: 900,
            interval: Some(5),
        };
        let mut shown = Vec::new();
        show_code(&mut shown, &authorization).unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains("      WDJB-MJHT\n"));
        assert!(shown.contains("      https://app.nucarium.com/activate\n"));
        assert!(shown.contains("activate?user_code=WDJB-MJHT"));
        assert!(shown.contains("15 minutes"));
        assert!(!shown.contains("device-code-not-shown"));
    }
}
