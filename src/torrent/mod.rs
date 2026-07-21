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

//! BitTorrent transport for the 1.x patch archive: fetches the magnet
//! link from the distribution endpoint and drives a librqbit session
//! that downloads and (opt-out) seeds the payload.

pub mod endpoint;
pub mod service;

pub use endpoint::{MagnetFetchError, TORRENT_ENDPOINT, fetch_magnet};
pub use service::{TorrentService, TorrentServiceError, TorrentSnapshot, TorrentState};
