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

use anyhow::{Context, Result};
use directories::ProjectDirs;

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("me", "stegall", "garlemald-client")
        .context("could not resolve platform-specific project directories")
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
}

pub fn preferences_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("preferences.toml"))
}

pub fn servers_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("servers.xml"))
}

/// Path to the repo-bundled starter config (`./configs/garlemald-client.toml`
/// next to the binary). Used as a fallback when the per-user preferences
/// file doesn't exist yet — mirrors the server's `configs/*.toml` layout so
/// a fresh clone starts on the public Bahamut (main) server.
pub fn bundled_config_path() -> PathBuf {
    PathBuf::from("./configs/garlemald-client.toml")
}

/// Per-user cache directory. Used for regenerable data: the torrent
/// session-persistence folder and the patch extraction staging area.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().to_path_buf())
}

/// Default storage root for the torrented patch archive: the user's
/// Documents folder (the torrent adds its own `ffxiv_patches/` top-level
/// directory under it). Documents rather than the cache dir because the
/// payload doubles as long-lived seeding material the user may want to
/// keep, move, or reclaim by hand. Falls back to [`data_dir`] on the rare
/// platform setup with no resolvable Documents folder. Overridable via the
/// `patch_download_dir` preference.
pub fn default_torrent_storage_dir() -> Result<PathBuf> {
    match directories::UserDirs::new().and_then(|u| u.document_dir().map(|d| d.to_path_buf())) {
        Some(docs) => Ok(docs),
        None => data_dir(),
    }
}

/// librqbit session-persistence folder. Holds the fastresume state that
/// lets the app reboot into seeding without re-hashing the multi-gigabyte
/// payload. Regenerable, so it lives under the cache root.
pub fn torrent_session_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("torrent-session"))
}

/// Scratch directory the patcher extracts the torrented patch zip into
/// before verify + apply. Deleted after each run; regenerable from the
/// zip, so it lives under the cache root.
pub fn patch_staging_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("patch-staging"))
}
