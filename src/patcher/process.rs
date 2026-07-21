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

//! Port of `PatchProcess.cpp` helpers: version-file checks and post-patch
//! version-file writes. The actual driver loop (validate the patch set,
//! then apply it in sorted order) lives in `super::worker` since it needs
//! to report progress to the user.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::version::{FFXIV_BOOT_VERSION, FFXIV_GAME_VERSION};

pub struct PatchPlan {
    /// Absolute paths to the resolved patch files, in application order
    /// (sorted by filename leaf so chronologically-later patches apply later).
    pub patches_in_order: Vec<PathBuf>,
}

impl PatchPlan {
    /// Builds a plan from a user-chosen local directory by scanning
    /// recursively for each manifest entry's leaf filename. Missing files
    /// return an error listing what was not found; the caller is expected to
    /// validate sizes/CRCs separately.
    pub fn from_local_source(source_dir: &Path) -> Result<Self> {
        let index = index_patches_by_leaf(source_dir)?;
        let mut paths = Vec::with_capacity(crate::patcher::manifest::PATCH_MANIFEST.len());
        let mut missing = Vec::new();
        for entry in crate::patcher::manifest::PATCH_MANIFEST {
            let leaf = leaf_name(entry.path);
            match index.get(leaf) {
                Some(p) => paths.push(p.clone()),
                None => missing.push(leaf.to_string()),
            }
        }
        if !missing.is_empty() {
            return Err(anyhow!(
                "{} patch file(s) missing from {}: {}",
                missing.len(),
                source_dir.display(),
                summarize_names(&missing),
            ));
        }
        paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        Ok(Self {
            patches_in_order: paths,
        })
    }
}

fn leaf_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn summarize_names(names: &[String]) -> String {
    const MAX_SHOWN: usize = 4;
    if names.len() <= MAX_SHOWN {
        return names.join(", ");
    }
    let head = names[..MAX_SHOWN].join(", ");
    format!("{head}, … ({} more)", names.len() - MAX_SHOWN)
}

/// Walks `source_dir` recursively, returning a map of leaf filename → full
/// path for every `*.patch` file. If two files share the same leaf, the first
/// one encountered wins; the alternative is to fail, but in practice users
/// aren't expected to have duplicates in a single install.
fn index_patches_by_leaf(source_dir: &Path) -> Result<HashMap<String, PathBuf>> {
    if !source_dir.is_dir() {
        return Err(anyhow!(
            "local patch source is not a directory: {}",
            source_dir.display()
        ));
    }
    let mut out = HashMap::new();
    let mut stack = vec![source_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("iterating {}", dir.display()))?;
            let ty = entry
                .file_type()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            if ty.is_dir() {
                stack.push(entry.path());
            } else if ty.is_file() {
                let path = entry.path();
                let is_patch = path.extension().and_then(|e| e.to_str()) == Some("patch");
                if let (true, Some(name)) = (
                    is_patch,
                    path.file_name().and_then(|n| n.to_str()).map(String::from),
                ) {
                    out.entry(name).or_insert(path);
                }
            }
        }
    }
    Ok(out)
}

/// Returns `true` when the game's on-disk `game.ver` matches our expected version.
pub fn check_game_version(game_location: &Path) -> bool {
    let ver_path = game_location.join("game.ver");
    match fs::read_to_string(&ver_path) {
        Ok(text) => text.trim() == FFXIV_GAME_VERSION,
        Err(_) => false,
    }
}

/// Writes `boot.ver` and `game.ver` to `game_location` to mark it as updated.
pub fn write_version_files(game_location: &Path) -> Result<()> {
    let boot = game_location.join("boot.ver");
    let game = game_location.join("game.ver");
    fs::write(&boot, FFXIV_BOOT_VERSION).with_context(|| format!("writing {}", boot.display()))?;
    fs::write(&game, FFXIV_GAME_VERSION).with_context(|| format!("writing {}", game.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_ver_file_is_out_of_date() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!check_game_version(tmp.path()));
    }

    #[test]
    fn matching_ver_file_is_up_to_date() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("game.ver"), FFXIV_GAME_VERSION).unwrap();
        assert!(check_game_version(tmp.path()));
    }

    #[test]
    fn write_version_files_creates_both() {
        let tmp = tempfile::tempdir().unwrap();
        write_version_files(tmp.path()).unwrap();
        assert!(tmp.path().join("boot.ver").exists());
        assert!(tmp.path().join("game.ver").exists());
    }

    // Guards the sequential-apply invariant: patches always apply in
    // filename-leaf order, oldest patch first.
    #[test]
    fn plan_sorts_by_leaf_name() {
        let tmp = tempfile::tempdir().unwrap();
        for entry in crate::patcher::manifest::PATCH_MANIFEST {
            let leaf = entry.path.rsplit('/').next().unwrap();
            fs::write(tmp.path().join(leaf), b"").unwrap();
        }
        let plan = PatchPlan::from_local_source(tmp.path()).unwrap();
        let leaves: Vec<_> = plan
            .patches_in_order
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let mut sorted = leaves.clone();
        sorted.sort();
        assert_eq!(leaves, sorted);
        assert_eq!(leaves.len(), crate::patcher::manifest::PATCH_MANIFEST.len());
    }

    #[test]
    fn local_plan_finds_patches_in_nested_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        // Drop every manifest patch into an arbitrary nested subdir to make
        // sure the recursive scan finds them regardless of layout.
        let sub = tmp.path().join("some/deeply/nested/dir");
        fs::create_dir_all(&sub).unwrap();
        for entry in crate::patcher::manifest::PATCH_MANIFEST {
            let leaf = entry.path.rsplit('/').next().unwrap();
            fs::write(sub.join(leaf), b"").unwrap();
        }
        let plan = PatchPlan::from_local_source(tmp.path()).unwrap();
        assert_eq!(
            plan.patches_in_order.len(),
            crate::patcher::manifest::PATCH_MANIFEST.len()
        );
    }

    #[test]
    fn local_plan_errors_when_patches_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = PatchPlan::from_local_source(tmp.path());
        let err = match result {
            Ok(_) => panic!("expected error for empty directory"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(msg.contains("missing"), "got {msg}");
    }
}
