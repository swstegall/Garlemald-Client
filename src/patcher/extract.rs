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

//! Locating and unpacking the torrented `ffxiv_patches.zip` payload.
//!
//! The torrent drops a `ffxiv_patches/` folder holding the zip plus a
//! few tiny metadata files. [`find_patch_payload`] figures out whether
//! a storage directory already holds the raw `*.patch` files (an
//! earlier extraction, or a hand-assembled cache) or still needs
//! unpacking from the archive. [`extract_manifest_patches`] does the
//! unpacking into a scratch staging directory, streaming through the
//! same [`IO_CHUNK`] buffer and progress counters the validate pass
//! uses, so [`super::worker`] can drive it with the same UI progress
//! bar.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use super::manifest::PATCH_MANIFEST;
use super::worker::PatcherShared;

/// Byte size of the read/write buffer used while streaming a zip entry
/// to disk; matches the validate pass's chunk size so progress
/// reporting stays consistent across phases.
const IO_CHUNK: usize = 0x10000;

/// Filename of a manifest entry's path, ignoring any directory
/// components. Mirrors the sibling helper of the same name in
/// `process.rs` and `worker.rs`.
fn leaf_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// How many missing filenames to spell out before collapsing the rest
/// into a count. Matches process.rs's `MissingPatches` error shape.
const MAX_NAMES_IN_ERROR: usize = 4;

/// How many directory levels under the storage root [`find_patch_payload`]
/// will descend while looking for the patch zip.
const MAX_SEARCH_DEPTH: u32 = 4;

/// Where [`find_patch_payload`] located the patch data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchPayload {
    /// The archive itself; needs [`extract_manifest_patches`] before the
    /// patches can be applied.
    Zip(PathBuf),
    /// A directory that already holds every manifest patch as a loose
    /// `*.patch` file.
    PatchDir(PathBuf),
}

/// Failures from unpacking the patch archive.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("patch archive is not a file: {0}")]
    NotAZip(PathBuf),

    #[error("i/o error while {context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("could not read the patch archive: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("{missing_count} patch file(s) not found in the archive: {summary}")]
    MissingPatches {
        missing_count: usize,
        summary: String,
    },

    #[error("extraction was cancelled")]
    Cancelled,
}

impl ExtractError {
    fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }
}

/// Figures out where the patch data lives under `storage_dir`: a
/// directory that already has every manifest patch loose on disk, the
/// still-zipped `ffxiv_patches.zip`, or (as a fallback for a
/// differently-named download) the sole `*.zip` file found nearby.
/// Returns `None` when `storage_dir` does not exist or neither shape is
/// found.
pub fn find_patch_payload(storage_dir: &Path) -> Option<PatchPayload> {
    if !storage_dir.is_dir() {
        return None;
    }
    if has_every_manifest_patch(storage_dir) {
        return Some(PatchPayload::PatchDir(storage_dir.to_path_buf()));
    }
    find_patch_zip(storage_dir).map(PatchPayload::Zip)
}

/// True when every [`PATCH_MANIFEST`] leaf exists as a loose `*.patch`
/// file somewhere under `dir`. A plain recursive walk in the same spirit
/// as process.rs's `index_patches_by_leaf`, duplicated here since that
/// helper is private to the `process` module. Capped at
/// [`MAX_SEARCH_DEPTH`] and skipping hidden directories like the zip
/// search below: the storage root can be something as big as the user's
/// whole Documents folder, and an unbounded walk there can take minutes.
fn has_every_manifest_patch(dir: &Path) -> bool {
    let mut missing: HashSet<&str> = PATCH_MANIFEST.iter().map(|e| leaf_name(e.path)).collect();
    let mut pending = vec![(dir.to_path_buf(), 0u32)];
    while let Some((current, depth)) = pending.pop() {
        if depth > MAX_SEARCH_DEPTH {
            continue;
        }
        let Ok(listing) = fs::read_dir(&current) else {
            continue;
        };
        for item in listing.flatten() {
            let path = item.path();
            if path.is_dir() {
                if !is_hidden(&path) {
                    pending.push((path, depth + 1));
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("patch")
                && let Some(leaf) = path.file_name().and_then(|n| n.to_str())
            {
                missing.remove(leaf);
            }
        }
    }
    missing.is_empty()
}

/// Searches under `dir` for the patch zip: `ffxiv_patches.zip` (any
/// case) by preference, or the sole `*.zip` file found if there is
/// exactly one.
fn find_patch_zip(dir: &Path) -> Option<PathBuf> {
    let zips = collect_zip_files(dir, 0);
    if let Some(named) = zips.iter().find(|p| is_named_ffxiv_patches_zip(p)) {
        return Some(named.clone());
    }
    match zips.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

fn is_named_ffxiv_patches_zip(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("ffxiv_patches.zip"))
}

/// Recursively collects every `*.zip` file under `dir`, capped at
/// [`MAX_SEARCH_DEPTH`] levels and skipping hidden (dotfile) directories.
fn collect_zip_files(dir: &Path, depth: u32) -> Vec<PathBuf> {
    if depth > MAX_SEARCH_DEPTH {
        return Vec::new();
    }
    let Ok(listing) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for item in listing.flatten() {
        let path = item.path();
        if path.is_dir() {
            if is_hidden(&path) {
                continue;
            }
            found.extend(collect_zip_files(&path, depth + 1));
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        {
            found.push(path);
        }
    }
    found
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// Unpacks every [`PATCH_MANIFEST`] entry from `zip_path` into a fresh
/// `staging_dir`, reporting progress through `shared`'s download
/// counters exactly as the download and validate passes do, so the same
/// progress bar keeps working. Checks `shared.is_cancel_requested()`
/// between chunks and at each file boundary.
pub fn extract_manifest_patches(
    zip_path: &Path,
    staging_dir: &Path,
    shared: &PatcherShared,
) -> Result<(), ExtractError> {
    if !zip_path.is_file() {
        return Err(ExtractError::NotAZip(zip_path.to_path_buf()));
    }

    match fs::remove_dir_all(staging_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(ExtractError::io("clearing the staging directory", e)),
    }
    fs::create_dir_all(staging_dir)
        .map_err(|e| ExtractError::io("creating the staging directory", e))?;

    let file =
        File::open(zip_path).map_err(|e| ExtractError::io("opening the patch archive", e))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file))?;

    // Index by leaf filename rather than the archive's own internal
    // paths, matching the leaf-based lookup process.rs uses for a loose
    // patch directory: robust to whatever nesting the zip was built with.
    let mut index_by_leaf: HashMap<String, usize> = HashMap::with_capacity(archive.len());
    for (i, name) in archive.file_names().enumerate() {
        if let Some(leaf) = Path::new(name).file_name().and_then(|n| n.to_str()) {
            index_by_leaf.entry(leaf.to_owned()).or_insert(i);
        }
    }

    let mut missing = Vec::new();
    for entry in PATCH_MANIFEST {
        let leaf = leaf_name(entry.path);
        if !index_by_leaf.contains_key(leaf) {
            missing.push(leaf.to_owned());
        }
    }
    if !missing.is_empty() {
        return Err(ExtractError::MissingPatches {
            missing_count: missing.len(),
            summary: join_names(&missing),
        });
    }

    for (idx, entry) in PATCH_MANIFEST.iter().enumerate() {
        if shared.is_cancel_requested() {
            return Err(ExtractError::Cancelled);
        }
        shared.file_idx.store(idx, Ordering::Release);
        // Restart the per-file counter; previous_completed_bytes carries
        // the running total of files already staged.
        shared.transfer.bytes_transferred.store(0, Ordering::SeqCst);

        let leaf = leaf_name(entry.path);
        log::info!(
            "extract: staging {leaf} ({}/{})",
            idx + 1,
            PATCH_MANIFEST.len()
        );
        let zip_index = index_by_leaf[leaf];
        let dst_path = staging_dir.join(leaf);
        extract_one(&mut archive, zip_index, &dst_path, shared)?;

        shared
            .previous_completed_bytes
            .fetch_add(entry.size, Ordering::Release);
    }

    Ok(())
}

/// Streams zip entry `zip_index` to `dst_path` in [`IO_CHUNK`] chunks,
/// feeding `shared`'s byte counter and checking for a cancel between
/// chunks. Mirrors the read loop in `super::worker::validate_file`.
fn extract_one<R: Read + io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    zip_index: usize,
    dst_path: &Path,
    shared: &PatcherShared,
) -> Result<(), ExtractError> {
    let mut entry = archive.by_index(zip_index)?;
    let mut sink =
        File::create(dst_path).map_err(|e| ExtractError::io("creating a staged patch file", e))?;

    let mut buf = [0u8; IO_CHUNK];
    loop {
        if shared.is_cancel_requested() {
            drop(sink);
            let _ = fs::remove_file(dst_path);
            return Err(ExtractError::Cancelled);
        }
        let read = entry
            .read(&mut buf)
            .map_err(|e| ExtractError::io("reading a patch entry from the archive", e))?;
        if read == 0 {
            break;
        }
        sink.write_all(&buf[..read])
            .map_err(|e| ExtractError::io("writing a staged patch file", e))?;
        shared
            .transfer
            .bytes_transferred
            .fetch_add(read as u64, Ordering::Relaxed);
    }
    Ok(())
}

/// Renders a short, friendly list of missing names for the error text.
/// Matches process.rs's `summarize_names`.
fn join_names(names: &[String]) -> String {
    if names.len() <= MAX_NAMES_IN_ERROR {
        return names.join(", ");
    }
    let shown = names[..MAX_NAMES_IN_ERROR].join(", ");
    format!("{shown}, and {} more", names.len() - MAX_NAMES_IN_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a fixture zip at `dst`: every manifest entry at its real
    /// relative path (tiny fake contents) except the indices in `skip`,
    /// plus one unrelated entry - close enough to the real archive's
    /// layout to exercise the leaf-based matching.
    fn build_fixture_zip(dst: &Path, skip: &[usize]) {
        let file = File::create(dst).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (i, entry) in PATCH_MANIFEST.iter().enumerate() {
            if skip.contains(&i) {
                continue;
            }
            zip.start_file(entry.path, options).unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.start_file("readme.txt", options).unwrap();
        zip.write_all(b"not a patch").unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn full_extraction_stages_every_manifest_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("ffxiv_patches.zip");
        build_fixture_zip(&zip_path, &[]);
        let staging = tmp.path().join("staging");
        let shared = PatcherShared::new();

        extract_manifest_patches(&zip_path, &staging, &shared).unwrap();

        for entry in PATCH_MANIFEST {
            assert!(
                staging.join(leaf_name(entry.path)).exists(),
                "missing {} after extraction",
                leaf_name(entry.path)
            );
        }
    }

    #[test]
    fn a_zip_missing_leaves_reports_the_count() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("ffxiv_patches.zip");
        build_fixture_zip(&zip_path, &[0, 1, 2]);
        let staging = tmp.path().join("staging");
        let shared = PatcherShared::new();

        match extract_manifest_patches(&zip_path, &staging, &shared).unwrap_err() {
            ExtractError::MissingPatches { missing_count, .. } => {
                assert_eq!(missing_count, 3);
            }
            other => panic!("expected MissingPatches, got {other:?}"),
        }
    }

    #[test]
    fn a_preset_cancel_flag_stops_extraction() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("ffxiv_patches.zip");
        build_fixture_zip(&zip_path, &[]);
        let staging = tmp.path().join("staging");
        let shared = PatcherShared::new();
        shared.request_cancel();

        assert!(matches!(
            extract_manifest_patches(&zip_path, &staging, &shared).unwrap_err(),
            ExtractError::Cancelled
        ));
    }

    #[test]
    fn find_patch_payload_prefers_a_dir_with_every_loose_patch() {
        let tmp = tempfile::tempdir().unwrap();
        for entry in PATCH_MANIFEST {
            fs::write(tmp.path().join(leaf_name(entry.path)), b"x").unwrap();
        }
        assert_eq!(
            find_patch_payload(tmp.path()),
            Some(PatchPayload::PatchDir(tmp.path().to_path_buf()))
        );
    }

    #[test]
    fn find_patch_payload_finds_the_named_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("ffxiv_patches.zip");
        build_fixture_zip(&zip_path, &[]);
        assert_eq!(
            find_patch_payload(tmp.path()),
            Some(PatchPayload::Zip(zip_path))
        );
    }

    #[test]
    fn find_patch_payload_falls_back_to_a_lone_differently_named_zip() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("patches-mirror.zip");
        build_fixture_zip(&zip_path, &[]);
        assert_eq!(
            find_patch_payload(tmp.path()),
            Some(PatchPayload::Zip(zip_path))
        );
    }

    #[test]
    fn find_patch_payload_is_none_for_an_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(find_patch_payload(tmp.path()), None);
    }
}
