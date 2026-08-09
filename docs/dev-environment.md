# Developer environment

How to build and run the launcher, turn on logging (launcher- and Wine-side), find
and reset the launcher's on-disk state, and run the whole thing end-to-end against a
local Garlemald-Server.

For *what* the launcher does, read [`architecture.md`](architecture.md) first.

---

## Prerequisites

- **Rust 1.95.0**, pinned in `rust-toolchain.toml` (channel `1.95.0`, with `rustfmt`
  + `clippy`). With `rustup` the correct toolchain installs automatically on first
  build. The crate is **edition 2024**.
- The launcher is a **GUI app** (`eframe`/`egui`) with a native **login WebView**
  (`wry`/`tao`), so it needs each OS's GUI/WebView libraries:

  | Platform | What you need |
  |----------|---------------|
  | **macOS** (Apple Silicon + Intel) | Xcode Command Line Tools. Apple Silicon also needs **Rosetta 2** at *run* time (the managed Wine engine is x86) — `softwareupdate --install-rosetta --agree-to-license`. |
  | **Linux** | A C toolchain plus the GTK3 / WebKit2GTK / X11 / Wayland / GL dev libraries (see the list below). The game itself runs under **system Wine** (Wine 7+), so install `wine` from your distro too. |
  | **Windows** | The **32-bit** MSVC C++ build tools — the launcher builds as `i686` (it reads the suspended 32-bit `ffxivgame.exe` thread context to patch it). Plus **[NASM](https://www.nasm.us/)** on `PATH` for the 32-bit build (see the note under [Build and run](#build-and-run)). |

  Linux dev libraries (the same set CI installs — Debian/Ubuntu names):

  ```sh
  sudo apt-get install -y \
    libwebkit2gtk-4.1-dev libjavascriptcoregtk-4.1-dev libsoup-3.0-dev \
    libgtk-3-dev libxdo-dev libayatana-appindicator3-dev librsvg2-dev \
    libgl1-mesa-dev libxkbcommon-dev libwayland-dev \
    libxcursor-dev libxrandr-dev libxi-dev
  ```

The `eframe` build pulls the `glow`, `wayland`, and `x11` backends (see
`Cargo.toml`), so it runs under both X11 and Wayland on Linux.

---

## Build and run

```sh
cargo build --release
cargo run --release
```

On **Windows** the launcher is **32-bit only** (`i686-pc-windows-msvc`): the PE
patcher reads the suspended 32-bit `ffxivgame.exe` thread context, which a 64-bit
process cannot do, so an `x86_64-pc-windows-msvc` build is **rejected at compile
time** (`src/lib.rs`). CI and the release workflow build i686; so should you:

```powershell
rustup target add i686-pc-windows-msvc
cargo run --release --target i686-pc-windows-msvc
```

To drop the `--target` and have plain `cargo build` / `cargo run` default to 32-bit,
create a **local** `.cargo/config.toml` in the repo root — it's gitignored on
purpose, because a *committed* pin would force i686 on macOS/Linux checkouts too:

```toml
[build]
target = "i686-pc-windows-msvc"
```

> **NASM is required on Windows.** `aws-lc-sys` (pulled in transitively via
> `librqbit` → `aws-lc-rs` for torrent-piece SHA-1) assembles its crypto with
> [NASM](https://www.nasm.us/), and the 32-bit `i686` target has no prebuilt
> objects, so NASM must be on `PATH`:
>
> ```powershell
> winget install nasm   # or: choco install nasm
> ```
>
> If NASM lands somewhere off `PATH` (e.g. `%LOCALAPPDATA%\bin\NASM` or
> `C:\Program Files\NASM`), add that directory to `PATH` so the aws-lc-sys build
> script can invoke `nasm`.

Distributable bundles (see `scripts/`):

```sh
scripts/package-macos.sh               # macOS .app, host arch, ad-hoc signed
scripts/package-macos.sh --universal   # x86_64 + aarch64 fat binary
scripts/package-windows.cmd            # Windows package
```

> The `ws2_32-proxy/` directory is a **separate** crate (a Windows winsock-tracing
> DLL), intentionally *not* part of this workspace — you only build it if you want
> the optional packet-tracing developer feature (see [Logging](#logging)).

---

## Logging

### Launcher log (`env_logger` / `RUST_LOG`)

The launcher logs through `log` + `env_logger`, initialized in `src/main.rs` with a
default of **`info`**. Override with the standard `RUST_LOG` syntax:

```sh
RUST_LOG=debug cargo run --release
RUST_LOG=garlemald_client::patcher=trace cargo run --release   # one module, verbose
RUST_LOG=warn,garlemald_client::login=debug cargo run --release
```

### Wine / game log (macOS + Linux)

When the game runs under Wine, its stdout/stderr are captured to
**`<data_dir>/logs/wine.log`** (`src/platform/wine.rs`), truncated on each launch; on
a launch failure the launcher echoes the tail of that file into the launcher log.
Wine's own verbosity is controlled by `WINEDEBUG` (default
`fixme-all,err+all,+seh,+debugstr,warn+d3d`). To raise it, enable
**Developer Settings → verbose Wine debug logging** in the launcher (it sets a more
verbose `WINEDEBUG`), or set `WINEDEBUG`/the launch override yourself.

### Winsock packet tracing (optional, macOS + Linux)

For wire-level debugging there's an optional **`ws2_32` proxy DLL** that logs the
game's socket traffic. Build it separately and enable **Developer Settings → winsock
tracing**; the launcher then deploys the DLL next to the game and writes a trace log
beside it. See `ws2_32-proxy/README.md` for the build/trace details.

---

## Launcher state — where it lives and how to reset it

The launcher keeps **no state in the repo**. Everything lives in per-user OS
directories via the [`directories`](https://docs.rs/directories) crate,
`ProjectDirs::from("me", "stegall", "garlemald-client")` (`src/config/paths.rs`):

| Location | Holds |
|----------|-------|
| **`config_dir()`** | `preferences.toml` (selected server, game location, dev flags) |
| **`data_dir()`** | `prefix/` (the managed Wine prefix), `runtime/` (the auto-downloaded Wine engine — macOS), `logs/wine.log` |

On macOS both resolve under `~/Library/Application Support/me.stegall.garlemald-client/`
(`src/platform/macos.rs`); Linux uses `~/.config/...` and `~/.local/share/...`;
Windows uses the roaming/local `AppData` equivalents.

- **Server list & defaults:** the selectable server list is baked into the binary
  from `src/servers/default_servers.toml` (`ServerDefinitions::load_default`); the
  *selected* server's name/address is saved in `preferences.toml`. If there's no
  `preferences.toml` yet, the launcher falls back to the bundled
  `./configs/garlemald-client.toml`. (A `servers.xml` config path is defined in
  `src/config/paths.rs` but is not yet wired up.)
- **`preferences.toml`** (`src/config/preferences.rs`) stores `LauncherPreferences`
  (server name/address, `game_location`, optional `wine_runtime_dir`,
  `patch_download_dir`) and `DeveloperPreferences` (`enable_verbose_wine_debug`,
  `enable_winsock_tracing`).

### Getting a clean slate

There's no in-app reset; delete the relevant files/dirs (the launcher recreates them):

```sh
# macOS paths shown; adjust the base dir for Linux/Windows.
APP="$HOME/Library/Application Support/me.stegall.garlemald-client"

rm -f  "$APP/preferences.toml"   # reset settings, selected server, and game location
rm -rf "$APP/prefix"             # nuke the Wine prefix (re-created via `wineboot --init` next launch)
rm -f  "$APP/logs/wine.log"      # clear the Wine log (re-created next launch)
```

- Deleting `prefix/` forces a fresh Wine prefix on the next launch
  (`ensure_prefix_initialized` runs `wineboot --init` when `system.reg` is absent).
- Deleting `game_location` from `preferences.toml` (or the whole file) makes the
  launcher **re-detect** the install on startup, and a version mismatch re-triggers
  patching.
- On Apple Silicon you can also delete `runtime/` to force a fresh Wine-engine
  download.

---

## Running against a local Garlemald-Server

End-to-end test loop against a server on your machine:

1. **Start the server stack.** In a [Garlemald-Server](https://github.com/swstegall/Garlemald-Server)
   checkout, run `./scripts/run-all.sh` (web `:54993`, lobby `:54994`, world `:54992`,
   map `:1989`). See the server's
   [`docs/dev-environment.md`](https://github.com/swstegall/Garlemald-Server/blob/develop/docs/dev-environment.md).
2. **Make an account.** Open the server's web auth (`http://127.0.0.1:54993/signup`)
   and create one — that's what mints the `sessionId` the launcher captures.
3. **Point the launcher at it.** Run the launcher and pick a server whose `loginUrl`
   is the local web login (`http://127.0.0.1:54993/login`). The bundled defaults
   (`src/servers/default_servers.toml`) already include a `Localhost` entry; to change
   the targets, edit that file and rebuild.
4. **Detect/patch the game**, then **Launch** — the WebView login returns a
   `sessionId`, the launcher hands a patched install + session to `ffxivgame.exe`, and
   the game connects to your local lobby/world/map.

> The game only negotiates correctly at client version `2012.09.19.0001`, so let the
> launcher finish patching before launching.

---

## Verify your setup

Before opening a PR, run what CI runs (see [`../CONTRIBUTING.md`](../CONTRIBUTING.md)):

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo build --all-targets --locked
cargo test --locked
```

CI runs `fmt`/`clippy` on Linux and `build`/`test` on macOS, Linux, and Windows, so
cross-platform breakage shows up there even if you only build on one OS locally.

---

## Where to read next

- [`architecture.md`](architecture.md) — the launcher pipeline and module map.
- [`agents.md`](agents.md) — running an AI coding agent against an issue.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — the contribution workflow.
