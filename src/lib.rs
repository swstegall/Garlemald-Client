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

// The launcher must be built 32-bit on Windows: the PE patcher reads the
// suspended 32-bit `ffxivgame.exe` thread context (via GetThreadContext), which a
// 64-bit process cannot do, so a 64-bit launcher cannot patch or launch the game.
// Reject it at compile time rather than ship a non-functional x64 binary. Build
// with `--target i686-pc-windows-msvc` (a local .cargo/config.toml can pin it as
// the default — see docs/dev-environment.md). This never fires on macOS/Linux or
// on the i686 target.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
compile_error!(
    "garlemald-client must be built as 32-bit on Windows: use \
     `--target i686-pc-windows-msvc` (the x86_64 launcher cannot patch/launch the \
     32-bit ffxivgame.exe). See docs/dev-environment.md."
);

pub mod app;
pub mod config;
pub mod crypto;
pub mod install_check;
pub mod launcher;
pub mod login;
pub mod patch_format;
pub mod patcher;
pub mod platform;
pub mod servers;
pub mod torrent;
pub mod version;

use anyhow::Result;

pub fn run() -> Result<()> {
    app::run()
}
