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

//! The patch-distribution endpoint that serves the torrent magnet link.

use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Distribution endpoint. Responds `{"magnet": "<magnet-uri>"}`.
pub const TORRENT_ENDPOINT: &str = "https://www.stegall.me/ffxiv/1.0/patches/torrent";

/// Failures fetching or decoding the magnet-link response.
#[derive(Debug, thiserror::Error)]
pub enum MagnetFetchError {
    #[error("could not reach the patch distribution endpoint: {0}")]
    Transport(String),
    #[error("patch distribution endpoint returned a malformed response: {0}")]
    Malformed(String),
}

#[derive(serde::Deserialize)]
struct MagnetResponse {
    magnet: String,
}

/// Fetch the magnet URI from `endpoint_url`.
pub fn fetch_magnet(endpoint_url: &str) -> Result<String, MagnetFetchError> {
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
    let response = match agent.get(endpoint_url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => {
            return Err(MagnetFetchError::Malformed(format!("HTTP {code}")));
        }
        Err(ureq::Error::Transport(t)) => {
            return Err(MagnetFetchError::Transport(t.to_string()));
        }
    };

    let decoded: MagnetResponse = response
        .into_json()
        .map_err(|e| MagnetFetchError::Malformed(format!("invalid JSON: {e}")))?;

    if !decoded.magnet.starts_with("magnet:") {
        return Err(MagnetFetchError::Malformed(format!(
            "expected a magnet: URI, got {:?}",
            decoded.magnet
        )));
    }

    Ok(decoded.magnet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Starts a one-shot HTTP server on an ephemeral port: it accepts a
    /// single connection, discards the request headers, writes back the
    /// canned `status`/`body` response, and then exits. Returns the base
    /// URL to hit it at. Stands in for `httpmock`, which is not a
    /// dependency here.
    fn one_shot_http_server(status: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("could not bind test listener");
        let addr = listener.local_addr().expect("test listener has no addr");

        thread::spawn(move || {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut reader = BufReader::new(stream.try_clone().expect("could not clone stream"));
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            let mut stream = stream;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });

        format!("http://{addr}/torrent")
    }

    #[test]
    fn fetch_magnet_success_returns_the_uri() {
        let url = one_shot_http_server(
            "200 OK",
            r#"{"magnet":"magnet:?xt=urn:btih:abcdef0123456789"}"#,
        );
        let magnet = fetch_magnet(&url).unwrap();
        assert_eq!(magnet, "magnet:?xt=urn:btih:abcdef0123456789");
    }

    #[test]
    fn fetch_magnet_server_error_is_malformed() {
        let url = one_shot_http_server("500 Internal Server Error", "internal error");
        let err = fetch_magnet(&url).unwrap_err();
        match err {
            MagnetFetchError::Malformed(msg) => assert!(msg.contains("500"), "got {msg:?}"),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn fetch_magnet_invalid_json_is_malformed() {
        let url = one_shot_http_server("200 OK", "not json at all");
        let err = fetch_magnet(&url).unwrap_err();
        assert!(matches!(err, MagnetFetchError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn fetch_magnet_missing_field_is_malformed() {
        let url = one_shot_http_server("200 OK", r#"{"not_magnet":"nope"}"#);
        let err = fetch_magnet(&url).unwrap_err();
        assert!(matches!(err, MagnetFetchError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn fetch_magnet_non_magnet_value_is_malformed() {
        let url =
            one_shot_http_server("200 OK", r#"{"magnet":"https://example.com/not-a-magnet"}"#);
        let err = fetch_magnet(&url).unwrap_err();
        match err {
            MagnetFetchError::Malformed(msg) => assert!(msg.contains("magnet:"), "got {msg:?}"),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
