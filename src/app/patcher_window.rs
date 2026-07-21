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

//! Patcher screen — mirrors `SeventhUmbral/launcher/PatcherWindow.cpp`. Two
//! progress bars (extract/validate + apply), status labels, Cancel/Close
//! button. The worker runs on a background thread; this module only polls
//! shared state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Instant;

use eframe::egui;

use crate::app::theme;
use crate::patcher::manifest::PATCH_MANIFEST;
use crate::patcher::{PatchSource, PatcherShared, Phase, start_patcher_worker};

pub struct PatcherScreen {
    shared: Arc<PatcherShared>,
    // Worker handle retained so the thread isn't instantly joined-on-drop; we
    // let it finish in the background after cancel and ignore the result.
    _worker: JoinHandle<()>,
    cancel_pending: bool,
    last_sample: Instant,
    last_sampled_bytes: u64,
    smoothed_rate_bytes_per_sec: f64,
}

impl PatcherScreen {
    pub fn start(game_dir: PathBuf, source: PatchSource) -> Self {
        let shared = PatcherShared::new();
        let worker = start_patcher_worker(shared.clone(), game_dir, source);
        Self {
            shared,
            _worker: worker,
            cancel_pending: false,
            last_sample: Instant::now(),
            last_sampled_bytes: 0,
            smoothed_rate_bytes_per_sec: 0.0,
        }
    }

    pub fn phase(&self) -> Phase {
        self.shared.phase()
    }

    pub fn is_terminal(&self) -> bool {
        self.shared.is_terminal()
    }

    /// The worker's fatal error message, once it has failed.
    pub fn error(&self) -> Option<String> {
        self.shared.error()
    }

    /// EWMA transfer-rate estimate, updated roughly every 250ms.
    fn update_rate_estimate(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();
        if elapsed < 0.25 {
            return;
        }
        let total_bytes = self.total_transferred_bytes();
        let delta = total_bytes.saturating_sub(self.last_sampled_bytes);
        let instantaneous = delta as f64 / elapsed;
        // 0.3 weight on the new sample: responsive but not jittery.
        self.smoothed_rate_bytes_per_sec =
            0.7 * self.smoothed_rate_bytes_per_sec + 0.3 * instantaneous;
        self.last_sample = now;
        self.last_sampled_bytes = total_bytes;
    }

    fn total_transferred_bytes(&self) -> u64 {
        let completed = self.shared.previous_completed_bytes.load(Ordering::Acquire);
        let current = self.shared.transfer.bytes();
        completed + current
    }

    /// Renders the screen. Returns `true` when the user has dismissed the
    /// screen (clicked "Close" after completion); the caller should then
    /// transition back to the main menu.
    pub fn render(&mut self, ui: &mut egui::Ui) -> bool {
        self.update_rate_estimate();

        let phase = self.phase();

        ui.heading("Game Update");
        ui.separator();

        self.render_progress_section(ui, phase);
        ui.add_space(12.0);
        self.render_patch_section(ui, phase);

        ui.separator();

        let error = self.shared.error();
        let warnings = self.shared.warnings();

        if let Some(message) = &error {
            ui.colored_label(theme::error(ui.visuals()), message);
        }
        if !warnings.is_empty() {
            ui.collapsing(format!("Warnings ({})", warnings.len()), |ui| {
                for w in &warnings {
                    ui.label(w);
                }
            });
        }

        ui.separator();
        self.render_button_row(ui, phase)
    }

    fn render_progress_section(&self, ui: &mut egui::Ui, phase: Phase) {
        let total_bytes = self.shared.total_source_bytes.max(1);
        let transferred = self.total_transferred_bytes();
        let fraction = (transferred as f64 / total_bytes as f64).clamp(0.0, 1.0);

        let status = match phase {
            Phase::Starting => "Starting…".to_string(),
            Phase::Validating | Phase::Extracting => {
                let idx = self.shared.file_idx.load(Ordering::Acquire);
                let entry = PATCH_MANIFEST.get(idx);
                let current = self.shared.transfer.bytes();
                let verb = match phase {
                    Phase::Validating => "Validating",
                    Phase::Extracting => "Extracting",
                    _ => unreachable!("only Validating/Extracting reach this arm"),
                };
                match entry {
                    Some(entry) => {
                        let name = leaf_name(entry.path);
                        format!(
                            "{verb} {name} ({}/{}) @ {}/s",
                            format_kb(current),
                            format_kb(entry.size),
                            format_kb(self.smoothed_rate_bytes_per_sec as u64),
                        )
                    }
                    None => format!("{verb}…"),
                }
            }
            Phase::Patching | Phase::Done => "Patches ready.".to_string(),
            Phase::Error => "Interrupted by error.".to_string(),
            Phase::Cancelled => "Cancelled.".to_string(),
        };

        ui.label(status);
        let bar = egui::ProgressBar::new(fraction as f32)
            .text(format!("{:.0}%", fraction * 100.0))
            .desired_width(ui.available_width());
        ui.add(bar);
    }

    fn render_patch_section(&self, ui: &mut egui::Ui, phase: Phase) {
        let total = self.shared.total_patches.max(1);
        let idx = self.shared.patch_idx.load(Ordering::Acquire);
        let fraction = match phase {
            Phase::Starting | Phase::Validating | Phase::Extracting => 0.0,
            Phase::Patching => (idx as f64 / total as f64).clamp(0.0, 1.0),
            Phase::Done => 1.0,
            Phase::Error | Phase::Cancelled => (idx as f64 / total as f64).clamp(0.0, 1.0),
        };

        let status = match phase {
            Phase::Starting => "Patcher waiting for patches to be prepared…".to_string(),
            Phase::Validating => "Patcher waiting for local patches to be validated…".to_string(),
            Phase::Extracting => "Patcher waiting for patches to be extracted…".to_string(),
            Phase::Patching => PATCH_MANIFEST
                .get(idx)
                .map(|entry| format!("Applying {}…", leaf_name(entry.path)))
                .unwrap_or_else(|| "Applying patch…".to_string()),
            Phase::Done => "Complete!".to_string(),
            Phase::Error => "Patch interrupted by error.".to_string(),
            Phase::Cancelled => "Patch cancelled.".to_string(),
        };

        ui.label(status);
        let bar = egui::ProgressBar::new(fraction as f32)
            .text(format!("{:.0}%", fraction * 100.0))
            .desired_width(ui.available_width());
        ui.add(bar);
    }

    fn render_button_row(&mut self, ui: &mut egui::Ui, _phase: Phase) -> bool {
        let mut dismiss = false;
        ui.horizontal(|ui| {
            if self.is_terminal() {
                if ui.button("Close").clicked() {
                    dismiss = true;
                }
            } else if self.cancel_pending {
                ui.label("Cancelling, please wait…");
            } else if ui.button("Cancel").clicked() {
                self.shared.request_cancel();
                self.cancel_pending = true;
            }
        });
        dismiss
    }
}

fn format_kb(bytes: u64) -> String {
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

fn leaf_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
