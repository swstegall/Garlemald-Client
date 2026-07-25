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

//! Linux DXVK provisioning.
//!
//! FFXIV 1.0 is a 32-bit Direct3D 9 game. Wine's builtin `wined3d` translates
//! D3D9 → OpenGL on a single thread; it is CPU-bound and starves the GPU on
//! this title (single-digit framerate in-world, GPU near-idle). DXVK translates
//! D3D9 → Vulkan with far lower CPU overhead — the same accelerated path the
//! bundled macOS engine already provides, which is why macOS runs it well.
//!
//! On first launch we download a pinned DXVK release into the runtime cache,
//! drop its 32-bit `d3d9.dll` / `dxgi.dll` into the prefix's `syswow64`, and
//! return the `WINEDLLOVERRIDES` fragment that makes Wine load them natively.
//! The `,b` builtin fallback means a DXVK/Vulkan failure degrades to wined3d
//! rather than breaking the launch. Everything here is best-effort: any error
//! (no Vulkan, offline, disabled) leaves the game on wined3d, logged.
//!
//! macOS is intentionally not handled here — its bundled engine ships its own
//! accelerated D3D path, and forcing upstream DXVK into that prefix can conflict
//! with the engine's patched Wine + bundled MoltenVK.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::platform::wine::WineRuntime;

/// Pinned DXVK release. Bump together with [`DXVK_URL`].
const DXVK_VERSION: &str = "3.0";
const DXVK_URL: &str = "https://github.com/doitsujin/dxvk/releases/download/v3.0/dxvk-3.0.tar.gz";

/// 32-bit DXVK DLLs we install (FFXIV 1.0 is a 32-bit D3D9 client, so it loads
/// from the prefix's `syswow64`).
const DXVK_DLLS: &[&str] = &["d3d9.dll", "dxgi.dll"];

/// `WINEDLLOVERRIDES` fragment: prefer the native (DXVK) DLLs, fall back to the
/// builtin (wined3d) if DXVK can't initialize.
const DXVK_OVERRIDES: &str = "d3d9=n,b;dxgi=n,b";

/// Set `GARLEMALD_DISABLE_DXVK=1` to force Wine's builtin wined3d path.
const DISABLE_ENV: &str = "GARLEMALD_DISABLE_DXVK";

/// Best-effort: make DXVK available in `runtime`'s prefix and return the
/// `WINEDLLOVERRIDES` fragment to apply at launch, or `None` to stay on wined3d
/// (DXVK disabled, no Vulkan, or a setup error — all logged).
///
/// `cache_root` is where the downloaded DXVK release is unpacked and reused
/// across launches. The prefix must already be initialized (its `syswow64`
/// present).
pub fn ensure_dxvk(runtime: &WineRuntime, cache_root: &Path) -> Option<String> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        log::info!("{DISABLE_ENV} is set — using builtin wined3d");
        return None;
    }
    if !vulkan_available() {
        log::warn!(
            "no Vulkan ICD found under /usr/share/vulkan or /etc/vulkan — DXVK unavailable, using wined3d"
        );
        return None;
    }
    match install_dxvk(runtime, cache_root) {
        Ok(()) => {
            log::info!("DXVK {DXVK_VERSION} active (Direct3D 9 → Vulkan)");
            Some(DXVK_OVERRIDES.to_string())
        }
        Err(e) => {
            log::warn!("DXVK setup failed ({e:#}); falling back to wined3d");
            None
        }
    }
}

/// A present Vulkan ICD manifest is the most reliable signal that a usable
/// Vulkan driver is installed. (The `,b` override fallback covers a driver that
/// is listed but non-functional.)
fn vulkan_available() -> bool {
    ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|ext| ext == "json"))
}

/// Copy the pinned DXVK 32-bit DLLs into the prefix's `syswow64`, downloading
/// and unpacking the release into `cache_root` on first use. Idempotent via a
/// `.dxvk-version` marker written into the prefix.
fn install_dxvk(runtime: &WineRuntime, cache_root: &Path) -> Result<()> {
    let syswow64 = runtime.prefix.join("drive_c/windows/syswow64");
    if !syswow64.is_dir() {
        return Err(anyhow!(
            "prefix syswow64 missing at {} (prefix not initialized, or a 32-bit prefix?)",
            syswow64.display()
        ));
    }
    let marker = runtime.prefix.join(".dxvk-version");
    if std::fs::read_to_string(&marker)
        .ok()
        .as_deref()
        .map(str::trim)
        == Some(DXVK_VERSION)
    {
        return Ok(()); // already installed
    }

    let dxvk_dir = ensure_downloaded(cache_root)?;
    for dll in DXVK_DLLS {
        let src = dxvk_dir.join("x32").join(dll);
        let dst = syswow64.join(dll);
        std::fs::copy(&src, &dst)
            .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
    }
    std::fs::write(&marker, DXVK_VERSION)
        .with_context(|| format!("writing DXVK marker {}", marker.display()))?;
    log::info!("installed DXVK {DXVK_VERSION} into {}", syswow64.display());
    Ok(())
}

/// Ensure `cache_root/dxvk-<version>/x32/d3d9.dll` exists, downloading and
/// unpacking the release tarball if needed. Returns the `dxvk-<version>` dir.
fn ensure_downloaded(cache_root: &Path) -> Result<PathBuf> {
    let dxvk_dir = cache_root.join(format!("dxvk-{DXVK_VERSION}"));
    if dxvk_dir.join("x32").join("d3d9.dll").exists() {
        return Ok(dxvk_dir);
    }
    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("creating DXVK cache dir {}", cache_root.display()))?;
    let tmp = tempfile::tempdir().context("creating tmp dir for DXVK archive")?;
    let archive = tmp.path().join("dxvk.tar.gz");
    log::info!("downloading DXVK {DXVK_VERSION} ({DXVK_URL})");
    download_to(DXVK_URL, &archive)?;
    extract_tar_gz(&archive, cache_root)?;
    if !dxvk_dir.join("x32").join("d3d9.dll").exists() {
        return Err(anyhow!(
            "DXVK archive did not contain the expected {}/x32/d3d9.dll",
            dxvk_dir.display()
        ));
    }
    Ok(dxvk_dir)
}

fn download_to(url: &str, dst: &Path) -> Result<()> {
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = response.into_reader();
    let mut out =
        std::fs::File::create(dst).with_context(|| format!("creating {}", dst.display()))?;
    std::io::copy(&mut reader, &mut out).context("streaming DXVK download to disk")?;
    Ok(())
}

fn extract_tar_gz(archive: &Path, dst: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dst)
        .status()
        .context("running tar -xzf for the DXVK archive")?;
    if !status.success() {
        return Err(anyhow!(
            "tar -xzf {} -> {} failed with {status:?}",
            archive.display(),
            dst.display()
        ));
    }
    Ok(())
}
