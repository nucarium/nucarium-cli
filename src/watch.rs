//! The Watch API, as the box uses it: sign in, revoke, heartbeat, events.
//!
//! Everything the box sends goes through this file.

use crate::credentials::Token;
use crate::envelope::Envelope;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

pub const CLIENT_ID: &str = "nucleate";
pub const SCOPE: &str = "ingest";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MAX_BODY: u64 = 1024 * 1024;

/// Why a request did not succeed, sorted by what the caller should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No answer: no network, no route, a timeout, a TLS failure. Try again later.
    Unreachable(String),
    /// Watch does not accept the token. Trying again will not help.
    Unauthorized,
    /// Watch asked for a pause.
    RateLimited { retry_after: Option<Duration> },
    /// Watch understood the request and refused it. Do not send it again as it is.
    Refused {
        status: u16,
        code: Option<String>,
        detail: Option<String>,
    },
    /// Watch failed. Try again later.
    Server { status: u16 },
    /// An answer that is not the API's.
    Unexpected(String),
}

impl ApiError {
    /// True when the same request may succeed later.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ApiError::Unreachable(_)
                | ApiError::RateLimited { .. }
                | ApiError::Server { .. }
                | ApiError::Unexpected(_)
        )
    }

    /// A short word for logs and status, with nothing from the server in it.
    pub fn kind(&self) -> &'static str {
        match self {
            ApiError::Unreachable(_) => "unreachable",
            ApiError::Unauthorized => "unauthorized",
            ApiError::RateLimited { .. } => "rate_limited",
            ApiError::Refused { .. } => "refused",
            ApiError::Server { .. } => "server_error",
            ApiError::Unexpected(_) => "unexpected_answer",
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unreachable(why) => write!(f, "Watch could not be reached ({why})"),
            ApiError::Unauthorized => write!(f, "Watch does not accept this token"),
            ApiError::RateLimited { .. } => write!(f, "Watch asked this box to slow down"),
            ApiError::Refused {
                status,
                code,
                detail,
            } => {
                write!(f, "Watch refused the request ({status}")?;
                if let Some(code) = code {
                    write!(f, " {}", printable(code))?;
                }
                write!(f, ")")?;
                if let Some(detail) = detail {
                    write!(f, ": {}", printable(detail))?;
                }
                Ok(())
            }
            ApiError::Server { status } => write!(f, "Watch failed ({status})"),
            ApiError::Unexpected(what) => write!(f, "an unexpected answer from Watch ({what})"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Text from the network, made safe to print on a terminal.
pub fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(300).collect()
}

/// Check an API address and trim it. `https` anywhere; `http` only for this
/// machine, so a token is never sent in the clear across a network.
pub fn normalize_api_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("{url:?} is not an http(s) address"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || rest.contains(['?', '#']) {
        bail!("{url:?} is not a plain API address");
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1");
    match scheme {
        "https" => {}
        "http" if local => {}
        "http" => bail!("{url:?} is not https; plain http is only for an API on this machine"),
        _ => bail!("{url:?} is not an http(s) address"),
    }
    Ok(url.to_string())
}

/// The answer to a device authorization request (RFC 8628 §3.2).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub interval: Option<u64>,
}

/// What signing in grants.
#[derive(Debug, Clone)]
pub struct Grant {
    pub token: Token,
    pub scope: Option<String>,
    pub machine_id: Option<String>,
    pub machine_name: Option<String>,
    pub account_name: Option<String>,
}

/// One answer from the token endpoint while waiting for approval.
#[derive(Debug, Clone)]
pub enum TokenPoll {
    Granted(Grant),
    Pending,
    SlowDown,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineHealth {
    pub disk_used_pct: f64,
    pub load_1m: f64,
    pub uptime_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_pressure_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EnvironmentReport {
    pub env_id: String,
    pub server_alive: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Heartbeat {
    pub machine: MachineHealth,
    pub environments: Vec<EnvironmentReport>,
    pub daemon_version: String,
    pub client_ts: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatAck {
    #[serde(default)]
    pub heartbeat_interval_s: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RejectedEvent {
    pub event_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventsAck {
    #[serde(default)]
    pub accepted: u64,
    #[serde(default)]
    pub duplicates: u64,
    #[serde(default)]
    pub rejected: Vec<RejectedEvent>,
}

/// Whether Watch still accepts a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenStatus {
    Valid,
    Rejected,
    Unknown(ApiError),
}

struct Answer {
    status: u16,
    retry_after: Option<Duration>,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// Sort a non-2xx answer written as RFC 7807 problem details.
    fn problem(&self) -> ApiError {
        match self.status {
            401 => ApiError::Unauthorized,
            408 => ApiError::Unreachable("the request timed out at Watch".to_string()),
            429 => ApiError::RateLimited {
                retry_after: self.retry_after,
            },
            400..=499 => {
                let body = self.json();
                ApiError::Refused {
                    status: self.status,
                    code: body["code"].as_str().map(str::to_string),
                    detail: body["detail"]
                        .as_str()
                        .or_else(|| body["title"].as_str())
                        .map(str::to_string),
                }
            }
            500..=599 => ApiError::Server {
                status: self.status,
            },
            other => ApiError::Unexpected(format!("status {other}")),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, ApiError> {
        serde_json::from_str(&self.body).map_err(|_| {
            ApiError::Unexpected(format!(
                "status {} with a body that is not the API's",
                self.status
            ))
        })
    }
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
}

impl Client {
    /// A client for the API at `base_url`. No request takes longer than `timeout`.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Client> {
        let base = normalize_api_url(base_url)?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            // A redirect could carry the token to another host. The API does
            // not redirect, so one is treated as an answer that is not the API's.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .user_agent(concat!("nucleate/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Client { agent, base })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn answer(
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Answer, ApiError> {
        let mut response = result.map_err(transport)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(transport)?;
        Ok(Answer {
            status,
            retry_after,
            body,
        })
    }

    fn form(&self, path: &str, fields: &[(&str, &str)]) -> Result<Answer, ApiError> {
        Self::answer(
            self.agent
                .post(format!("{}{path}", self.base))
                .header("Accept", "application/json")
                .send_form(fields.iter().copied()),
        )
    }

    fn ingest(&self, path: &str, token: &Token, body: &impl Serialize) -> Result<Answer, ApiError> {
        let answer = Self::answer(
            self.agent
                .post(format!("{}{path}", self.base))
                .header("Authorization", format!("Bearer {}", token.expose()))
                .header("Accept", "application/json")
                .send_json(body),
        )?;
        match answer.status {
            200..=299 => Ok(answer),
            _ => Err(answer.problem()),
        }
    }

    /// Ask for a code the owner can approve (RFC 8628 §3.1).
    pub fn device_authorization(
        &self,
        machine_name: Option<&str>,
    ) -> Result<DeviceAuthorization, ApiError> {
        let mut fields = vec![("client_id", CLIENT_ID), ("scope", SCOPE)];
        if let Some(name) = machine_name {
            fields.push(("machine_name", name));
        }
        let answer = self.form("/v1/oauth/device_authorization", &fields)?;
        match answer.status {
            200..=299 => answer.parse(),
            _ => Err(oauth_error(&answer)),
        }
    }

    /// Ask once whether the code has been approved (RFC 8628 §3.4).
    pub fn token(&self, device_code: &str) -> Result<TokenPoll, ApiError> {
        let answer = self.form(
            "/v1/oauth/token",
            &[
                ("grant_type", DEVICE_GRANT),
                ("device_code", device_code),
                ("client_id", CLIENT_ID),
            ],
        )?;
        let body = answer.json();
        if (200..=299).contains(&answer.status) {
            let text = |key: &str| body[key].as_str().map(str::to_string);
            let token = body["access_token"]
                .as_str()
                .and_then(|raw| Token::parse(raw).ok())
                .ok_or_else(|| ApiError::Unexpected("a grant with no usable token".to_string()))?;
            return Ok(TokenPoll::Granted(Grant {
                token,
                scope: text("scope"),
                machine_id: text("machine_id"),
                machine_name: text("machine_name"),
                account_name: text("account_name"),
            }));
        }
        match body["error"].as_str() {
            Some("authorization_pending") => Ok(TokenPoll::Pending),
            Some("slow_down") => Ok(TokenPoll::SlowDown),
            Some("access_denied") => Ok(TokenPoll::Denied),
            Some("expired_token") => Ok(TokenPoll::Expired),
            _ => Err(oauth_error(&answer)),
        }
    }

    /// Revoke a token (RFC 7009). Watch answers 200 whether or not the token
    /// was still live.
    pub fn revoke(&self, token: &Token) -> Result<(), ApiError> {
        let answer = self.form(
            "/v1/oauth/revoke",
            &[
                ("token", token.expose()),
                ("token_type_hint", "access_token"),
                ("client_id", CLIENT_ID),
            ],
        )?;
        match answer.status {
            200..=299 => Ok(()),
            _ => Err(oauth_error(&answer)),
        }
    }

    pub fn heartbeat(
        &self,
        token: &Token,
        heartbeat: &Heartbeat,
    ) -> Result<HeartbeatAck, ApiError> {
        self.ingest("/v1/ingest/heartbeat", token, heartbeat)?
            .parse()
    }

    /// Send a batch of at most 100 envelopes.
    pub fn events(&self, token: &Token, events: &[&Envelope]) -> Result<EventsAck, ApiError> {
        self.ingest("/v1/ingest/events", token, &json!({ "events": events }))?
            .parse()
    }

    /// Ask whether Watch still accepts a token, without reporting anything:
    /// an empty batch of events is answered with a validation error when the
    /// token is good, and with 401 when it is not.
    pub fn token_status(&self, token: &Token) -> TokenStatus {
        let events: [&Envelope; 0] = [];
        match self.events(token, &events) {
            Ok(_) | Err(ApiError::Refused { .. }) | Err(ApiError::RateLimited { .. }) => {
                TokenStatus::Valid
            }
            Err(ApiError::Unauthorized) => TokenStatus::Rejected,
            Err(other) => TokenStatus::Unknown(other),
        }
    }
}

fn transport(error: ureq::Error) -> ApiError {
    match error {
        ureq::Error::BadUri(_) | ureq::Error::Http(_) | ureq::Error::Protocol(_) => {
            ApiError::Unexpected(error.to_string())
        }
        ureq::Error::BodyExceedsLimit(_) => ApiError::Unexpected("an oversized answer".to_string()),
        other => ApiError::Unreachable(other.to_string()),
    }
}

/// Sort a non-2xx answer from an OAuth endpoint, which carries
/// `{ "error", "error_description" }` rather than problem details.
fn oauth_error(answer: &Answer) -> ApiError {
    let body = answer.json();
    match answer.status {
        429 => ApiError::RateLimited {
            retry_after: answer.retry_after,
        },
        400..=499 => ApiError::Refused {
            status: answer.status,
            code: body["error"]
                .as_str()
                .or_else(|| body["code"].as_str())
                .map(str::to_string),
            detail: body["error_description"]
                .as_str()
                .or_else(|| body["detail"].as_str())
                .map(str::to_string),
        },
        _ => answer.problem(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_anywhere_and_http_only_on_this_machine() {
        assert_eq!(
            normalize_api_url("https://api.nucarium.com/").unwrap(),
            "https://api.nucarium.com"
        );
        assert!(normalize_api_url("http://localhost:8787").is_ok());
        assert!(normalize_api_url("http://127.0.0.1:8787").is_ok());
        assert!(normalize_api_url("http://[::1]:8787").is_ok());
        assert!(normalize_api_url("http://api.nucarium.com").is_err());
        assert!(normalize_api_url("http://localhost.example.com").is_err());
        assert!(normalize_api_url("http://localhost@example.com").is_err());
        assert!(normalize_api_url("ftp://localhost").is_err());
        assert!(normalize_api_url("api.nucarium.com").is_err());
        assert!(normalize_api_url("https://api.nucarium.com/?x=1").is_err());
    }

    #[test]
    fn text_from_the_network_cannot_drive_the_terminal() {
        assert_eq!(printable("ok\u{1b}[2Jgone\n"), "ok[2Jgone");
    }
}
