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

//! macOS (Apple Silicon) platform backend.
//!
//! Manages its own Wine prefix and runtime under
//! `~/Library/Application Support/me.stegall.garlemald-client/`, downloading the
//! Sikarugir Wine engine + Frameworks on first launch. Interoperates
//! with externally-managed prefixes (e.g. the sibling
//! `xiv1point0-apple-silicon-installer`) by deriving `WINEPREFIX` from the
//! game-location path that the user has configured — our own Wine binary
//! plus their prefix works fine as long as both sides share the FFXIV 1.x
//! install layout.
//!
//! The actual process start is Wine's responsibility: we pre-patch a copy of
//! `ffxivgame.exe` on disk rather than trying to `WriteProcessMemory` across
//! the Wine boundary. See `crate::launcher::pe_patch`.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};

use crate::config;
use crate::crypto;
use crate::launcher::{
    GameLaunchRequest, apply_patches_on_disk, assert_log_patch, encryption_time_patch,
    lobby_host_patch, null_member8_write_nop_patch, null_this_guard_patch,
};
use crate::platform::Platform;
use crate::platform::wine::{
    PREFIX_FFXIV_SUBPATH, WineRuntime, copy_exe_for_patching, ensure_prefix_initialized,
    launch_ffxiv_game, monotonic_ms_since_boot,
};

const WRAPPER_VERSION: &str = "1.0.11";
const WRAPPER_URL: &str =
    "https://github.com/Sikarugir-App/Wrapper/releases/download/v1.0/Template-1.0.11.tar.xz";
// WineCX 23.7.1 rather than 24.0.7 or an upstream-based build: CX 24's mac
// driver fails to initialize on macOS 27, unpatched wined3d-GL engines render
// this title too slowly to play, and DXVK is blocked on macOS (winevulkan
// wow64 feature thunking + MoltenVK gaps), so the CX-patched D3D9 path is
// required.
const ENGINE_NAME: &str = "WS12WineCX23.7.1_4";
const ENGINE_URL: &str =
    "https://github.com/Sikarugir-App/Engines/releases/download/v1.0/WS12WineCX23.7.1_4.tar.xz";

pub struct MacosPlatform;

impl Default for MacosPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosPlatform {
    pub fn new() -> Self {
        Self
    }

    /// Path to the managed prefix root (`<dataDir>/prefix/`).
    fn managed_prefix_dir() -> Result<PathBuf> {
        Ok(config::data_dir()?.join("prefix"))
    }

    /// Path to the managed FFXIV install (`<managed-prefix>/drive_c/.../FFXIV`).
    fn managed_install_dir() -> Result<PathBuf> {
        Ok(Self::managed_prefix_dir()?.join(PREFIX_FFXIV_SUBPATH))
    }

    /// Path to the managed runtime root (`<dataDir>/runtime/`). The Sikarugir
    /// engine + Frameworks live here.
    fn runtime_root() -> Result<PathBuf> {
        Ok(config::data_dir()?.join("runtime"))
    }

    fn wine_bin() -> Result<PathBuf> {
        Ok(Self::runtime_root()?.join("wswine.bundle/bin/wine"))
    }

    fn wineserver_bin() -> Result<PathBuf> {
        Ok(Self::runtime_root()?.join("wswine.bundle/bin/wineserver"))
    }

    /// Builds the [`WineRuntime`] that should be used to launch a game at
    /// `game_dir`. Uses the prefix that actually contains `game_dir` if we
    /// can derive one; falls back to the managed prefix otherwise.
    fn runtime_for_game_dir(game_dir: &Path) -> Result<WineRuntime> {
        let prefix = derive_prefix_from_game_location(game_dir)
            .inspect(|p| {
                log::info!("using WINEPREFIX derived from game dir: {}", p.display());
            })
            .unwrap_or_else(|| Self::managed_prefix_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let runtime_root = Self::runtime_root()?;
        Ok(WineRuntime {
            root: config::data_dir()?,
            prefix,
            wine_bin: Self::wine_bin()?,
            wineserver_bin: Self::wineserver_bin()?,
            dyld_fallback_paths: vec![
                runtime_root.join("Frameworks"),
                // GStreamer dylibs live inside the framework at this path; without
                // it, `winegstreamer.so` fails to dlopen `libgstreamer-1.0.0.dylib`,
                // every `wg_parser_create` returns failure, and quartz can't build
                // a splitter for any media file.
                runtime_root.join("Frameworks/GStreamer.framework/Versions/Current/lib"),
                runtime_root.join("wswine.bundle/lib"),
                PathBuf::from("/usr/local/lib"),
                PathBuf::from("/usr/lib"),
            ],
            gst_plugin_path: Some(
                runtime_root
                    .join("Frameworks/GStreamer.framework/Versions/Current/lib/gstreamer-1.0"),
            ),
        })
    }
}

impl Platform for MacosPlatform {
    fn detect_game_install(&self) -> Option<PathBuf> {
        let managed = Self::managed_install_dir().ok()?;
        if self.is_valid_game_location(&managed) {
            Some(managed)
        } else {
            None
        }
    }

    fn launch_game(&self, request: &GameLaunchRequest) -> Result<()> {
        ensure_rosetta_available()?;
        ensure_runtime_downloaded()?;

        let runtime = Self::runtime_for_game_dir(&request.game_dir)?;
        ensure_prefix_initialized(&runtime)?;

        let tick = monotonic_ms_since_boot();
        log::debug!(
            "launcher tick_count = 0x{tick:08x} ({tick}), blowfish key = \"{:08x}\"",
            tick & !0xFFFF_u32
        );
        let launch_args = crypto::build_launch_arguments(&request.session_id, tick)?;

        let src_exe = request.game_dir.join("ffxivgame.exe");
        if !src_exe.exists() {
            return Err(anyhow!("ffxivgame.exe not found at {}", src_exe.display()));
        }
        let patched_exe = request.game_dir.join("ffxivgame.patched.exe");
        copy_exe_for_patching(&src_exe, &patched_exe)?;

        let patches = vec![
            encryption_time_patch(),
            lobby_host_patch(&request.lobby_host)?,
            assert_log_patch(),
            null_this_guard_patch(),
            null_member8_write_nop_patch(),
        ];
        apply_patches_on_disk(&patched_exe, &patches)?;

        launch_ffxiv_game(
            &runtime,
            &patched_exe,
            &launch_args.encoded_argument,
            request.wine_debug_override.as_deref(),
            request.enable_winsock_proxy,
            // DXVK auto-provisioning is Linux-only; the bundled engine
            // provides the D3D path on macOS.
            None,
        )?;
        Ok(())
    }
}

/// Walks up `game_dir` looking for a `drive_c` ancestor; the parent of
/// `drive_c` is the Wine prefix. Returns `None` if the path isn't shaped like
/// a Wine install — e.g. a raw Windows-side game folder.
fn derive_prefix_from_game_location(game_dir: &Path) -> Option<PathBuf> {
    let mut current = game_dir;
    while let Some(parent) = current.parent() {
        if current.file_name() == Some(OsStr::new("drive_c")) {
            return Some(parent.to_path_buf());
        }
        current = parent;
    }
    None
}

/// Checks that Rosetta 2 is available (Apple Silicon requires it to run the
/// x86_64 Wine engine). Returns a user-friendly error if it isn't — we don't
/// attempt to install it ourselves because that needs admin auth.
fn ensure_rosetta_available() -> Result<()> {
    if !is_apple_silicon() {
        return Ok(());
    }
    let status = Command::new("/usr/bin/arch")
        .arg("-x86_64")
        .arg("/usr/bin/true")
        .status()
        .context("running /usr/bin/arch to probe Rosetta 2")?;
    if !status.success() {
        return Err(anyhow!(
            "Rosetta 2 is required to run the x86_64 Wine engine on Apple Silicon.\n\
             Install it by running:\n    softwareupdate --install-rosetta --agree-to-license"
        ));
    }
    Ok(())
}

fn is_apple_silicon() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

/// Downloads the Sikarugir wrapper Frameworks + Wine engine when they are
/// missing, or when the version markers show a different version on disk
/// (the bundle paths are version-less, so marker files in the runtime root
/// record what is installed). Safe to call every launch — it's a fast path
/// when the runtime is current. A stale component is removed only after its
/// replacement archive has been downloaded and extracted, so a failed
/// download never destroys a working runtime.
pub fn ensure_runtime_downloaded() -> Result<()> {
    let runtime_root = MacosPlatform::runtime_root()?;
    let frameworks = runtime_root.join("Frameworks");
    let wswine_bundle = runtime_root.join("wswine.bundle");
    let wine_bin = MacosPlatform::wine_bin()?;

    fs::create_dir_all(&runtime_root)
        .with_context(|| format!("creating runtime dir {}", runtime_root.display()))?;

    let wrapper_marker = runtime_root.join("wrapper-version");
    if install_needed(
        frameworks.exists(),
        fs::read_to_string(&wrapper_marker).ok().as_deref(),
        WRAPPER_VERSION,
    ) {
        log::info!("downloading Sikarugir wrapper v{WRAPPER_VERSION} ({WRAPPER_URL})");
        let tmp = tempfile::tempdir().context("creating tmp dir for wrapper archive")?;
        let archive = tmp.path().join("wrapper.tar.xz");
        download_to(WRAPPER_URL, &archive)?;
        extract_tar_xz(&archive, tmp.path())?;
        let src = tmp.path().join(format!(
            "Template-{WRAPPER_VERSION}.app/Contents/Frameworks"
        ));
        if !src.exists() {
            return Err(anyhow!(
                "wrapper archive didn't contain expected path {}",
                src.display()
            ));
        }
        if frameworks.exists() {
            fs::remove_dir_all(&frameworks)
                .with_context(|| format!("removing stale {}", frameworks.display()))?;
        }
        copy_dir_preserving_symlinks(&src, &frameworks).context("copying wrapper Frameworks")?;
        fs::write(&wrapper_marker, WRAPPER_VERSION)
            .with_context(|| format!("writing {}", wrapper_marker.display()))?;
        log::info!("installed Frameworks at {}", frameworks.display());
    }

    let engine_marker = runtime_root.join("engine-version");
    if install_needed(
        wine_bin.exists(),
        fs::read_to_string(&engine_marker).ok().as_deref(),
        ENGINE_NAME,
    ) {
        log::info!("downloading Wine engine {ENGINE_NAME} ({ENGINE_URL})");
        let tmp = tempfile::tempdir().context("creating tmp dir for engine archive")?;
        let archive = tmp.path().join("engine.tar.xz");
        download_to(ENGINE_URL, &archive)?;
        extract_tar_xz(&archive, tmp.path())?;
        let src = tmp.path().join("wswine.bundle");
        if !src.exists() {
            return Err(anyhow!(
                "engine archive didn't contain expected wswine.bundle at {}",
                src.display()
            ));
        }
        if wswine_bundle.exists() {
            fs::remove_dir_all(&wswine_bundle)
                .with_context(|| format!("removing stale {}", wswine_bundle.display()))?;
        }
        // rename is cheap but cross-fs fails; fall back to a deep copy.
        if fs::rename(&src, &wswine_bundle).is_err() {
            copy_dir_preserving_symlinks(&src, &wswine_bundle)
                .context("installing wswine.bundle")?;
        }
        log::info!("installed Wine engine at {}", wswine_bundle.display());
    }

    // Sanity check: the engine actually runs.
    let version_check = Command::new(&wine_bin).arg("--version").output();
    match version_check {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout);
            log::info!("wine --version: {}", s.trim());
        }
        Ok(out) => {
            return Err(anyhow!(
                "wine --version failed (exit {:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Err(e) => return Err(anyhow!("failed to execute {}: {e}", wine_bin.display())),
    }
    fs::write(&engine_marker, ENGINE_NAME)
        .with_context(|| format!("writing {}", engine_marker.display()))?;
    Ok(())
}

/// Whether a runtime component must be (re)installed: it is missing, or its
/// version marker does not name the version this build wants.
fn install_needed(present: bool, marker: Option<&str>, want: &str) -> bool {
    !present || marker.is_none_or(|m| m.trim() != want)
}

fn download_to(url: &str, dst: &Path) -> Result<()> {
    let started = Instant::now();
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = response.into_reader();
    let mut out = fs::File::create(dst).with_context(|| format!("creating {}", dst.display()))?;
    let bytes = std::io::copy(&mut reader, &mut out).context("streaming HTTP body to disk")?;
    log::info!(
        "downloaded {} ({} bytes) in {:.1}s",
        dst.display(),
        bytes,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn extract_tar_xz(archive: &Path, dst: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xJf")
        .arg(archive)
        .arg("-C")
        .arg(dst)
        .status()
        .context("running tar -xJf (macOS bsdtar should handle .xz via libarchive)")?;
    if !status.success() {
        return Err(anyhow!(
            "tar -xJf {} -> {} failed with {status:?}",
            archive.display(),
            dst.display()
        ));
    }
    Ok(())
}

/// Recursive copy that preserves symlinks (critical for Frameworks and
/// wswine.bundle, which use version-suffixed dylibs linked into unversioned
/// names). Does *not* preserve file modes beyond the std defaults.
fn copy_dir_preserving_symlinks(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("creating dir {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let source = entry.path();
        let destination = dst.join(entry.file_name());
        if ty.is_symlink() {
            let link_target = fs::read_link(&source)?;
            // If destination already exists (from a previous partial copy),
            // remove it first — std symlink() won't overwrite.
            let _ = fs::remove_file(&destination);
            std::os::unix::fs::symlink(&link_target, &destination).with_context(|| {
                format!(
                    "symlinking {} -> {}",
                    destination.display(),
                    link_target.display()
                )
            })?;
        } else if ty.is_dir() {
            copy_dir_preserving_symlinks(&source, &destination)?;
        } else {
            fs::copy(&source, &destination).with_context(|| {
                format!("copying {} -> {}", source.display(), destination.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_prefix_from_nested_game_dir() {
        let game = PathBuf::from(
            "/Users/me/Library/Application Support/garlemald-client/prefix/drive_c/Program Files (x86)/SquareEnix/FINAL FANTASY XIV",
        );
        let prefix = derive_prefix_from_game_location(&game);
        assert_eq!(
            prefix,
            Some(PathBuf::from(
                "/Users/me/Library/Application Support/garlemald-client/prefix"
            ))
        );
    }

    #[test]
    fn no_prefix_when_no_drive_c() {
        let game = PathBuf::from("/tmp/ffxiv");
        assert_eq!(derive_prefix_from_game_location(&game), None);
    }

    #[test]
    fn derives_prefix_from_external_installer_layout() {
        // The xiv1point0-apple-silicon-installer's default prefix.
        let game = PathBuf::from(
            "/Users/me/Code/xiv1point0-apple-silicon-installer/target/prefix/drive_c/Program Files (x86)/SquareEnix/FINAL FANTASY XIV",
        );
        let prefix = derive_prefix_from_game_location(&game).unwrap();
        assert!(prefix.ends_with("target/prefix"));
    }

    #[test]
    fn tick_count_is_plausible_uptime() {
        // The monotonic clock returns ms since boot. On any reasonable CI /
        // developer machine this is well above 1 second and nowhere near
        // wrapping u32 (~49.7 days).
        let t = monotonic_ms_since_boot();
        assert!(t > 1_000, "expected non-trivial uptime, got {t}");
    }

    #[test]
    fn install_needed_quadrants() {
        // A missing component installs regardless of the marker.
        assert!(install_needed(false, None, ENGINE_NAME));
        assert!(install_needed(false, Some(ENGINE_NAME), ENGINE_NAME));
        // Present + matching marker (trailing newline tolerated) is the only skip.
        assert!(!install_needed(true, Some(ENGINE_NAME), ENGINE_NAME));
        assert!(!install_needed(
            true,
            Some(&format!("{ENGINE_NAME}\n")),
            ENGINE_NAME
        ));
        // Present but unmarked or differently marked reinstalls.
        assert!(install_needed(true, None, ENGINE_NAME));
        assert!(install_needed(
            true,
            Some("WS12WineCX24.0.7_7"),
            ENGINE_NAME
        ));
    }
}
