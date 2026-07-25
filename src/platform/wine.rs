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

//! Shared Wine-prefix plumbing used by the macOS and Linux platform backends.
//!
//! A managed "runtime" looks like:
//!
//! ```text
//! <data_dir>/    (per-app dir, e.g. ~/Library/Application Support/me.stegall.garlemald-client/ on macOS)
//! ├── prefix/                              # WINEPREFIX
//! │   └── drive_c/Program Files (x86)/SquareEnix/FINAL FANTASY XIV/
//! └── runtime/                             # macOS only
//!     ├── wswine.bundle/                   # Sikarugir Wine engine
//!     └── Frameworks/                      # bundled MoltenVK, libinotify, …
//! ```
//!
//! On Linux we rely on a system-installed `wine`; the `runtime/` directory is
//! a no-op there.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

use crate::config;

/// `WINEDEBUG` channel selection for launching the game.
///
/// Wine's debug syntax is `[class][+/-]channel`. Without a class prefix,
/// `-fixme` / `+err` treat those as *channel* names (which don't exist), so
/// the obvious-looking `-fixme,+err` is a silent no-op. Correct form:
/// * `fixme-all` — silence fixme for every channel
/// * `err+all` — keep the err class on for every channel. This also
///   surfaces genuine crashes / unhandled exceptions as `err:seh`.
/// * `+debugstr` — log every `OutputDebugStringA/W` call. Pairs with the
///   `assert_log_patch` PE patch, which redirects the
///   game's silent assert handler into `OutputDebugStringA`
///   so the assertion message lands in the wine log right
///   before the trap fires.
/// * `warn+d3d` — keep the wined3d WARN level on (otherwise silent),
///   so the rejection reason for failed `StretchRect` /
///   `Present` / surface-creation calls is visible. Wine's
///   WARN class is suppressed by default; the cinematic
///   crash trace lives here.
///
/// `+seh` is deliberately NOT in this default. It enables every class
/// (including `trace`) on the seh channel, so Wine writes a full CPU register
/// dump on every stack unwind — and FFXIV 1.0 fires a storm of `longjmp`
/// unwinds (`RtlUnwindEx code=STATUS_LONGJUMP`) once in-world. With Wine's
/// stdout/stderr redirected to `wine.log`, that floods the disk and drops the
/// game to a slideshow (GPU near-idle, CPU pinned in the trace path). Genuine
/// crashes still surface via `err+all` (`err:seh`); full seh tracing is
/// available on demand through the "verbose Wine debug logging" developer
/// toggle (see `VERBOSE_WINE_DEBUG` in `app/developer_window.rs`).
///
/// Callers that want more verbosity (e.g. `+relay,+module,+loaddll`) can set
/// `WINEDEBUG` in the environment; we only fill this in as a default.
const WINEDEBUG_DEFAULT: &str = "fixme-all,err+all,+debugstr,warn+d3d";

/// Relative path inside the prefix to the FFXIV install root, matching the
/// default the InstallShield installer uses.
pub const PREFIX_FFXIV_SUBPATH: &str = "drive_c/Program Files (x86)/SquareEnix/FINAL FANTASY XIV";

/// Returns a 32-bit millisecond tick value compatible with what Wine's
/// `GetTickCount()` will report to the game process.
///
/// Wine implements `GetTickCount` on top of `clock_gettime(CLOCK_MONOTONIC)`
/// (ms since boot), NOT the wall-clock. The Blowfish key used in the
/// command-line encryption is derived from the top 16 bits of this value, so
/// the launcher's tick and the game's tick must come from the same clock for
/// the keys to agree.
pub fn monotonic_ms_since_boot() -> u32 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    let ms = (ts.tv_sec as u64).wrapping_mul(1_000) + (ts.tv_nsec as u64 / 1_000_000);
    ms as u32
}

pub struct WineRuntime {
    #[allow(dead_code)]
    pub root: PathBuf,
    pub prefix: PathBuf,
    pub wine_bin: PathBuf,
    #[allow(dead_code)]
    pub wineserver_bin: PathBuf,
    /// Additional `DYLD_FALLBACK_LIBRARY_PATH` entries (macOS only).
    #[allow(dead_code)] // read only in the macOS cfg block; dead on Linux.
    pub dyld_fallback_paths: Vec<PathBuf>,
    /// `gstreamer-1.0` plugin directory shipped in the runtime bundle. When
    /// `Some`, gets exported as `GST_PLUGIN_PATH` / `GST_PLUGIN_SYSTEM_PATH`
    /// so Wine's `winegstreamer` finds these instead of any system / brew
    /// install at a different version.
    pub gst_plugin_path: Option<PathBuf>,
}

impl WineRuntime {
    #[allow(dead_code)] // used by the Linux backend; macOS derives paths differently.
    pub fn install_root(&self) -> PathBuf {
        self.prefix.join(PREFIX_FFXIV_SUBPATH)
    }

    pub fn configure_command(&self, cmd: &mut Command) {
        self.configure_command_with_debug(cmd, None);
    }

    /// Like [`configure_command`], but lets the caller inject a specific
    /// `WINEDEBUG` value (e.g. from the Developer Settings dialog). When
    /// `wine_debug` is `None`, behaviour matches the original default:
    /// honour the parent env, else fall back to [`WINEDEBUG_DEFAULT`].
    pub fn configure_command_with_debug(&self, cmd: &mut Command, wine_debug: Option<&str>) {
        cmd.env("WINEPREFIX", &self.prefix);
        if let Some(value) = wine_debug {
            cmd.env("WINEDEBUG", value);
        } else if std::env::var_os("WINEDEBUG").is_none() {
            cmd.env("WINEDEBUG", WINEDEBUG_DEFAULT);
        }
        #[cfg(target_os = "macos")]
        if !self.dyld_fallback_paths.is_empty() {
            let joined = std::env::join_paths(&self.dyld_fallback_paths)
                .expect("wine runtime dyld paths contain no colons");
            cmd.env("DYLD_FALLBACK_LIBRARY_PATH", joined);
        }
        if let Some(plugin_dir) = &self.gst_plugin_path {
            cmd.env("GST_PLUGIN_PATH", plugin_dir);
            cmd.env("GST_PLUGIN_SYSTEM_PATH", plugin_dir);
        }
    }

    /// Runs `wineserver -w` to let in-flight writes flush before we take
    /// action on the prefix contents.
    #[allow(dead_code)]
    pub fn wait_for_wineserver(&self) -> Result<()> {
        let mut cmd = Command::new(&self.wineserver_bin);
        cmd.arg("-w");
        self.configure_command(&mut cmd);
        let status = cmd.status().context("running wineserver -w")?;
        if !status.success() {
            return Err(anyhow!("wineserver -w exited with status {status:?}"));
        }
        Ok(())
    }
}

pub fn ensure_prefix_initialized(runtime: &WineRuntime) -> Result<()> {
    if runtime.prefix.join("system.reg").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&runtime.prefix)
        .with_context(|| format!("creating prefix {}", runtime.prefix.display()))?;
    let mut cmd = Command::new(&runtime.wine_bin);
    cmd.arg("wineboot").arg("--init");
    runtime.configure_command(&mut cmd);
    let status = cmd.status().context("running wineboot --init")?;
    if !status.success() {
        return Err(anyhow!("wineboot --init exited with status {status:?}"));
    }
    Ok(())
}

pub fn launch_ffxiv_game(
    runtime: &WineRuntime,
    exe_path: &Path,
    encoded_argument: &str,
    wine_debug_override: Option<&str>,
    enable_winsock_proxy: bool,
    extra_dll_overrides: Option<&str>,
) -> Result<()> {
    // Resolve / tear down the ws2_32 DLL-hijack proxy before we fork
    // the game process. The proxy logs every `send`/`recv`/etc. call to
    // `<game_dir>/ws2_32-trace.log`; when the toggle is off we actively
    // remove both our proxy and the renamed system-DLL copy so the PE
    // loader goes back to Wine's built-in ws2_32 on the next launch.
    if let Some(game_dir) = exe_path.parent() {
        apply_winsock_proxy(runtime, game_dir, enable_winsock_proxy)
            .unwrap_or_else(|e| log::warn!("winsock proxy deploy skipped: {e:#}"));
    }
    // Wine classifies `ws2_32.dll` as a builtin and loads it from its own
    // WINEDLLPATH bundle regardless of what sits next to the exe. `ws2_32=n,b`
    // tells the loader to try the native (file-based) copy first, then fall
    // back to the builtin on miss. The comma separates the preferred order; `n`
    // alone would reject the builtin entirely and break the unhooked fallback
    // when the proxy isn't deployed, so we pair it with `b`. It is assembled
    // into WINEDLLOVERRIDES alongside any DXVK overrides just before launch.
    let log_path = wine_log_path()?;
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .with_context(|| format!("opening wine log {}", log_path.display()))?;
    writeln!(
        &log_file,
        "=== garlemald-client launch ===\nwine: {}\nexe:  {}\narg:  {}\nprefix: {}\nWINEDEBUG: {}\n",
        runtime.wine_bin.display(),
        exe_path.display(),
        encoded_argument,
        runtime.prefix.display(),
        wine_debug_override.unwrap_or("(default)"),
    )
    .ok();

    let stderr_log = log_file
        .try_clone()
        .context("cloning wine log fd for stderr redirect")?;
    let stdout_log = log_file
        .try_clone()
        .context("cloning wine log fd for stdout redirect")?;

    let mut cmd = Command::new(&runtime.wine_bin);
    cmd.arg(exe_path)
        .arg(encoded_argument)
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log));
    if let Some(cwd) = exe_path.parent() {
        cmd.current_dir(cwd);
    }
    runtime.configure_command_with_debug(&mut cmd, wine_debug_override);
    // Assemble WINEDLLOVERRIDES for this launch: DXVK's native D3D DLLs (Linux,
    // `d3d9=n,b;dxgi=n,b`) plus, when the winsock proxy is on, `ws2_32=n,b` (see
    // the note above). Entries are `;`-joined. Setting it on this specific
    // Command — not the parent process — keeps the override scoped to this
    // launch; Wine reads WINEDLLOVERRIDES on process start, before any DLL
    // resolution inside the game.
    let mut dll_overrides: Vec<&str> = Vec::new();
    if let Some(dxvk) = extra_dll_overrides {
        dll_overrides.push(dxvk);
    }
    if enable_winsock_proxy {
        dll_overrides.push("ws2_32=n,b");
    }
    if !dll_overrides.is_empty() {
        let joined = dll_overrides.join(";");
        log::info!("WINEDLLOVERRIDES={joined}");
        cmd.env("WINEDLLOVERRIDES", joined);
    }

    log::info!(
        "launching ffxivgame via wine; output → {}",
        log_path.display()
    );
    let status = cmd.status().context("launching ffxivgame.exe via wine")?;

    writeln!(&log_file, "\n=== exit: {status:?} ===").ok();

    if !status.success() {
        emit_log_tail(&log_path);
        return Err(anyhow!(
            "ffxivgame.exe exited with status {status:?}; see {} for wine output",
            log_path.display(),
        ));
    }
    Ok(())
}

/// Path where we write a fresh Wine stdout+stderr capture on every launch.
/// Lives next to the prefix under `<data_dir>/logs/wine.log`.
fn wine_log_path() -> Result<PathBuf> {
    let dir = config::data_dir()?.join("logs");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating wine log dir {}", dir.display()))?;
    Ok(dir.join("wine.log"))
}

/// Prints the tail of the wine log to our own logger so a failed launch
/// leaves immediately-visible breadcrumbs without forcing the user to open
/// the file themselves.
fn emit_log_tail(log_path: &Path) {
    const TAIL_BYTES: usize = 8 * 1024;
    match std::fs::read(log_path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(TAIL_BYTES);
            let tail = String::from_utf8_lossy(&bytes[start..]);
            log::error!("--- tail of {} ---", log_path.display());
            for line in tail.lines() {
                log::error!("wine: {line}");
            }
            log::error!("--- end of wine log tail ---");
        }
        Err(e) => log::error!("could not read wine log {}: {e}", log_path.display()),
    }
}

/// Install or remove the `ws2_32.dll` hijack proxy in the game folder.
///
/// The proxy is a 32-bit Windows cdylib built out of the sibling
/// `ws2_32-proxy` crate — `cargo build --release --target
/// i686-pc-windows-gnu` inside `garlemald-client/ws2_32-proxy/`. Once
/// built, the DLL lives at
/// `ws2_32-proxy/target/i686-pc-windows-gnu/release/ws2_32.dll`.
///
/// When the Developer Setting toggle is on, we:
///   1. Copy the real `drive_c/windows/system32/ws2_32.dll` out of the
///      Wine prefix into `<game_dir>/ws2_32_real.dll`. The proxy's
///      export table forwards every unhooked function to
///      `ws2_32_real.X`, so this renamed copy is what keeps the game's
///      actual networking alive.
///   2. Copy the built proxy into `<game_dir>/ws2_32.dll`, winning the
///      loader's search-order race against `system32`.
///
/// When the toggle is off — or on but the proxy hasn't been built — we
/// remove both files so the stock loader path resumes. Errors here
/// don't abort the launch: if the proxy can't be deployed, the user
/// gets plain networking and a log line explaining why.
fn apply_winsock_proxy(_runtime: &WineRuntime, game_dir: &Path, enable: bool) -> Result<()> {
    let proxy_dest = game_dir.join("ws2_32.dll");
    // Earlier versions of this launcher also deployed a renamed copy
    // of Wine's real ws2_32 as `ws2_32_real.dll` and used PE `FORWARD`
    // entries to route unhooked exports to it. That DllMain aborted
    // under its non-canonical name (Wine's builtin init expects to
    // find a matching `ws2_32.dll.so` for the Unix socket backend) —
    // `err:module:loader_init "ws2_32_real.dll" failed ... c0000142`.
    // The proxy now resolves the real DLL at runtime by absolute path
    // (`C:\\windows\\syswow64\\ws2_32.dll`) and JMP-thunks every
    // unhooked export, so the rename is no longer needed. Scrub any
    // stale copy a previous launch might have left behind.
    let legacy_real_copy = game_dir.join("ws2_32_real.dll");

    if !enable {
        for p in [&proxy_dest, &legacy_real_copy] {
            if p.exists()
                && let Err(e) = std::fs::remove_file(p)
            {
                log::warn!("failed to remove {}: {e}", p.display());
            }
        }
        return Ok(());
    }

    let built_proxy =
        workspace_ws2_32_proxy_dll().context("locating the built ws2_32 proxy DLL")?;
    if !built_proxy.exists() {
        return Err(anyhow!(
            "winsock tracing is on but the proxy DLL is missing at {}.\n\
             Run `cargo build --release --target i686-pc-windows-gnu` inside\n\
             garlemald-client/ws2_32-proxy before launching.",
            built_proxy.display(),
        ));
    }

    if legacy_real_copy.exists() {
        let _ = std::fs::remove_file(&legacy_real_copy);
    }
    std::fs::copy(&built_proxy, &proxy_dest).with_context(|| {
        format!(
            "copying proxy DLL: {} -> {}",
            built_proxy.display(),
            proxy_dest.display()
        )
    })?;
    log::info!("ws2_32 proxy installed: {}", proxy_dest.display(),);
    log::info!(
        "winsock traces will land at {}",
        game_dir.join("ws2_32-trace.log").display()
    );
    Ok(())
}

/// Resolve the path to the pre-built ws2_32 proxy DLL. We expect the
/// proxy crate to live at `ws2_32-proxy/` alongside this client's
/// source tree (same directory as `Cargo.toml`), with the mingw-gnu
/// cross-target output under `target/i686-pc-windows-gnu/release/`.
fn workspace_ws2_32_proxy_dll() -> Result<PathBuf> {
    // `CARGO_MANIFEST_DIR` at build time points at the client crate
    // root. We embed it as a compile-time constant so the launched
    // binary can find the proxy DLL even when invoked from a different
    // cwd (e.g. the macOS `.app` bundle).
    let client_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(client_root
        .join("ws2_32-proxy")
        .join("target")
        .join("i686-pc-windows-gnu")
        .join("release")
        .join("ws2_32.dll"))
}

/// Copies `source_exe` to `dest_exe`, replacing `dest_exe` if it exists.
/// Intended for producing a fresh patched working copy per launch.
pub fn copy_exe_for_patching(source_exe: &Path, dest_exe: &Path) -> Result<()> {
    if let Some(parent) = dest_exe.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating dir {}", parent.display()))?;
    }
    std::fs::copy(source_exe, dest_exe)
        .with_context(|| format!("copying {} -> {}", source_exe.display(), dest_exe.display()))?;
    Ok(())
}
