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

//! Background driver for the patcher phase. Runs on its own thread so the
//! egui UI stays responsive; the UI polls [`PatcherShared`] each frame.

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

use crc32fast::Hasher as Crc32Hasher;
use parking_lot::Mutex;

use super::extract::{ExtractError, extract_manifest_patches};
use super::manifest::{PATCH_MANIFEST, PatchEntry, total_bytes};
use super::process::{PatchPlan, write_version_files};
use super::progress::TransferProgress;

/// Where the worker should get the patch files from: either a local patch
/// directory (typically another install's patch cache, or one populated by
/// "Install from Local Patches...") or a torrented archive already unpacked
/// on disk, which just needs validating + applying.
#[derive(Debug)]
pub enum PatchSource {
    Local {
        source_dir: PathBuf,
    },
    /// Unpack the manifest patches from a torrented archive, then apply.
    LocalZip {
        zip_path: PathBuf,
    },
}

/// High-level phase reported by the worker. Serialized as `u8` into an
/// atomic so UI observers can read it without locking.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting = 0,
    Patching = 2,
    Done = 3,
    Error = 4,
    Cancelled = 5,
    Validating = 6,
    Extracting = 7,
}

impl Phase {
    fn from_u8(value: u8) -> Self {
        match value {
            2 => Phase::Patching,
            3 => Phase::Done,
            4 => Phase::Error,
            5 => Phase::Cancelled,
            6 => Phase::Validating,
            7 => Phase::Extracting,
            _ => Phase::Starting,
        }
    }
}

/// State shared between the patcher thread and the UI. Progress fields use
/// atomics so the UI can poll them lock-free each frame; the string fields
/// (error + warnings) are protected by a lightweight mutex.
pub struct PatcherShared {
    pub transfer: TransferProgress,
    pub file_idx: AtomicUsize,
    pub previous_completed_bytes: AtomicU64,
    pub patch_idx: AtomicUsize,
    phase: AtomicU8,
    error_message: Mutex<Option<String>>,
    warnings: Mutex<Vec<String>>,
    pub total_source_bytes: u64,
    pub total_patches: usize,
}

impl PatcherShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            transfer: TransferProgress::new(),
            file_idx: AtomicUsize::new(0),
            previous_completed_bytes: AtomicU64::new(0),
            patch_idx: AtomicUsize::new(0),
            phase: AtomicU8::new(Phase::Starting as u8),
            error_message: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
            total_source_bytes: total_bytes(),
            total_patches: PATCH_MANIFEST.len(),
        })
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }

    pub fn error(&self) -> Option<String> {
        self.error_message.lock().clone()
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().clone()
    }

    pub fn request_cancel(&self) {
        self.transfer.cancel();
    }

    pub fn is_cancel_requested(&self) -> bool {
        self.transfer.is_cancelled()
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.phase(), Phase::Done | Phase::Error | Phase::Cancelled)
    }

    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    fn set_error(&self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("patcher: {message}");
        *self.error_message.lock() = Some(message);
        self.set_phase(Phase::Error);
    }

    fn push_warning(&self, message: impl Into<String>) {
        self.warnings.lock().push(message.into());
    }
}

/// Spawns the validate+apply worker (extracting a torrented archive first,
/// when the source is `LocalZip`). The returned [`JoinHandle`] lets the UI
/// detach or wait on the worker; typical use is to let it run and poll the
/// shared state for updates.
pub fn start_patcher_worker(
    shared: Arc<PatcherShared>,
    game_dir: PathBuf,
    source: PatchSource,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("garlemald-patcher".into())
        .spawn(move || run_patcher(shared, game_dir, source))
        .expect("failed to spawn patcher thread")
}

fn run_patcher(shared: Arc<PatcherShared>, game_dir: PathBuf, source: PatchSource) {
    log::info!("patcher: starting, source={source:?}");

    // Populated only by the LocalZip arm; its Drop impl removes the
    // staging directory once this function returns, on every exit path
    // (cancel, apply failure, or a clean finish) alike.
    let mut staging_cleanup = StagingCleanup {
        shared: shared.clone(),
        staging_dir: None,
    };

    let plan = match source {
        PatchSource::Local { source_dir } => match run_validate_phase(&shared, &source_dir) {
            Some(plan) => plan,
            None => return,
        },
        PatchSource::LocalZip { zip_path } => {
            match extract_and_verify(&shared, &zip_path, &mut staging_cleanup) {
                Some(plan) => plan,
                None => return,
            }
        }
    };

    shared.set_phase(Phase::Patching);
    log::info!("patcher: applying {} patches", plan.patches_in_order.len());

    for (idx, patch_path) in plan.patches_in_order.iter().enumerate() {
        if shared.is_cancel_requested() {
            shared.set_phase(Phase::Cancelled);
            return;
        }
        shared.patch_idx.store(idx, Ordering::Release);

        let leaf = patch_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| patch_path.display().to_string());
        log::info!(
            "patcher: applying {leaf} ({}/{})",
            idx + 1,
            plan.patches_in_order.len()
        );

        match crate::patch_format::apply_patch_file(patch_path, &game_dir) {
            Ok(result) => {
                for msg in result.messages {
                    shared.push_warning(msg);
                }
            }
            Err(e) => {
                shared.set_error(format!("Applying patch {leaf} failed: {e}"));
                return;
            }
        }
    }

    if let Err(e) = write_version_files(&game_dir) {
        shared.push_warning(format!("Failed to write version files: {e}"));
    }

    log::info!("patcher: all patches applied");
    shared.set_phase(Phase::Done);
}

/// RAII guard that best-effort removes the extraction staging directory
/// once `run_patcher` reaches a terminal phase, however it gets there
/// (cancel, a failed patch apply, or a clean finish). A no-op unless the
/// LocalZip arm populates `staging_dir`.
struct StagingCleanup {
    shared: Arc<PatcherShared>,
    staging_dir: Option<PathBuf>,
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        let Some(dir) = self.staging_dir.take() else {
            return;
        };
        if let Err(e) = fs::remove_dir_all(&dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            self.shared
                .push_warning(format!("Could not remove the patch staging directory: {e}"));
        }
    }
}

/// Unpacks the manifest patches from `zip_path` into a fresh staging
/// directory, then validates them exactly like [`run_validate_phase`] - the
/// LocalZip flow reuses the same plan builder and progress reporting a
/// user-supplied patch directory gets. Records the staging directory on
/// `cleanup` so [`StagingCleanup`] removes it once the run finishes.
/// Returns `None` (after setting the terminal phase) on cancel or any
/// extraction failure.
fn extract_and_verify(
    shared: &Arc<PatcherShared>,
    zip_path: &Path,
    cleanup: &mut StagingCleanup,
) -> Option<PatchPlan> {
    shared.set_phase(Phase::Extracting);

    let staging = match crate::config::patch_staging_dir() {
        Ok(dir) => dir,
        Err(e) => {
            shared.set_error(format!(
                "Could not resolve the patch staging directory: {e}"
            ));
            return None;
        }
    };
    cleanup.staging_dir = Some(staging.clone());

    if let Err(e) = extract_manifest_patches(zip_path, &staging, shared) {
        match e {
            ExtractError::Cancelled => shared.set_phase(Phase::Cancelled),
            other => shared.set_error(format!("Could not unpack the patch archive: {other}")),
        }
        return None;
    }

    // The extract pass already consumed the full total; restart the
    // counters so the Validating pass's progress bar starts clean.
    shared.previous_completed_bytes.store(0, Ordering::Release);
    shared.transfer.bytes_transferred.store(0, Ordering::SeqCst);

    run_validate_phase(shared, &staging)
}

/// Pre-apply validation phase: resolves each manifest entry to a file in
/// `source_dir`, verifies size + CRC32, and reports progress via the shared
/// progress atomics so the UI's existing bar can show it. Used directly for
/// a local patch directory, and via [`extract_and_verify`] for a torrented
/// archive already unpacked into a staging directory.
fn run_validate_phase(shared: &Arc<PatcherShared>, source_dir: &Path) -> Option<PatchPlan> {
    shared.set_phase(Phase::Validating);

    let plan = match PatchPlan::from_local_source(source_dir) {
        Ok(p) => p,
        Err(e) => {
            shared.set_error(format!("Local patch scan failed: {e}"));
            return None;
        }
    };

    // Pair each manifest entry to its resolved path (by leaf name). The plan
    // is already sorted by leaf; so is the manifest, but we don't rely on
    // that — look up each manifest entry's leaf in the plan.
    let pairs = pair_manifest_with_paths(&plan.patches_in_order);

    for (idx, (entry, path)) in pairs.iter().enumerate() {
        if shared.is_cancel_requested() {
            shared.set_phase(Phase::Cancelled);
            return None;
        }
        shared.file_idx.store(idx, Ordering::Release);
        // Reset the per-file counter so the UI shows "validated/size" for the
        // current file while `previous_completed_bytes` accumulates the rest.
        shared.transfer.bytes_transferred.store(0, Ordering::SeqCst);

        match validate_file(path, entry.size, entry.crc32, shared) {
            Ok(true) => {
                shared
                    .previous_completed_bytes
                    .fetch_add(entry.size, Ordering::Release);
            }
            Ok(false) => {
                shared.set_phase(Phase::Cancelled);
                return None;
            }
            Err(ValidateError::SizeMismatch { actual }) => {
                shared.set_error(format!(
                    "Local patch {} has wrong size ({} bytes, expected {})",
                    path.display(),
                    actual,
                    entry.size,
                ));
                return None;
            }
            Err(ValidateError::Crc32Mismatch) => {
                shared.set_error(format!("Local patch {} failed CRC32 check", path.display(),));
                return None;
            }
            Err(ValidateError::Io(e)) => {
                shared.set_error(format!(
                    "Reading local patch {} failed: {e}",
                    path.display(),
                ));
                return None;
            }
        }
    }

    Some(plan)
}

fn pair_manifest_with_paths(paths: &[PathBuf]) -> Vec<(&'static PatchEntry, PathBuf)> {
    let mut out = Vec::with_capacity(PATCH_MANIFEST.len());
    for entry in PATCH_MANIFEST {
        let leaf = leaf_name(entry.path);
        if let Some(p) = paths
            .iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(leaf))
        {
            out.push((entry, p.clone()));
        }
    }
    out
}

fn leaf_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

enum ValidateError {
    SizeMismatch { actual: u64 },
    Crc32Mismatch,
    Io(std::io::Error),
}

impl From<std::io::Error> for ValidateError {
    fn from(e: std::io::Error) -> Self {
        ValidateError::Io(e)
    }
}

/// Returns `Ok(true)` when the file matches the expected size + CRC32, or
/// `Ok(false)` when the worker was cancelled mid-read. Streams the file so we
/// can update the shared progress counter as we go.
fn validate_file(
    path: &Path,
    expected_size: u64,
    expected_crc32: u32,
    shared: &Arc<PatcherShared>,
) -> Result<bool, ValidateError> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() != expected_size {
        return Err(ValidateError::SizeMismatch {
            actual: metadata.len(),
        });
    }

    let mut reader = BufReader::new(file);
    let mut hasher = Crc32Hasher::new();
    let mut buf = [0u8; 0x10000];
    loop {
        if shared.is_cancel_requested() {
            return Ok(false);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        shared
            .transfer
            .bytes_transferred
            .fetch_add(n as u64, Ordering::Relaxed);
    }

    if hasher.finalize() != expected_crc32 {
        return Err(ValidateError::Crc32Mismatch);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_survives_a_u8_round_trip() {
        for phase in [
            Phase::Starting,
            Phase::Patching,
            Phase::Done,
            Phase::Error,
            Phase::Cancelled,
            Phase::Validating,
            Phase::Extracting,
        ] {
            assert_eq!(Phase::from_u8(phase as u8), phase);
        }
    }

    #[test]
    fn fresh_shared_is_in_starting_phase() {
        let shared = PatcherShared::new();
        assert_eq!(shared.phase(), Phase::Starting);
        assert!(!shared.is_terminal());
        assert!(shared.error().is_none());
    }

    #[test]
    fn cancel_request_reaches_the_progress_flag() {
        let shared = PatcherShared::new();
        assert!(!shared.is_cancel_requested());
        shared.request_cancel();
        assert!(shared.is_cancel_requested());
    }

    #[test]
    fn set_error_records_message_and_is_terminal() {
        let shared = PatcherShared::new();
        shared.set_error("boom");
        assert_eq!(shared.phase(), Phase::Error);
        assert_eq!(shared.error().as_deref(), Some("boom"));
        assert!(shared.is_terminal());
    }

    #[test]
    fn warnings_keep_their_order() {
        let shared = PatcherShared::new();
        shared.push_warning("first");
        shared.push_warning("second");
        assert_eq!(shared.warnings(), vec!["first", "second"]);
    }
}
