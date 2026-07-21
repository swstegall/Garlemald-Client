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

//! Byte-progress counter and cancel flag shared between the patcher worker
//! and the UI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Live progress snapshot for a single transfer pass. Cheap to clone — the
/// internal counters are shared via `Arc`s so UI threads can observe them
/// while a worker thread runs the transfer.
#[derive(Clone)]
pub struct TransferProgress {
    pub bytes_transferred: Arc<AtomicU64>,
    pub cancel_flag: Arc<AtomicBool>,
}

impl TransferProgress {
    pub fn new() -> Self {
        Self {
            bytes_transferred: Arc::new(AtomicU64::new(0)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    pub fn bytes(&self) -> u64 {
        self.bytes_transferred.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::Relaxed)
    }
}

impl Default for TransferProgress {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_before_download_is_sticky() {
        let d = TransferProgress::new();
        d.cancel();
        assert!(d.cancel_flag.load(Ordering::Relaxed));
    }
}
