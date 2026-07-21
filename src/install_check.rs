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

//! The install gate: is there a real FFXIV 1.x install, and is it 1.23b?
//!
//! Login (and launch) are blocked until this reports [`InstallState::Ready`].
//! "Found" means `ffxivboot.exe` exists - the SeventhUmbral validity rule.
//! A base 1.0 install has no `ffxivgame.exe`; that binary only exists after
//! the patch chain has been applied, so "Ready" additionally requires it
//! alongside a `game.ver` that records the final 1.23b build (the same
//! launch-readiness shape EchoGate's ClientInstallReport uses).

use std::path::{Path, PathBuf};

use crate::patcher::check_game_version;

/// Present in every 1.x install from the base installer onward; its absence
/// means the directory is not an FFXIV install at all.
const BOOT_EXE: &str = "ffxivboot.exe";

/// Only exists once the patch chain has run; required to launch.
const CLIENT_EXE: &str = "ffxivgame.exe";

/// Where the install stands relative to the 1.23b launch requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    /// No game directory is configured/detected, or it lacks `ffxivboot.exe`.
    NotFound,
    /// A real install, but not at the target 1.23b build.
    FoundNeedsPatch {
        /// Contents of `game.ver` if present (e.g. the base install's
        /// `2010.07.10.0000`), for the UI's "version X - patches required".
        game_version: Option<String>,
    },
    /// `game.ver` records the 1.23b build and `ffxivgame.exe` exists.
    Ready,
}

/// The gate verdict plus the directory it was computed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallStatus {
    pub state: InstallState,
    pub game_dir: Option<PathBuf>,
}

/// Evaluate the install gate for an optional resolved game directory.
pub fn check_install(game_dir: Option<&Path>) -> InstallStatus {
    let Some(dir) = game_dir else {
        return InstallStatus {
            state: InstallState::NotFound,
            game_dir: None,
        };
    };
    if !dir.join(BOOT_EXE).is_file() {
        return InstallStatus {
            state: InstallState::NotFound,
            game_dir: Some(dir.to_path_buf()),
        };
    }
    let state = if check_game_version(dir) && dir.join(CLIENT_EXE).is_file() {
        InstallState::Ready
    } else {
        InstallState::FoundNeedsPatch {
            game_version: read_game_version(dir),
        }
    };
    InstallStatus {
        state,
        game_dir: Some(dir.to_path_buf()),
    }
}

/// The trimmed contents of `<game>/game.ver`, if readable.
fn read_game_version(game_dir: &Path) -> Option<String> {
    std::fs::read_to_string(game_dir.join("game.ver"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::FFXIV_GAME_VERSION;
    use std::fs;

    #[test]
    fn no_dir_is_not_found() {
        let status = check_install(None);
        assert_eq!(status.state, InstallState::NotFound);
        assert_eq!(status.game_dir, None);
    }

    #[test]
    fn missing_dir_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("nope");
        let status = check_install(Some(&gone));
        assert_eq!(status.state, InstallState::NotFound);
    }

    #[test]
    fn dir_without_boot_exe_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let status = check_install(Some(tmp.path()));
        assert_eq!(status.state, InstallState::NotFound);
        assert_eq!(status.game_dir.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn base_install_needs_patch_and_reports_its_version() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ffxivboot.exe"), b"x").unwrap();
        fs::write(tmp.path().join("game.ver"), "2010.07.10.0000\n").unwrap();
        let status = check_install(Some(tmp.path()));
        assert_eq!(
            status.state,
            InstallState::FoundNeedsPatch {
                game_version: Some("2010.07.10.0000".into())
            }
        );
    }

    #[test]
    fn boot_exe_without_game_ver_needs_patch() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ffxivboot.exe"), b"x").unwrap();
        let status = check_install(Some(tmp.path()));
        assert_eq!(
            status.state,
            InstallState::FoundNeedsPatch { game_version: None }
        );
    }

    #[test]
    fn target_game_ver_without_client_exe_still_needs_patch() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ffxivboot.exe"), b"x").unwrap();
        fs::write(tmp.path().join("game.ver"), FFXIV_GAME_VERSION).unwrap();
        let status = check_install(Some(tmp.path()));
        assert!(matches!(status.state, InstallState::FoundNeedsPatch { .. }));
    }

    #[test]
    fn patched_install_is_ready() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ffxivboot.exe"), b"x").unwrap();
        fs::write(tmp.path().join("ffxivgame.exe"), b"x").unwrap();
        fs::write(tmp.path().join("game.ver"), FFXIV_GAME_VERSION).unwrap();
        let status = check_install(Some(tmp.path()));
        assert_eq!(status.state, InstallState::Ready);
    }
}
