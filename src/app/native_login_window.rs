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

//! Native login/sign-up modal for servers that expose a JSON auth API
//! (`api_url`) instead of a web login page (`login_url`). Owns the
//! in-flight [`NativeAuthTask`] and polls it each frame; the parent only
//! sees the terminal outcome.

use std::time::Duration;

use crate::login::{NativeAuthOutcome, NativeAuthTask};

pub enum NativeLoginOutcome {
    Open,
    Cancelled,
    /// Login succeeded; carries the 56-character hex session id.
    Success(String),
}

pub struct NativeLoginModal {
    server_name: String,
    api_url: String,
    username: String,
    password: String,
    confirm_password: String,
    create_account: bool,
    task: Option<NativeAuthTask>,
    error: Option<String>,
    pub open: bool,
}

impl NativeLoginModal {
    pub fn new(server_name: String, api_url: String) -> Self {
        Self {
            server_name,
            api_url,
            username: String::new(),
            password: String::new(),
            confirm_password: String::new(),
            create_account: false,
            task: None,
            error: None,
            open: true,
        }
    }

    fn submit(&mut self) {
        let username = self.username.trim().to_string();
        if username.is_empty() || self.password.is_empty() {
            self.error = Some("Enter a username and password.".into());
            return;
        }
        if self.create_account && self.password != self.confirm_password {
            self.error = Some("Passwords do not match.".into());
            return;
        }
        match NativeAuthTask::start(
            &self.api_url,
            username,
            self.password.clone(),
            self.create_account,
        ) {
            Ok(task) => {
                self.error = None;
                self.task = Some(task);
            }
            Err(message) => self.error = Some(message),
        }
    }

    pub fn render(&mut self, ctx: &egui::Context) -> NativeLoginOutcome {
        let mut outcome = NativeLoginOutcome::Open;

        // Poll the in-flight attempt before drawing so the busy state and
        // error label reflect this frame's reality.
        if let Some(task) = self.task.as_mut()
            && let Some(auth_outcome) = task.try_recv()
        {
            self.task = None;
            match auth_outcome {
                NativeAuthOutcome::Success(session_id) => {
                    return NativeLoginOutcome::Success(session_id);
                }
                NativeAuthOutcome::Error(message) => self.error = Some(message),
            }
        }

        let busy = self.task.is_some();
        let mut open = self.open;
        let mut submitted = false;
        egui::Window::new(format!("Log in to {}", self.server_name))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    ui.checkbox(&mut self.create_account, "Create a new account");
                    ui.separator();
                    ui.label("Username:");
                    ui.add(egui::TextEdit::singleline(&mut self.username).desired_width(240.0));
                    ui.label("Password:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(240.0),
                    );
                    if self.create_account {
                        ui.label("Confirm password:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.confirm_password)
                                .password(true)
                                .desired_width(240.0),
                        );
                    }
                    ui.add_space(4.0);
                    let label = if self.create_account {
                        "Create account and log in"
                    } else {
                        "Log in"
                    };
                    if ui.button(label).clicked() {
                        submitted = true;
                    }
                });
                if busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(if self.create_account {
                            "Creating account…"
                        } else {
                            "Logging in…"
                        });
                    });
                }
                if let Some(error) = &self.error {
                    ui.colored_label(super::theme::error(ui.visuals()), error);
                }
            });
        if submitted {
            self.submit();
        }
        if busy || self.task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Closing the window abandons any in-flight attempt (the worker
        // thread ends on its own once the request completes or times out).
        if !open {
            outcome = NativeLoginOutcome::Cancelled;
        }
        self.open = open;
        outcome
    }
}
