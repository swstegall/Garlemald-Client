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

//! Child-process entry point: hosts a single tao window with a wry WebView
//! pointed at the server's login page and intercepts navigation to the
//! `ffxiv://login_success?sessionId=…` custom scheme. Login pages that
//! instead answer with the retail-era `<x-sqexauth sid="…">` element
//! (Project Meteor lineage servers such as AetherXIV 1.3) are bridged to
//! the same scheme by an injected script that scrapes the element and
//! triggers the `ffxiv://` navigation itself.
//!
//! Communicates the outcome to the parent over stdout using three
//! one-line sentinels defined below. The parent (see
//! `super::subprocess`) reads stdout line-by-line and maps them to a
//! [`LoginOutcome`].

use std::io::Write;

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
#[cfg(target_os = "linux")]
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;
#[cfg(target_os = "linux")]
use wry::WebViewBuilderExtUnix;

use crate::crypto::SESSION_ID_LEN;
use crate::version::APP_NAME;

/// Printed on success, followed by the 56-char session id.
pub const SESSION_PREFIX: &str = "SESSION_ID=";

/// Bridges the retail `<x-sqexauth sid="…"/>` login contract to the
/// `ffxiv://login_success` navigation the handler below understands.
/// Runs on every page load; pages without the element are untouched.
const SQEXAUTH_BRIDGE_SCRIPT: &str = r#"
document.addEventListener("DOMContentLoaded", function () {
    var el = document.querySelector("x-sqexauth");
    if (el) {
        var sid = el.getAttribute("sid");
        if (sid) {
            window.location.href = "ffxiv://login_success?sessionId=" + sid;
        }
    }
});
"#;
/// Printed when the user closes the webview without logging in.
pub const CANCEL_SENTINEL: &str = "LOGIN_CANCELLED";
/// Printed when the webview itself errors out (e.g. fails to load).
pub const ERROR_PREFIX: &str = "LOGIN_ERROR=";

pub fn run_webview(login_url: &str) -> Result<()> {
    let event_loop: EventLoop<()> = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(format!("{APP_NAME} — Login"))
        .with_inner_size(LogicalSize::new(760.0, 600.0))
        .build(&event_loop)
        .context("building tao login window")?;

    let _webview = match build_webview(&window, login_url) {
        Ok(webview) => webview,
        Err(err) => {
            report_line(&format!("{ERROR_PREFIX}{err:#}"));
            return Err(err);
        }
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            report_line(CANCEL_SENTINEL);
            *control_flow = ControlFlow::Exit;
        }
    });

    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(target_os = "linux")]
fn build_webview(window: &tao::window::Window, login_url: &str) -> Result<wry::WebView> {
    // `WebViewBuilder::new(window)` builds from the raw window handle, which wry
    // only supports under X11 — under Wayland it fails with "the window handle
    // kind is not supported". Building from the tao window's GTK vbox embeds the
    // WebKitGTK view at the widget level, which works under both X11 and Wayland
    // (wry's own docs recommend `new_gtk` for Wayland support).
    let vbox = window
        .default_vbox()
        .context("tao login window is missing its default GTK vbox")?;
    WebViewBuilder::new_gtk(vbox)
        .with_url(login_url)
        .with_initialization_script(SQEXAUTH_BRIDGE_SCRIPT)
        .with_navigation_handler(navigation_handler)
        .build()
        .context("building wry webview")
}

#[cfg(target_os = "windows")]
fn build_webview(window: &tao::window::Window, login_url: &str) -> Result<wry::WebView> {
    WebViewBuilder::new(window)
        .with_url(login_url)
        .with_initialization_script(SQEXAUTH_BRIDGE_SCRIPT)
        .with_navigation_handler(navigation_handler)
        .build()
        .context("building wry webview")
}

#[cfg(target_os = "macos")]
fn build_webview(window: &tao::window::Window, login_url: &str) -> Result<wry::WebView> {
    // On macOS wry 0.44 defaults to a child NSView — that is fine for us
    // since the window has no other content.
    WebViewBuilder::new(window)
        .with_url(login_url)
        .with_initialization_script(SQEXAUTH_BRIDGE_SCRIPT)
        .with_navigation_handler(navigation_handler)
        .build()
        .context("building wry webview")
}

fn navigation_handler(uri: String) -> bool {
    if uri.starts_with("ffxiv://login_success") {
        if let Some(session_id) = parse_session_id(&uri) {
            report_line(&format!("{SESSION_PREFIX}{session_id}"));
            std::process::exit(0);
        } else {
            report_line(&format!(
                "{ERROR_PREFIX}login_success URL missing a {SESSION_ID_LEN}-char sessionId"
            ));
            std::process::exit(1);
        }
    }
    if uri.starts_with("ffxiv://") {
        // Other ffxiv:// targets aren't ours — cancel the navigation so the
        // engine doesn't try to handle them itself.
        return false;
    }
    true
}

fn parse_session_id(uri: &str) -> Option<String> {
    let (_, query) = uri.split_once('?')?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == "sessionId" && value.len() == SESSION_ID_LEN {
            return Some(value.to_string());
        }
    }
    None
}

fn report_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_id_from_full_uri() {
        let uri = format!(
            "ffxiv://login_success?sessionId={}",
            "a".repeat(SESSION_ID_LEN)
        );
        let got = parse_session_id(&uri);
        assert_eq!(got.as_deref(), Some(&*"a".repeat(SESSION_ID_LEN)));
    }

    #[test]
    fn rejects_wrong_length_session_id() {
        let uri = "ffxiv://login_success?sessionId=tooshort";
        assert!(parse_session_id(uri).is_none());
    }

    #[test]
    fn ignores_extra_query_params() {
        let good = "a".repeat(SESSION_ID_LEN);
        let uri = format!("ffxiv://login_success?other=1&sessionId={good}&trailing=2");
        assert_eq!(parse_session_id(&uri).as_deref(), Some(&*good));
    }

    #[test]
    fn returns_none_for_missing_query() {
        assert!(parse_session_id("ffxiv://login_success").is_none());
    }
}
