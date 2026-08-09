// garlemald-client — cross-platform launcher for FINAL FANTASY XIV 1.x private servers
// Copyright (C) 2026  Samuel Stegall
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published
// by the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native JSON login/sign-up against a bahamut-style auth API, used when a
//! server entry supplies an `api_url` instead of a `login_url` web login
//! page. Speaks the contract of the BahamutXIV auth service (mirroring the
//! sibling bahamut-launcher's client): `POST <api_url>/accounts` to
//! register, `POST <api_url>/sessions` to log in; non-2xx responses carry
//! an `{"error":{"code","message"}}` envelope.
//!
//! Plain HTTP is allowed only for loopback hosts — the request body carries
//! the account password, which must not cross a network unencrypted.
//!
//! The blocking HTTP calls run on a detached worker thread and report back
//! over an `mpsc` channel the egui frame loop polls, mirroring `LoginTask`.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::crypto::SESSION_ID_LEN;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("garlemald-client/", env!("CARGO_PKG_VERSION"));

#[derive(Serialize)]
struct CredentialsRequest<'a> {
    username: &'a str,
    password: &'a str,
}

/// `POST <api_url>/sessions` success body. `username` and `expires_at` are
/// also in the contract but unused here, and serde ignores unknown fields.
#[derive(Deserialize)]
struct LoginResponse {
    session_id: String,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeAuthOutcome {
    /// Login succeeded; carries the 56-character hex session id.
    Success(String),
    /// User-presentable failure message.
    Error(String),
}

/// Validate and normalize an `api_url` into a base ending in `/`, enforcing
/// the loopback-only-plain-HTTP rule. Returns a user-presentable error.
pub fn validate_api_base(api_url: &str) -> Result<String, String> {
    let rest = if let Some(rest) = api_url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = api_url.strip_prefix("http://") {
        let host = host_of(rest);
        if !is_loopback_host(&host) {
            return Err(format!(
                "refusing to send a password over unencrypted http to {host:?}; \
                 the server's api_url must use https (http is allowed only for localhost)"
            ));
        }
        rest
    } else {
        return Err(format!(
            "server api_url {api_url:?} must start with https:// (or http:// for localhost)"
        ));
    };
    if host_of(rest).is_empty() {
        return Err(format!("server api_url {api_url:?} has no host"));
    }
    let mut base = api_url.to_string();
    if !base.ends_with('/') {
        base.push('/');
    }
    Ok(base)
}

/// Host portion (no port) of a URL with the scheme already stripped.
fn host_of(rest: &str) -> String {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(bracketed) = authority.strip_prefix('[') {
        // [::1]:8080 — the port sits outside the brackets.
        return bracketed.split(']').next().unwrap_or("").to_string();
    }
    authority.split(':').next().unwrap_or("").to_string()
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Map an HTTP-level failure to a message fit for the login panel.
fn friendly_api_error(status: u16, error: ApiError, retry_after: Option<u32>) -> String {
    match error.code.as_str() {
        "invalid_credentials" => "Wrong username or password.".into(),
        "username_taken" => "That username is already taken.".into(),
        "rate_limited" => match retry_after {
            Some(secs) => format!("Too many attempts; try again in {secs} seconds."),
            None => "Too many attempts; try again shortly.".into(),
        },
        // validation_error and any future codes: the server message is
        // already written for the account owner.
        _ => format!("{} (HTTP {status})", error.message),
    }
}

fn post_credentials(
    agent: &ureq::Agent,
    url: &str,
    username: &str,
    password: &str,
) -> Result<ureq::Response, String> {
    let body = CredentialsRequest { username, password };
    match agent.post(url).send_json(&body) {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(status, response)) => {
            let retry_after = response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u32>().ok());
            match response.into_json::<ErrorEnvelope>() {
                Ok(envelope) => Err(friendly_api_error(status, envelope.error, retry_after)),
                Err(_) => Err(format!("server returned HTTP {status}")),
            }
        }
        Err(ureq::Error::Transport(t)) => Err(format!("could not reach the server: {t}")),
    }
}

/// Register (optionally) then log in; returns the session id.
fn run_auth_flow(
    api_base: &str,
    username: &str,
    password: &str,
    create_account: bool,
) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();
    if create_account {
        // 201 has no session token by contract; a login must follow.
        post_credentials(&agent, &format!("{api_base}accounts"), username, password)?;
    }
    let response = post_credentials(&agent, &format!("{api_base}sessions"), username, password)?;
    let login: LoginResponse = response
        .into_json()
        .map_err(|e| format!("malformed login response: {e}"))?;
    let sid = login.session_id;
    if sid.len() != SESSION_ID_LEN || !sid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "server returned a malformed session id ({} chars; expected {SESSION_ID_LEN} hex)",
            sid.len()
        ));
    }
    Ok(sid)
}

/// One in-flight native login (or sign-up + login) attempt.
pub struct NativeAuthTask {
    receiver: mpsc::Receiver<NativeAuthOutcome>,
}

impl NativeAuthTask {
    /// Validates `api_url` synchronously (so a bad bundled entry fails
    /// immediately), then runs the HTTP flow on a detached worker thread.
    /// The thread is not joined on drop: abandoning the task simply means
    /// its send lands in a closed channel once the request times out.
    pub fn start(
        api_url: &str,
        username: String,
        password: String,
        create_account: bool,
    ) -> Result<Self, String> {
        let api_base = validate_api_base(api_url)?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("garlemald-native-auth".into())
            .spawn(move || {
                let outcome = match run_auth_flow(&api_base, &username, &password, create_account) {
                    Ok(session_id) => NativeAuthOutcome::Success(session_id),
                    Err(message) => NativeAuthOutcome::Error(message),
                };
                let _ = tx.send(outcome);
            })
            .map_err(|e| format!("spawning auth worker thread: {e}"))?;
        Ok(Self { receiver: rx })
    }

    pub fn try_recv(&mut self) -> Option<NativeAuthOutcome> {
        self.receiver.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn https_api_base_is_accepted_and_slash_terminated() {
        let base = validate_api_base("https://bahamut.stegall.me/api/v1").unwrap();
        assert_eq!(base, "https://bahamut.stegall.me/api/v1/");
        let already = validate_api_base("https://bahamut.stegall.me/api/v1/").unwrap();
        assert_eq!(already, "https://bahamut.stegall.me/api/v1/");
    }

    #[test]
    fn plain_http_is_loopback_only() {
        for ok in [
            "http://127.0.0.1:8080/api/v1",
            "http://localhost:8080/api/v1",
            "http://[::1]:8080/api/v1",
        ] {
            assert!(validate_api_base(ok).is_ok(), "{ok} should be allowed");
        }
        let err = validate_api_base("http://bahamut.stegall.me:8080/api/v1").unwrap_err();
        assert!(err.contains("unencrypted"), "got: {err}");
    }

    #[test]
    fn junk_urls_are_rejected() {
        assert!(validate_api_base("").is_err());
        assert!(validate_api_base("ftp://example.com/api").is_err());
        assert!(validate_api_base("bahamut.stegall.me/api/v1").is_err());
        assert!(validate_api_base("https://").is_err());
    }

    #[test]
    fn known_error_codes_map_to_friendly_messages() {
        let err = |code: &str| ApiError {
            code: code.into(),
            message: "server words".into(),
        };
        assert_eq!(
            friendly_api_error(401, err("invalid_credentials"), None),
            "Wrong username or password."
        );
        assert_eq!(
            friendly_api_error(409, err("username_taken"), None),
            "That username is already taken."
        );
        assert_eq!(
            friendly_api_error(429, err("rate_limited"), Some(30)),
            "Too many attempts; try again in 30 seconds."
        );
        // Unknown / validation codes surface the server's own message.
        assert_eq!(
            friendly_api_error(400, err("validation_error"), None),
            "server words (HTTP 400)"
        );
    }

    /// Serves `responses` to sequential connections (one request each,
    /// `Connection: close` so ureq cannot pipeline onto a kept-alive
    /// socket), capturing each request head+body for assertions.
    fn canned_http_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let mut request = Vec::new();
                // Read until the JSON body closes; requests here are tiny.
                loop {
                    let n = stream.read(&mut buf).expect("read request");
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let content_length = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + 4 + content_length {
                            break;
                        }
                    }
                }
                seen.push(String::from_utf8_lossy(&request).into_owned());
                let reason = if status == 200 { "OK" } else { "ERR" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
            seen
        });
        (format!("http://127.0.0.1:{}/api/v1", addr.port()), handle)
    }

    const GOOD_SESSION: &str = "0123456789abcdef0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn login_round_trip_returns_session_id() {
        let (api_url, server) = canned_http_server(vec![(
            200,
            r#"{"session_id":"0123456789abcdef0123456789abcdef0123456789abcdef01234567","username":"alice","expires_at":"2026-08-09T00:00:00Z"}"#,
        )]);
        let mut task = NativeAuthTask::start(&api_url, "alice".into(), "pw".into(), false).unwrap();
        let outcome = wait_for(&mut task);
        assert_eq!(outcome, NativeAuthOutcome::Success(GOOD_SESSION.into()));
        let seen = server.join().unwrap();
        assert!(
            seen[0].starts_with("POST /api/v1/sessions "),
            "got: {}",
            seen[0]
        );
        assert!(seen[0].contains(r#""username":"alice""#));
        assert!(seen[0].contains(r#""password":"pw""#));
    }

    #[test]
    fn sign_up_registers_then_logs_in() {
        let (api_url, server) = canned_http_server(vec![
            (
                201,
                r#"{"username":"bob","created_at":"2026-08-09T00:00:00Z"}"#,
            ),
            (
                200,
                r#"{"session_id":"0123456789abcdef0123456789abcdef0123456789abcdef01234567","username":"bob","expires_at":"2026-08-09T00:00:00Z"}"#,
            ),
        ]);
        let mut task = NativeAuthTask::start(&api_url, "bob".into(), "pw".into(), true).unwrap();
        let outcome = wait_for(&mut task);
        assert_eq!(outcome, NativeAuthOutcome::Success(GOOD_SESSION.into()));
        let seen = server.join().unwrap();
        assert!(
            seen[0].starts_with("POST /api/v1/accounts "),
            "got: {}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("POST /api/v1/sessions "),
            "got: {}",
            seen[1]
        );
    }

    #[test]
    fn invalid_credentials_surface_the_friendly_message() {
        let (api_url, _server) = canned_http_server(vec![(
            401,
            r#"{"error":{"code":"invalid_credentials","message":"nope"}}"#,
        )]);
        let mut task =
            NativeAuthTask::start(&api_url, "alice".into(), "wrong".into(), false).unwrap();
        assert_eq!(
            wait_for(&mut task),
            NativeAuthOutcome::Error("Wrong username or password.".into())
        );
    }

    #[test]
    fn malformed_session_id_is_rejected() {
        let (api_url, _server) = canned_http_server(vec![(
            200,
            r#"{"session_id":"deadbeef","username":"alice","expires_at":"2026-08-09T00:00:00Z"}"#,
        )]);
        let mut task = NativeAuthTask::start(&api_url, "alice".into(), "pw".into(), false).unwrap();
        match wait_for(&mut task) {
            NativeAuthOutcome::Error(msg) => {
                assert!(msg.contains("malformed session id"), "got: {msg}")
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    fn wait_for(task: &mut NativeAuthTask) -> NativeAuthOutcome {
        for _ in 0..200 {
            if let Some(outcome) = task.try_recv() {
                return outcome;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("auth task did not report within 5s");
    }
}
