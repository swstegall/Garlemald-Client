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

//! Game-settings modal — mirrors `GameSettingsWindow.cpp`. Editable game
//! location + "Browse…" button, patch storage folder + seeding opt-out,
//! OK/Cancel. Rendered as an `egui::Window` from the main launcher screen.

use std::path::PathBuf;

use eframe::egui;

/// The settings fields the OK button hands back to the caller in one shot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsValues {
    pub game_location: Option<PathBuf>,
    pub patch_download_dir: Option<PathBuf>,
    pub seed_patches: bool,
}

/// Outcome of rendering the modal once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsOutcome {
    Open,
    Accepted(SettingsValues),
    Cancelled,
}

pub struct SettingsModal {
    game_location_text: String,
    patch_download_dir_text: String,
    seed_patches: bool,
    // Resolved once at open so the hint can show the concrete default
    // path an empty storage field falls back to.
    storage_hint: String,
    pub open: bool,
}

impl SettingsModal {
    pub fn new(
        game_location: Option<&PathBuf>,
        patch_download_dir: Option<&PathBuf>,
        seed_patches: bool,
    ) -> Self {
        let storage_hint = match crate::config::default_torrent_storage_dir() {
            Ok(dir) => format!(
                "Empty = {} (patches/torrent are stored here).",
                dir.display()
            ),
            Err(_) => {
                "Empty = your Documents folder (patches/torrent are stored here).".to_string()
            }
        };
        Self {
            game_location_text: game_location
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            patch_download_dir_text: patch_download_dir
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            seed_patches,
            storage_hint,
            open: true,
        }
    }

    pub fn render(&mut self, ctx: &egui::Context) -> SettingsOutcome {
        let mut outcome = SettingsOutcome::Open;
        let mut open = self.open;
        egui::Window::new("Game Settings")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Game install location:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.game_location_text)
                            .desired_width(320.0),
                    );
                    if ui.button("Browse…").clicked()
                        && let Some(folder) = rfd::FileDialog::new()
                            .set_title("Specify FFXIV folder")
                            .pick_folder()
                    {
                        self.game_location_text = folder.display().to_string();
                    }
                });

                ui.separator();

                ui.label("Patch storage folder:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.patch_download_dir_text)
                            .desired_width(320.0),
                    );
                    if ui.button("Browse…").clicked()
                        && let Some(folder) = rfd::FileDialog::new()
                            .set_title("Specify patch storage folder")
                            .pick_folder()
                    {
                        self.patch_download_dir_text = folder.display().to_string();
                    }
                });
                ui.small(&self.storage_hint);

                ui.checkbox(
                    &mut self.seed_patches,
                    "Seed patches over BitTorrent while the launcher is open",
                );

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        let game_location = {
                            let trimmed = self.game_location_text.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(PathBuf::from(trimmed))
                            }
                        };
                        let patch_download_dir = {
                            let trimmed = self.patch_download_dir_text.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(PathBuf::from(trimmed))
                            }
                        };
                        outcome = SettingsOutcome::Accepted(SettingsValues {
                            game_location,
                            patch_download_dir,
                            seed_patches: self.seed_patches,
                        });
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = SettingsOutcome::Cancelled;
                    }
                });
            });
        if !open {
            outcome = SettingsOutcome::Cancelled;
        }
        self.open = matches!(outcome, SettingsOutcome::Open);
        outcome
    }
}
