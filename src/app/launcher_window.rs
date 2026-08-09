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
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;

use crate::app::developer_window::{DeveloperModal, DeveloperOutcome, VERBOSE_WINE_DEBUG};
use crate::app::native_login_window::{NativeLoginModal, NativeLoginOutcome};
use crate::app::patcher_window::PatcherScreen;
use crate::app::settings_window::{SettingsModal, SettingsOutcome};
use crate::app::theme;
use crate::config::{
    Preferences, bundled_config_path, default_torrent_storage_dir, preferences_file_path,
    torrent_session_dir,
};
use crate::install_check::{self, InstallState, InstallStatus};
use crate::launcher::GameLaunchRequest;
use crate::login::{LoginOutcome, LoginTask};
use crate::patcher::{PatchPayload, PatchSource, Phase, check_game_version, find_patch_payload};
use crate::platform::{Platform, current as current_platform};
use crate::servers::ServerDefinitions;
use crate::torrent::{TORRENT_ENDPOINT, TorrentService, TorrentServiceError, TorrentState};
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
    native_login_modal: Option<NativeLoginModal>,
    outdated_prompt_open: bool,
    last_message: Option<(MessageKind, String)>,
    torrent: TorrentService,
    install_status: InstallStatus,
    last_install_check: Instant,
    torrent_apply_attempted: bool,
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

        let persistence_dir = torrent_session_dir().unwrap_or_else(|e| {
            log::warn!(
                "could not resolve the torrent session dir, falling back to a temp dir: {e}"
            );
            std::env::temp_dir().join("garlemald-torrent-session")
        });
        let torrent = TorrentService::new(persistence_dir);
        // The service's seeding flag is always synced from the preference
        // first so a download started THIS run also honors a persisted
        // opt-out (the flag defaults to true inside the service). Resuming
        // seeding of an already-downloaded payload from a previous run is
        // never fatal: a fresh install has nothing to seed yet, and the
        // service itself no-ops when nothing was ever downloaded.
        torrent.set_seeding_enabled(prefs.launcher.seed_patches);
        if prefs.launcher.seed_patches {
            let storage_dir = prefs
                .launcher
                .patch_download_dir
                .clone()
                .or_else(|| default_torrent_storage_dir().ok());
            if let Some(dir) = storage_dir {
                torrent.ensure_seeding_if_complete(dir);
            }
        }

        let resolved_location = prefs
            .launcher
            .game_location
            .clone()
            .or_else(|| detected_install.clone());
        let install_status = install_check::check_install(resolved_location.as_deref());

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
            native_login_modal: None,
            outdated_prompt_open: false,
            last_message: None,
            torrent,
            install_status,
            last_install_check: Instant::now(),
            torrent_apply_attempted: false,
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

    fn resolved_login_url(&self) -> Option<String> {
        let name = self.selected_server_name.as_ref()?;
        let def = self.servers.get(name)?;
        if def.login_url.is_empty() {
            None
        } else {
            Some(def.login_url.clone())
        }
    }

    fn resolved_api_url(&self) -> Option<String> {
        let name = self.selected_server_name.as_ref()?;
        let def = self.servers.get(name)?;
        if def.api_url.is_empty() {
            None
        } else {
            Some(def.api_url.clone())
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

    /// Resolves the effective patch-storage directory: a
    /// `patch_download_dir` preference override wins over the platform
    /// default (the user's Documents folder).
    fn resolved_patch_storage_dir(&self) -> Option<PathBuf> {
        self.prefs
            .launcher
            .patch_download_dir
            .clone()
            .or_else(|| default_torrent_storage_dir().ok())
    }

    fn refresh_install_status(&mut self) {
        self.install_status =
            install_check::check_install(self.resolved_game_location().as_deref());
        self.last_install_check = Instant::now();
        // The boot-time "your install is outdated" prompt answers a question
        // that no longer exists once the gate reports Ready (the auto-chain
        // or a manual apply patched the install behind it); leaving it open
        // would offer a redundant full re-patch.
        if self.install_ready() {
            self.outdated_prompt_open = false;
        }
    }

    fn install_ready(&self) -> bool {
        matches!(self.install_status.state, InstallState::Ready)
    }

    /// Explains why login is blocked, mirrored next to the disabled Launch
    /// button and used verbatim by the hard-block in the launch methods.
    /// Empty once the gate is satisfied.
    fn install_gate_message(&self) -> String {
        match &self.install_status.state {
            InstallState::NotFound => {
                "Game install not found - set the game location in Game Settings.".to_string()
            }
            // game.ver alone can read 1.23b while the patched client binary
            // is missing (an interrupted or hand-damaged install); telling
            // that user their 1.23b client needs "the 1.23b patches" reads
            // as a contradiction, so name the real problem instead.
            InstallState::FoundNeedsPatch {
                game_version: Some(v),
            } if v == FFXIV_GAME_VERSION => {
                "Client files are incomplete (ffxivgame.exe is missing) - re-apply the patches before logging in."
                    .to_string()
            }
            InstallState::FoundNeedsPatch {
                game_version: Some(v),
            } => format!("Client is at {v} - apply the 1.23b patches before logging in."),
            InstallState::FoundNeedsPatch { game_version: None } => {
                "Client is not 1.23b - apply the patches before logging in.".to_string()
            }
            InstallState::Ready => String::new(),
        }
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

    /// Kicks off the patcher against a resolved local patch source (either a
    /// patch-cache directory or the extracted/zipped torrent payload).
    /// Validates sizes + CRCs before applying.
    fn kick_off_local_patcher(&mut self, source: PatchSource) {
        let Some(game_dir) = self.resolved_game_location() else {
            self.set_error("No game location set.");
            return;
        };
        self.screen = Screen::Patcher(PatcherScreen::start(game_dir, source));
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
        let Some(picked) = rfd::FileDialog::new()
            .set_title("Select folder containing existing FFXIV 1.x patches")
            .pick_folder()
        else {
            return;
        };

        // PatchDir IS the picked directory by construction; None falls
        // through so verification reports the detailed missing-files error.
        let source = match find_patch_payload(&picked) {
            Some(PatchPayload::Zip(zip_path)) => PatchSource::LocalZip { zip_path },
            _ => PatchSource::Local { source_dir: picked },
        };
        self.kick_off_local_patcher(source);
    }

    /// Starts (or resumes) downloading the patch torrent into the resolved
    /// storage directory.
    fn start_torrent_download(&mut self) {
        let Some(dir) = self.resolved_patch_storage_dir() else {
            self.set_error("Could not resolve a patch storage directory.");
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.set_error(format!(
                "Could not create patch storage directory {}: {e}",
                dir.display()
            ));
            return;
        }
        self.torrent_apply_attempted = false;
        match self
            .torrent
            .start_download(TORRENT_ENDPOINT.to_string(), dir)
        {
            Ok(()) => self.set_info("Fetching the patch torrent..."),
            Err(TorrentServiceError::Busy) => self.set_info(
                "A patch download is already running; patches apply automatically when it finishes.",
            ),
        }
    }

    /// Applies the already-downloaded torrent payload, locating either the
    /// extracted patch directory or the payload zip via
    /// [`find_patch_payload`].
    fn kick_off_torrent_apply(&mut self) {
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

        // Narrowest first: the torrent's own content folder, then the live
        // session's output root, then the configured storage dir. The wider
        // roots can be something as large as the user's whole Documents
        // folder, where even a capped scan is slow.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = self.torrent.payload_dir() {
            candidates.push(dir);
        }
        if let Some(dir) = self.torrent.session_output_dir()
            && !candidates.contains(&dir)
        {
            candidates.push(dir);
        }
        if let Some(dir) = self.resolved_patch_storage_dir()
            && !candidates.contains(&dir)
        {
            candidates.push(dir);
        }

        let source = match candidates.iter().find_map(|dir| find_patch_payload(dir)) {
            Some(PatchPayload::Zip(zip_path)) => PatchSource::LocalZip { zip_path },
            Some(PatchPayload::PatchDir(source_dir)) => PatchSource::Local { source_dir },
            None => {
                self.set_error(format!(
                    "No downloaded patches were found in {}. Download them first.",
                    candidates
                        .first()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| "the storage directory".to_string())
                ));
                return;
            }
        };

        self.screen = Screen::Patcher(PatcherScreen::start(game_dir, source));
    }

    /// Renders the torrent-download panel: its own block between the main
    /// action row and the developer collapsing section.
    fn render_torrent_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        let snapshot = self.torrent.snapshot();
        match snapshot.state {
            TorrentState::Idle => {
                ui.horizontal(|ui| {
                    if ui.button("Download Patches via Torrent").clicked() {
                        self.start_torrent_download();
                    }
                    if self.install_ready() {
                        ui.small("Game is up to date - downloading only helps seed.");
                    }
                });
            }
            TorrentState::Resolving => {
                ui.horizontal(|ui| {
                    ui.label("Fetching the patch torrent...");
                    if ui.button("Cancel").clicked() {
                        self.torrent.stop();
                    }
                });
            }
            TorrentState::Downloading => {
                let total = snapshot.total_bytes.max(1);
                let fraction = (snapshot.progress_bytes as f64 / total as f64).clamp(0.0, 1.0);
                ui.label(format!(
                    "Downloading patches: {} / {} ({:.1}%) - {}/s - {} peers",
                    format_bytes(snapshot.progress_bytes),
                    format_bytes(snapshot.total_bytes),
                    fraction * 100.0,
                    format_bytes(snapshot.download_bps),
                    snapshot.peers,
                ));
                let bar = egui::ProgressBar::new(fraction as f32)
                    .text(format!("{:.1}%", fraction * 100.0))
                    .desired_width(ui.available_width());
                ui.add(bar);
                if ui.button("Cancel").clicked() {
                    self.torrent.stop();
                    self.set_info("Torrent download stopped.");
                }
            }
            TorrentState::Complete => {
                ui.colored_label(theme::success(ui.visuals()), "Patch archive downloaded.");
                if !self.install_ready() && ui.button("Apply Downloaded Patches").clicked() {
                    self.kick_off_torrent_apply();
                }
            }
            TorrentState::Seeding => {
                ui.colored_label(
                    theme::success(ui.visuals()),
                    format!(
                        "Seeding - {} peers - {} uploaded",
                        snapshot.peers,
                        format_bytes(snapshot.uploaded_bytes),
                    ),
                );
                if !self.install_ready() && ui.button("Apply Downloaded Patches").clicked() {
                    self.kick_off_torrent_apply();
                }
            }
            TorrentState::Stopped => {
                ui.colored_label(theme::warn(ui.visuals()), "Torrent download stopped.");
                if ui.button("Resume Download").clicked() {
                    self.start_torrent_download();
                }
            }
            TorrentState::Error => {
                let message = snapshot
                    .error
                    .clone()
                    .unwrap_or_else(|| "Torrent download failed.".to_string());
                ui.colored_label(theme::error(ui.visuals()), message);
                if ui.button("Retry Download").clicked() {
                    self.start_torrent_download();
                }
            }
        }
    }

    fn launch_via_login(&mut self) {
        // Defense in depth: the Launch button is already disabled while the
        // install gate isn't Ready, but re-check here in case state went
        // stale between the last poll and this click.
        self.refresh_install_status();
        if !self.install_ready() {
            self.set_error(self.install_gate_message());
            return;
        }
        if self.login_task.is_some() || self.native_login_modal.is_some() {
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
            // No login webview supplied: fall back to the native JSON
            // login/sign-up form when the server exposes an auth API.
            if let Some(api_url) = self.resolved_api_url() {
                let server_name = self
                    .selected_server_name
                    .clone()
                    .unwrap_or_else(|| "server".into());
                self.save_preferences();
                self.native_login_modal = Some(NativeLoginModal::new(server_name, api_url));
                return;
            }
            self.set_error(
                "Selected server has no login URL or auth API. Use the developer session-id override.",
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
        // Defense in depth: mirrors the same gate check in launch_via_login.
        self.refresh_install_status();
        if !self.install_ready() {
            self.set_error(self.install_gate_message());
            return;
        }
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
                    let current_game = self.prefs.launcher.game_location.clone();
                    self.settings_modal = Some(SettingsModal::new(
                        current_game.as_ref(),
                        self.prefs.launcher.patch_download_dir.as_ref(),
                        self.prefs.launcher.seed_patches,
                    ));
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
        if self.resolved_game_location().is_some() {
            // Derived from the cached install gate rather than re-reading
            // game.ver from disk: with the torrent repaint cadence this
            // renders at ~2 Hz for as long as the launcher seeds.
            let up_to_date = self.install_ready();
            let label = if up_to_date {
                "game.ver matches expected version"
            } else {
                "install not fully patched"
            };
            let color = if up_to_date {
                theme::success(ui.visuals())
            } else {
                theme::warn(ui.visuals())
            };
            ui.colored_label(color, label);
        }

        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("Install from Local Patches…").clicked() {
                self.start_local_install();
            }
            let login_in_flight = self.login_task.is_some() || self.native_login_modal.is_some();
            let launch_button = egui::Button::new(if login_in_flight {
                "Login in progress…"
            } else {
                "Launch"
            });
            if ui
                .add_enabled(!login_in_flight && self.install_ready(), launch_button)
                .clicked()
            {
                self.launch_via_login();
            }
            if login_in_flight && ui.button("Cancel Login").clicked() {
                if let Some(task) = self.login_task.as_mut() {
                    task.cancel();
                }
                self.login_task = None;
                self.native_login_modal = None;
                self.set_info("Login cancelled.");
            }
        });
        if !self.install_ready() {
            ui.colored_label(theme::warn(ui.visuals()), self.install_gate_message());
        }

        self.render_torrent_section(ui);

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
                MessageKind::Info => theme::info(ui.visuals()),
                MessageKind::Error => theme::error(ui.visuals()),
            };
            ui.colored_label(color, msg);
        }

        if let Some(modal) = self.settings_modal.as_mut() {
            match modal.render(ctx) {
                SettingsOutcome::Open => {}
                SettingsOutcome::Cancelled => {
                    self.settings_modal = None;
                }
                SettingsOutcome::Accepted(values) => {
                    self.prefs.launcher.game_location = values.game_location;

                    let storage_changed =
                        values.patch_download_dir != self.prefs.launcher.patch_download_dir;
                    if storage_changed
                        && matches!(
                            self.torrent.snapshot().state,
                            TorrentState::Resolving | TorrentState::Downloading
                        )
                    {
                        // Repointing the storage folder mid-download would strand
                        // the in-flight bytes at the old root (the session's
                        // output dir is fixed at creation); make the user cancel
                        // first. The other fields still apply below.
                        self.set_error(
                            "Cancel the in-progress patch download before changing the patch storage folder.",
                        );
                    } else {
                        self.prefs.launcher.patch_download_dir = values.patch_download_dir;
                        // A session that already exists keeps the output
                        // root it was created with; only a session started
                        // after this point (usually the next launcher run)
                        // picks up the new folder.
                        if storage_changed && self.torrent.session_output_dir().is_some() {
                            self.set_info(
                                "Patch storage folder saved. The live torrent session keeps its \
                                 current folder; the new one applies from the next launcher start.",
                            );
                        }
                    }

                    self.prefs.launcher.seed_patches = values.seed_patches;
                    self.torrent.set_seeding_enabled(values.seed_patches);
                    self.save_preferences();
                    self.refresh_install_status();
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

        if let Some(modal) = self.native_login_modal.as_mut() {
            match modal.render(ctx) {
                NativeLoginOutcome::Open => {}
                NativeLoginOutcome::Cancelled => {
                    self.native_login_modal = None;
                    self.set_info("Login cancelled.");
                }
                NativeLoginOutcome::Success(session_id) => {
                    self.native_login_modal = None;
                    self.set_info("Login complete; launching game…");
                    self.launch_game_with_session(session_id);
                }
            }
        }

        self.render_outdated_prompt(ctx);
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
                ui.label(
                    "Your game install appears to be out of date.\nDownload the 1.23b patches via BitTorrent (about 6 GB) and apply them automatically?",
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Download and Update").clicked() {
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
            self.start_torrent_download();
        }
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_login_task();

        let on_main_screen = matches!(self.screen, Screen::Main);
        if on_main_screen && self.last_install_check.elapsed() >= Duration::from_secs(3) {
            self.refresh_install_status();
        }

        // Auto-chain: once the torrent payload lands while the install
        // still needs patching, kick off the apply without waiting for the
        // user to notice and click through. Mirrors bahamut-launcher's
        // maybeAutoChainPatch install gate.
        let no_modal_open = self.settings_modal.is_none()
            && self.developer_modal.is_none()
            && self.native_login_modal.is_none();
        if on_main_screen
            && no_modal_open
            && !self.torrent_apply_attempted
            && matches!(
                self.install_status.state,
                InstallState::FoundNeedsPatch { .. }
            )
            && matches!(
                self.torrent.snapshot().state,
                TorrentState::Complete | TorrentState::Seeding
            )
        {
            self.torrent_apply_attempted = true;
            // The apply answers the boot-time "update your install?" prompt;
            // close it so it cannot linger behind the patcher screen and
            // reappear over the freshly patched install.
            self.outdated_prompt_open = false;
            log::info!("torrent payload ready; starting patch apply");
            self.kick_off_torrent_apply();
        }

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

        // Keep polling while a torrent download/seed session is live so the
        // progress bar and byte counters actually move.
        if !matches!(
            self.torrent.snapshot().state,
            TorrentState::Idle | TorrentState::Stopped
        ) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        if dismiss_patcher {
            // The Close button is reachable from every terminal phase, so
            // the farewell message must match how the run actually ended.
            let (phase, error) = match &self.screen {
                Screen::Patcher(screen) => (screen.phase(), screen.error()),
                Screen::Main => (Phase::Done, None),
            };
            self.screen = Screen::Main;
            match phase {
                Phase::Error => {
                    let detail = error.unwrap_or_else(|| "see the log for details".to_string());
                    self.set_error(format!("Update failed: {detail}"));
                }
                Phase::Cancelled => self.set_info("Update cancelled."),
                _ => self.set_info("Update complete."),
            }
            self.refresh_install_status();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_preferences();
    }
}

/// Local byte formatter mirroring `patcher_window::format_kb`'s shape but
/// keyed off the torrent snapshot's raw byte counts.
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else {
        format!("{:.0} KB", b / KB)
    }
}
