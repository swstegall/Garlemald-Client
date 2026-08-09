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

//! Login flow: opens the selected server's loginUrl in a native webview
//! and waits for it to redirect to `ffxiv://login_success?sessionId=…`, at
//! which point we extract the 56-character session id and hand it off to
//! the launch pipeline.
//!
//! Because wry+tao and eframe's winit event loop can't both own the main
//! thread (a macOS constraint), the webview runs in a *subprocess*: the
//! binary re-enters itself with `--login-webview <URL>` and communicates the
//! result on stdout. The egui parent process spawns this child and polls a
//! channel for the outcome each frame, never blocking the UI.

//!
//! Servers without a web login page can instead expose a bahamut-style
//! JSON auth API (`api_url` in the server definition); `native.rs` speaks
//! it directly from an in-process login form — no webview involved.

mod native;
mod subprocess;
mod webview;

pub use native::{NativeAuthOutcome, NativeAuthTask};
pub use subprocess::{LoginOutcome, LoginTask};
pub use webview::{CANCEL_SENTINEL, ERROR_PREFIX, SESSION_PREFIX, run_webview};
