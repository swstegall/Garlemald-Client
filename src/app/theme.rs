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

//! Status colors that stay readable in both light and dark visuals.

use eframe::egui;

/// Success/up-to-date status text.
pub fn success(visuals: &egui::Visuals) -> egui::Color32 {
    if visuals.dark_mode {
        egui::Color32::LIGHT_GREEN
    } else {
        egui::Color32::from_rgb(0x1a, 0x7f, 0x37)
    }
}

/// Informational status text.
pub fn info(visuals: &egui::Visuals) -> egui::Color32 {
    if visuals.dark_mode {
        egui::Color32::LIGHT_BLUE
    } else {
        egui::Color32::from_rgb(0x0b, 0x5c, 0xad)
    }
}

/// Warning status text. egui's light-theme warn orange sits at ~2.8:1
/// contrast on the near-white window fill - below the 4.5:1 AA floor -
/// so light mode gets a hand-tuned dark amber instead.
pub fn warn(visuals: &egui::Visuals) -> egui::Color32 {
    if visuals.dark_mode {
        visuals.warn_fg_color
    } else {
        egui::Color32::from_rgb(0x9a, 0x67, 0x00)
    }
}

/// Error status text. egui's light-theme pure red is ~3.8:1 on the
/// near-white fill; light mode gets a darker red that clears AA.
pub fn error(visuals: &egui::Visuals) -> egui::Color32 {
    if visuals.dark_mode {
        visuals.error_fg_color
    } else {
        egui::Color32::from_rgb(0xcf, 0x22, 0x2e)
    }
}
