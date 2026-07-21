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

//! The patcher pipeline: the patch manifest (sizes + CRCs, `manifest`), the
//! torrented-archive extraction (`extract`), the plan builder that lays out
//! patches in apply order (`process`), the shared byte-progress counter
//! (`progress`), and the worker that validates a patch source (a local
//! directory or an extracted torrented archive) then applies it in order
//! (`worker`).

mod extract;
pub mod manifest;
mod process;
mod progress;
mod worker;

pub use extract::{PatchPayload, find_patch_payload};
pub use manifest::{PATCH_MANIFEST, PATCH_URL_BASE, PatchEntry};
pub use process::{PatchPlan, check_game_version, write_version_files};
pub use progress::TransferProgress;
pub use worker::{PatchSource, PatcherShared, Phase, start_patcher_worker};
