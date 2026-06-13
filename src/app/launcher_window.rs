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

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use eframe::egui;

use crate::app::developer_window::{DeveloperModal, DeveloperOutcome, VERBOSE_WINE_DEBUG};
use crate::app::patcher_window::PatcherScreen;
use crate::app::settings_window::{SettingsModal, SettingsOutcome};
use crate::config::{Preferences, bundled_config_path, data_dir, preferences_file_path};
use crate::launcher::GameLaunchRequest;
use crate::login::{LoginOutcome, LoginTask};
use crate::patcher::PatchSource;
use crate::patcher::check_game_version;
use crate::patcher::manifest::total_bytes;
use crate::platform::{Platform, current as current_platform};
use crate::servers::ServerDefinitions;
use crate::version::{APP_NAME, APP_VERSION, FFXIV_GAME_VERSION};

pub fn run() -> Result<()> {
    log::info!("{APP_NAME} {APP_VERSION} starting");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 480.0])
            .with_min_inner_size([520.0, 360.0])
            .with_title(format!("{APP_NAME} — {APP_VERSION}")),
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        native_options,
        Box::new(|_cc| Ok(Box::new(LauncherApp::new()))),
    )
    .map_err(|e| anyhow::anyhow!("eframe exited: {e}"))?;
    Ok(())
}

enum Screen {
    Main,
    Patcher(PatcherScreen),
}

#[derive(Copy, Clone)]
enum MessageKind {
    Info,
    Error,
}

struct LauncherApp {
    prefs: Preferences,
    prefs_path: PathBuf,
    servers: ServerDefinitions,
    selected_server_name: Option<String>,
    manual_server_address: String,
    detected_install: Option<PathBuf>,
    dev_session_id: String,
    screen: Screen,
    settings_modal: Option<SettingsModal>,
    developer_modal: Option<DeveloperModal>,
    login_task: Option<LoginTask>,
    outdated_prompt_open: bool,
    download_confirm: Option<DownloadConfirmState>,
    last_message: Option<(MessageKind, String)>,
}

struct DownloadConfirmState {
    download_dir: PathBuf,
}

impl LauncherApp {
    fn new() -> Self {
        let prefs_path =
            preferences_file_path().unwrap_or_else(|_| PathBuf::from("preferences.toml"));
        // Fall back to the repo-bundled `./configs/garlemald-client.toml` on
        // first run — that file ships localhost defaults matching the
        // sibling garlemald-server, so a fresh clone of both repos boots
        // end-to-end without user configuration.
        let prefs = if prefs_path.exists() {
            Preferences::load(&prefs_path).unwrap_or_default()
        } else {
            let bundled = bundled_config_path();
            if bundled.exists() {
                Preferences::load(&bundled).unwrap_or_default()
            } else {
                Preferences::default()
            }
        };
        let servers = ServerDefinitions::load_default().unwrap_or_default();
        let detected_install = current_platform().detect_game_install();

        let selected_server_name = if !prefs.launcher.server_name.is_empty() {
            Some(prefs.launcher.server_name.clone())
        } else {
            servers.iter().next().map(|s| s.name.clone())
        };

        let manual_server_address = prefs.launcher.server_address.clone();

        let mut app = Self {
            prefs,
            prefs_path,
            servers,
            selected_server_name,
            manual_server_address,
            detected_install,
            dev_session_id: String::new(),
            screen: Screen::Main,
            settings_modal: None,
            developer_modal: None,
            login_task: None,
            outdated_prompt_open: false,
            download_confirm: None,
            last_message: None,
        };
        app.outdated_prompt_open = app.game_is_installed_but_outdated();
        app
    }

    /// Returns true when a valid install is present on disk but `game.ver`
    /// reports a version other than [`FFXIV_GAME_VERSION`]. Used to
    /// auto-surface the update prompt on startup.
    fn game_is_installed_but_outdated(&self) -> bool {
        let Some(path) = self.resolved_game_location() else {
            return false;
        };
        if !current_platform().is_valid_game_location(&path) {
            return false;
        }
        !check_game_version(&path)
    }

    fn default_download_dir(&self) -> PathBuf {
        self.prefs
            .launcher
            .patch_download_dir
            .clone()
            .unwrap_or_else(|| {
                data_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("ffxiv_patches")
            })
    }

    fn resolved_login_url(&self) -> Option<String> {
        let name = self.selected_server_name.as_ref()?;
        let def = self.servers.get(name)?;
        if def.login_url.is_empty() {
            None
        } else {
            Some(def.login_url.clone())
        }
    }

    fn resolved_game_location(&self) -> Option<PathBuf> {
        self.prefs
            .launcher
            .game_location
            .clone()
            .or_else(|| self.detected_install.clone())
    }

    fn resolved_server_address(&self) -> Option<String> {
        if !self.manual_server_address.trim().is_empty() {
            return Some(self.manual_server_address.trim().to_string());
        }
        let name = self.selected_server_name.as_ref()?;
        let def = self.servers.get(name)?;
        Some(def.address.clone())
    }

    fn save_preferences(&mut self) {
        if let Some(name) = &self.selected_server_name {
            self.prefs.launcher.server_name = name.clone();
        }
        self.prefs.launcher.server_address = self.manual_server_address.clone();
        if let Err(e) = self.prefs.save(&self.prefs_path) {
            log::warn!("failed to save preferences: {e}");
        }
    }

    fn set_info(&mut self, msg: impl Into<String>) {
        self.last_message = Some((MessageKind::Info, msg.into()));
    }
    fn set_error(&mut self, msg: impl Into<String>) {
        self.last_message = Some((MessageKind::Error, msg.into()));
    }

    /// Opens the download-confirmation modal; the actual patcher doesn't
    /// start until the user clicks "Continue" in that modal.
    fn start_update(&mut self) {
        let Some(game_dir) = self.resolved_game_location() else {
            self.set_error("No game location set. Use Game Settings to pick one.");
            return;
        };
        let platform = current_platform();
        if !platform.is_valid_game_location(&game_dir) {
            self.set_error(format!(
                "'{}' doesn't look like an FFXIV install (no ffxivboot.exe).",
                game_dir.display()
            ));
            return;
        }
        self.download_confirm = Some(DownloadConfirmState {
            download_dir: self.default_download_dir(),
        });
    }

    fn kick_off_patcher(&mut self, download_dir: PathBuf) {
        let Some(game_dir) = self.resolved_game_location() else {
            self.set_error("No game location set.");
            return;
        };
        self.prefs.launcher.patch_download_dir = Some(download_dir.clone());
        self.save_preferences();
        self.screen = Screen::Patcher(PatcherScreen::start(
            game_dir,
            PatchSource::Download { download_dir },
        ));
    }

    /// Kicks off the patcher against a user-chosen local directory that
    /// already contains every manifest patch (e.g., another FFXIV 1.x install
    /// with a populated patch cache). Validates sizes + CRCs before applying.
    fn kick_off_local_patcher(&mut self, source_dir: PathBuf) {
        let Some(game_dir) = self.resolved_game_location() else {
            self.set_error("No game location set.");
            return;
        };
        self.screen = Screen::Patcher(PatcherScreen::start(
            game_dir,
            PatchSource::Local { source_dir },
        ));
    }

    /// Opens a folder picker for the user's existing patch cache, then
    /// starts the patcher in local-install mode once a folder is chosen.
    fn start_local_install(&mut self) {
        let game_dir = match self.resolved_game_location() {
            Some(dir) => dir,
            None => {
                self.set_error("No game location set. Use Game Settings to pick one.");
                return;
            }
        };
        let platform = current_platform();
        if !platform.is_valid_game_location(&game_dir) {
            self.set_error(format!(
                "'{}' doesn't look like an FFXIV install (no ffxivboot.exe).",
                game_dir.display()
            ));
            return;
        }
        let Some(source) = rfd::FileDialog::new()
            .set_title("Select folder containing existing FFXIV 1.x patches")
            .pick_folder()
        else {
            return;
        };
        self.kick_off_local_patcher(source);
    }

    fn launch_via_login(&mut self) {
        if self.login_task.is_some() {
            self.set_info("Login window already open.");
            return;
        }
        if self.resolved_game_location().is_none() {
            self.set_error("No game location set.");
            return;
        }
        if self.resolved_server_address().is_none() {
            self.set_error("No server address selected.");
            return;
        }
        let Some(login_url) = self.resolved_login_url() else {
            self.set_error(
                "Selected server has no login URL. Use the developer session-id override.",
            );
            return;
        };
        self.save_preferences();
        match LoginTask::start(login_url.clone()) {
            Ok(task) => {
                self.login_task = Some(task);
                self.set_info(format!("Opening login page: {login_url}"));
            }
            Err(e) => self.set_error(format!("Failed to open login window: {e}")),
        }
    }

    fn launch_game_with_session(&mut self, session_id: String) {
        let Some(game_dir) = self.resolved_game_location() else {
            self.set_error("No game location set.");
            return;
        };
        let Some(server_address) = self.resolved_server_address() else {
            self.set_error("No server address selected.");
            return;
        };
        let wine_debug_override = if self.prefs.developer.enable_verbose_wine_debug {
            Some(VERBOSE_WINE_DEBUG.to_string())
        } else {
            None
        };
        let request = GameLaunchRequest {
            game_dir: game_dir.clone(),
            lobby_host: server_address.clone(),
            session_id,
            wine_debug_override,
            enable_winsock_proxy: self.prefs.developer.enable_winsock_tracing,
        };
        match crate::launcher::launch_game(&request) {
            Ok(()) => self.set_info(format!("Launched ffxivgame.exe against {server_address}.")),
            Err(e) => self.set_error(format!("Failed to launch game: {e}")),
        }
    }

    fn launch_with_dev_session_id(&mut self) {
        let session_id = self.dev_session_id.trim().to_string();
        if session_id.len() != crate::crypto::SESSION_ID_LEN {
            self.set_error(format!(
                "Dev session ID must be {} characters.",
                crate::crypto::SESSION_ID_LEN,
            ));
            return;
        }
        self.save_preferences();
        self.launch_game_with_session(session_id);
    }

    fn poll_login_task(&mut self) {
        let outcome = match self.login_task.as_mut() {
            Some(task) => task.try_recv(),
            None => return,
        };
        let Some(outcome) = outcome else {
            return;
        };
        // Drop the task (kills the child if still running + joins reader thread).
        self.login_task = None;
        match outcome {
            LoginOutcome::Success(session_id) => {
                self.set_info("Login complete; launching game…");
                self.launch_game_with_session(session_id);
            }
            LoginOutcome::Cancelled => {
                self.set_info("Login window closed without completing.");
            }
            LoginOutcome::Error(msg) => {
                self.set_error(format!("Login failed: {msg}"));
            }
        }
    }

    fn render_main(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.heading(APP_NAME);
        ui.horizontal(|ui| {
            ui.label(format!("Version {APP_VERSION}"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Developer Settings…").clicked() {
                    self.developer_modal = Some(DeveloperModal::new(&self.prefs.developer));
                }
                if ui.button("Game Settings…").clicked() {
                    let current = self.prefs.launcher.game_location.clone();
                    self.settings_modal = Some(SettingsModal::new(current.as_ref()));
                }
            });
        });
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Server:");
            let selected_label = self
                .selected_server_name
                .as_deref()
                .unwrap_or("(none)")
                .to_string();
            egui::ComboBox::from_id_source("server-select")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    for server in self.servers.iter() {
                        ui.selectable_value(
                            &mut self.selected_server_name,
                            Some(server.name.clone()),
                            format!("{} ({})", server.address, server.name),
                        );
                    }
                });
        });

        ui.horizontal(|ui| {
            ui.label("Custom address:");
            ui.text_edit_singleline(&mut self.manual_server_address);
        });

        ui.separator();

        ui.horizontal_wrapped(|ui| {
            ui.label("Game location:");
            let shown = self
                .resolved_game_location()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(not set)".to_string());
            ui.monospace(shown);
        });
        if let Some(path) = self.resolved_game_location() {
            let up_to_date = check_game_version(&path);
            let (label, color) = if up_to_date {
                (
                    "game.ver matches expected version",
                    egui::Color32::LIGHT_GREEN,
                )
            } else {
                (
                    "game.ver missing or outdated — use Check for Updates",
                    egui::Color32::YELLOW,
                )
            };
            ui.colored_label(color, label);
        }

        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("Check for Updates").clicked() {
                self.start_update();
            }
            if ui.button("Install from Local Patches…").clicked() {
                self.start_local_install();
            }
            let login_in_flight = self.login_task.is_some();
            let launch_button = egui::Button::new(if login_in_flight {
                "Login in progress…"
            } else {
                "Launch"
            });
            if ui.add_enabled(!login_in_flight, launch_button).clicked() {
                self.launch_via_login();
            }
            if login_in_flight && ui.button("Cancel Login").clicked() {
                if let Some(task) = self.login_task.as_mut() {
                    task.cancel();
                }
                self.login_task = None;
                self.set_info("Login cancelled.");
            }
        });

        ui.add_space(8.0);
        ui.collapsing("Developer: launch with a manual session id", |ui| {
            ui.label(format!(
                "Paste a {}-character session ID to bypass the webview login (useful for testing).",
                crate::crypto::SESSION_ID_LEN
            ));
            ui.add(
                egui::TextEdit::singleline(&mut self.dev_session_id)
                    .desired_width(ui.available_width()),
            );
            if ui.button("Launch with this session ID").clicked() {
                self.launch_with_dev_session_id();
            }
        });

        ui.separator();
        if let Some((kind, msg)) = self.last_message.clone() {
            let color = match kind {
                MessageKind::Info => egui::Color32::LIGHT_BLUE,
                MessageKind::Error => egui::Color32::LIGHT_RED,
            };
            ui.colored_label(color, msg);
        }

        if let Some(modal) = self.settings_modal.as_mut() {
            match modal.render(ctx) {
                SettingsOutcome::Open => {}
                SettingsOutcome::Cancelled => {
                    self.settings_modal = None;
                }
                SettingsOutcome::Accepted(new_location) => {
                    self.prefs.launcher.game_location = new_location;
                    self.save_preferences();
                    self.settings_modal = None;
                }
            }
        }

        if let Some(modal) = self.developer_modal.as_mut() {
            match modal.render(ctx) {
                DeveloperOutcome::Open => {}
                DeveloperOutcome::Cancelled => {
                    self.developer_modal = None;
                }
                DeveloperOutcome::Accepted(new_prefs) => {
                    self.prefs.developer = new_prefs;
                    self.save_preferences();
                    self.developer_modal = None;
                }
            }
        }

        self.render_outdated_prompt(ctx);
        self.render_download_confirm(ctx);
    }

    fn render_outdated_prompt(&mut self, ctx: &egui::Context) {
        if !self.outdated_prompt_open {
            return;
        }
        let mut window_open = true;
        let mut dismiss = false;
        let mut accept = false;
        egui::Window::new("Outdated Game Installation")
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .open(&mut window_open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Your game install appears to be out of date.\nWould you like to update it to version {FFXIV_GAME_VERSION}?"
                ));
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Update").clicked() {
                        accept = true;
                    }
                    if ui.button("Not now").clicked() {
                        dismiss = true;
                    }
                });
            });
        if !window_open || dismiss {
            self.outdated_prompt_open = false;
        }
        if accept {
            self.outdated_prompt_open = false;
            self.start_update();
        }
    }

    fn render_download_confirm(&mut self, ctx: &egui::Context) {
        let Some(state) = self.download_confirm.as_mut() else {
            return;
        };
        let mut window_open = true;
        let mut proceed = false;
        let mut choose_dir = false;
        let mut cancel = false;
        let gigabytes = total_bytes() as f64 / 1_000_000_000.0;
        egui::Window::new("Patch Download Location")
            .collapsible(false)
            .resizable(false)
            .open(&mut window_open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "The launcher needs to download approximately {gigabytes:.2} GB of data\n\
                     to update the game. Files will be stored at:"
                ));
                ui.monospace(state.download_dir.display().to_string());
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Continue").clicked() {
                        proceed = true;
                    }
                    if ui.button("Choose different location…").clicked() {
                        choose_dir = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if choose_dir {
            if let Some(folder) = rfd::FileDialog::new()
                .set_title("Specify patch download location")
                .pick_folder()
            {
                state.download_dir = folder;
            }
            return;
        }

        if !window_open || cancel {
            self.download_confirm = None;
            return;
        }

        if proceed {
            let dir = state.download_dir.clone();
            self.download_confirm = None;
            self.kick_off_patcher(dir);
        }
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_login_task();

        let in_patcher = matches!(self.screen, Screen::Patcher(_));
        let mut dismiss_patcher = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            if in_patcher {
                if let Screen::Patcher(screen) = &mut self.screen {
                    dismiss_patcher = screen.render(ui);
                    if !screen.is_terminal() {
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                }
            } else {
                self.render_main(ctx, ui);
            }
        });

        // Keep polling while a login subprocess is in flight.
        if self.login_task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        if dismiss_patcher {
            self.screen = Screen::Main;
            self.set_info("Update complete.");
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_preferences();
    }
}
