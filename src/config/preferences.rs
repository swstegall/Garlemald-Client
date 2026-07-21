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

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default)]
    pub launcher: LauncherPreferences,
    #[serde(default)]
    pub developer: DeveloperPreferences,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherPreferences {
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub server_address: String,
    #[serde(default)]
    pub game_location: Option<PathBuf>,
    #[serde(default)]
    pub wine_runtime_dir: Option<PathBuf>,
    #[serde(default)]
    pub patch_download_dir: Option<PathBuf>,
    /// Seed the patch torrent while the launcher is open. Opt-out:
    /// defaults to `true` both for a missing field (older files) and a
    /// missing section.
    #[serde(default = "default_seed_patches")]
    pub seed_patches: bool,
}

fn default_seed_patches() -> bool {
    true
}

impl Default for LauncherPreferences {
    fn default() -> Self {
        Self {
            server_name: Default::default(),
            server_address: Default::default(),
            game_location: Default::default(),
            wine_runtime_dir: Default::default(),
            patch_download_dir: Default::default(),
            seed_patches: true,
        }
    }
}

/// Developer-only knobs surfaced in the Developer Settings dialog. Off by
/// default — enabling any of these trades runtime speed or log-file size
/// for visibility into the login / Now-Loading handshake.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeveloperPreferences {
    /// When true, overrides the default `WINEDEBUG` channel set with a
    /// verbose selection that covers DLL loads, winsock calls, structured
    /// exceptions, and thread ids. Wine's own output still lands in
    /// `<data_dir>/logs/wine.log`; this flag just raises the ceiling.
    #[serde(default)]
    pub enable_verbose_wine_debug: bool,
    /// When true, deploys our `ws2_32.dll` hijack proxy into the game
    /// folder before launch. The proxy tees every `send`/`recv` /
    /// `WSASend`/`WSARecv`/`connect`/`closesocket` call the 1.23b client
    /// makes to `<game_dir>/ws2_32-trace.log`. Everything else forwards
    /// transparently to a renamed copy of the real DLL
    /// (`<game_dir>/ws2_32_real.dll`). Off: the proxy + the copy are
    /// removed on launch so the stock loader path is restored.
    #[serde(default)]
    pub enable_winsock_tracing: bool,
}

impl Preferences {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading preferences file {}", path.display()))?;
        let prefs: Self = toml::from_str(&text)
            .with_context(|| format!("parsing preferences file {}", path.display()))?;
        Ok(prefs)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing preferences")?;
        fs::write(path, text)
            .with_context(|| format!("writing preferences file {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preferences() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prefs.toml");
        let mut prefs = Preferences::default();
        prefs.launcher.server_name = "Van Darnus Server".to_string();
        prefs.launcher.server_address = "vandarnus.seventhumbral.org".to_string();
        prefs.launcher.game_location = Some(PathBuf::from("/tmp/ffxiv"));
        prefs.save(&path).unwrap();

        let loaded = Preferences::load(&path).unwrap();
        assert_eq!(loaded.launcher.server_name, prefs.launcher.server_name);
        assert_eq!(
            loaded.launcher.server_address,
            prefs.launcher.server_address
        );
        assert_eq!(loaded.launcher.game_location, prefs.launcher.game_location);
    }

    #[test]
    fn missing_file_yields_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prefs.toml");
        let prefs = Preferences::load(&path).unwrap();
        assert!(prefs.launcher.server_name.is_empty());
    }

    #[test]
    fn seed_patches_defaults_to_true() {
        let tmp = tempfile::tempdir().unwrap();
        let empty_path = tmp.path().join("empty.toml");
        fs::write(&empty_path, "").unwrap();
        let empty = Preferences::load(&empty_path).unwrap();
        assert!(empty.launcher.seed_patches);

        // A [launcher] section written before the field existed.
        let old_path = tmp.path().join("old.toml");
        fs::write(
            &old_path,
            r#"
                [launcher]
                server_name = "Van Darnus Server"
            "#,
        )
        .unwrap();
        let old = Preferences::load(&old_path).unwrap();
        assert!(old.launcher.seed_patches);
    }

    #[test]
    fn seed_patches_opt_out_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prefs.toml");
        let mut prefs = Preferences::default();
        prefs.launcher.seed_patches = false;
        prefs.save(&path).unwrap();

        let loaded = Preferences::load(&path).unwrap();
        assert!(!loaded.launcher.seed_patches);
    }
}
