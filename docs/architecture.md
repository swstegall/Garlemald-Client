# Architecture

This document explains **what the launcher does end to end**, **how its modules
are organized**, and **how it talks to a Garlemald server**. Read it before diving
into any single module.

To build and run the launcher, see [`dev-environment.md`](dev-environment.md).

> **Scope.** Garlemald Client is a launcher for **FINAL FANTASY XIV v1.23b** — the
> last patch of the original 2010 1.0 release, *not* A Realm Reborn. It is a single
> Rust crate. Its job ends the moment a patched game is running with a valid
> session; from there the **game** talks to the server. For the server side, see
> the [Garlemald-Server architecture doc](https://github.com/swstegall/Garlemald-Server/blob/develop/docs/architecture.md).

---

## The launcher pipeline

From cold start to in-game, the launcher runs this pipeline:

```
                          ┌─────────────────────────── garlemald-client ───────────────────────────┐
                          │                                                                          │
  ┌──────────┐   ┌────────▼─────────┐   ┌──────────────┐   ┌───────────────┐   ┌──────────────────┐ │
  │  detect  │──▶│  check version   │──▶│    patch     │──▶│  WebView login │──▶│  launch the game │ │
  │ install  │   │  (game.ver vs    │   │  download +  │   │  ffxiv://login │   │  (+ PE patch /   │ │
  │          │   │  2012.09.19.0001)│   │  ZiPatch apply│   │  _success?     │   │   Wine on mac/   │ │
  └──────────┘   └──────────────────┘   │  (CRC32)     │   │  sessionId=…)  │   │   linux)         │ │
                          ▲             └──────────────┘   └───────┬───────┘   └────────┬─────────┘ │
                          │                  ▲                     │                    │           │
                          │        patch torrent swarm       server loginUrl           │           │
                          └──────────────────┼─────────────────────┼───────────────────┼───────────┘
                                             │                     │                    │
                                      BitTorrent swarm        web-server :54993    ffxivgame.exe
                                                                                        │
                                                            game connects to the server ▼
                                                       lobby :54994 · world :54992 · map :1989
```

1. **Detect** an installed FFXIV 1.x client (per-OS; see [Platform](#platform-support)).
2. **Check version** — read the install's `game.ver` and compare to the target
   `FFXIV_GAME_VERSION` (`2012.09.19.0001`). If it's behind, patch.
3. **Patch** — the needed ZiPatch files arrive via the BitTorrent transport (or a
   user-supplied local patches folder), are CRC32-validated against the manifest,
   then applied, taking the install from `FFXIV_BOOT_VERSION` (`2010.09.18.0000`) up
   to `2012.09.19.0001`.
4. **Log in** — open the selected server's `loginUrl` in a WebView and capture the
   `sessionId` from the `ffxiv://login_success` redirect.
5. **Launch** — build the (Blowfish-encrypted) game arguments, apply the launch-time
   PE patches (lobby hostname, etc.), and start `ffxivgame.exe` — natively on
   Windows, or through the managed Wine runtime on macOS/Linux.

After step 5 the launcher is done: the **game** opens its own TCP connections to
the server's lobby (`54994`), world (`54992`), and map (`1989`) services.

> Version constants live in `src/version.rs` (`FFXIV_BOOT_VERSION`,
> `FFXIV_GAME_VERSION`). `2012.09.19.0001` is the build the wire protocol targets —
> patching the install to it is what makes the game compatible with the server.

---

## Module responsibilities

Single crate, `src/`:

| Module            | Responsibility                                                                                  |
|-------------------|-------------------------------------------------------------------------------------------------|
| `app/`            | The `eframe`/`egui` GUI — server picker, game-location selector, patch progress, launch button   |
| `servers/`        | The server registry: `ServerDefinition { name, address, login_url }`                              |
| `patcher/`        | The apply worker thread, the patch manifest (sizes + CRCs), and torrented-archive extraction      |
| `patch_format/`   | The ZiPatch format itself — decompress + apply file deltas                                        |
| `login/`          | The WebView login subprocess and the `ffxiv://login_success` handshake                            |
| `crypto/`         | Blowfish encryption of the game's launch arguments                                                |
| `launcher/`       | Assembling the launch request, the PE patches, and starting the game                             |
| `platform/`       | Per-OS support: native Win32 on Windows, managed Wine on macOS/Linux                              |
| `torrent/`        | BitTorrent patch transport: magnet endpoint client + librqbit session service (download, then opt-out seeding) |
| `install_check.rs`| The 1.23b install gate: NotFound / FoundNeedsPatch / Ready; login and launch are blocked until Ready |
| `config/`         | Config/data paths (`directories`) and `preferences.toml`                                          |
| `version.rs`      | The launcher version and the FFXIV boot/game version constants                                    |
| `main.rs` / `lib.rs` | Entry point + the `run()` that wires the GUI; the `--login-webview` subcommand                 |

### `app/` — the GUI

`eframe`/`egui`. `app/launcher_window.rs` hosts `LauncherApp`, a small state machine
(a main screen and a patcher screen). `patcher_window.rs` renders the
download/apply progress (it polls the patcher's shared atomics each frame, so the UI
never blocks). `settings_window.rs` is the game-location picker; `developer_window.rs`
is the developer settings modal (verbose Wine logging, winsock tracing).

### `servers/` — the server registry

A `ServerDefinition` is `{ name, address, login_url, api_url }`. The selectable server
list is baked into the binary from `src/servers/default_servers.toml` (`include_str!`
as `DEFAULT_SERVERS_TOML`, parsed by `ServerDefinitions::load_default` — e.g. a
`Localhost` entry pointing at `127.0.0.1`'s web login). The *selected* server's
name/address is persisted in `preferences.toml`. (A `servers.xml` config path is
defined in `config/paths.rs` but is not yet wired up.)

Login takes one of two shapes. A server with a `login_url` opens it in the webview
subprocess (see `login/`). A server with an empty `login_url` and an `api_url`
instead gets the native login/sign-up form (`app/native_login_window.rs` +
`login/native.rs`), which speaks the bahamut-style JSON auth contract directly:
`POST <api_url>/accounts` to register, `POST <api_url>/sessions` for the
56-character hex session id that feeds the same launch pipeline. Plain-http
`api_url`s are refused for non-loopback hosts so passwords never cross a network
unencrypted.

### `patcher/` + `patch_format/` — patching

`patcher/manifest.rs` holds the hard-coded patch manifest (each entry: path, byte
size, CRC32) — validation reference data checked against whatever patches were
acquired, regardless of source. `PATCH_URL_BASE` is a historical record of the dead
`http://ffxivpatches.s3.amazonaws.com/` S3 archive the launcher used to download
from before the BitTorrent transport replaced it; nothing reads it anymore. Patches
are acquired one of two ways — the BitTorrent transport (below) or a user-selected
local directory — then `patcher/worker.rs` runs the apply step on a background
thread, reporting progress through lock-free shared state. The apply step calls
into `patch_format/` (the ZiPatch parser: zlib-decompress, apply file deltas), and
each file is CRC32-checked (`crc32fast`) against the manifest before it counts as
applied. When everything applies, the launcher writes the install's
`game.ver`/version files.

Patches can also arrive as a torrented archive - the launcher fetches a magnet
link from the patch-distribution endpoint (https://www.stegall.me/ffxiv/1.0/patches/torrent),
downloads the ffxiv_patches archive with an embedded librqbit session into the
user's patch storage folder (Documents by default, configurable in settings),
extracts the manifest patches from the zip, validates size + CRC32, and applies
them in order; while the launcher stays open the payload is seeded back to the
swarm unless the user opts out in settings.

### `login/` + `crypto/` — login and launch crypto

See [Client ↔ server communication](#client--server-communication) below.

### `launcher/` — assembling and starting the game

`launcher/game_launch.rs` defines the `GameLaunchRequest { game_dir, lobby_host,
session_id, wine_debug_override, enable_winsock_proxy }` and `launch_game()`, which
delegates to the current platform backend. `launcher/pe_patch.rs` defines the
launch-time PE patches applied to `ffxivgame.exe` — most importantly the **lobby
hostname** (so one client binary can target any server) and an encryption-time
constant; plus several stability patches used under Wine.

### `platform/` — per-OS backends

A `Platform` trait (`platform/mod.rs`) with `detect_game_install`,
`is_valid_game_location`, and `launch_game`; `platform::current()` returns the right
backend. See [Platform support](#platform-support).

---

## Client ↔ server communication

The launcher touches the server in three narrow places. Everything else is the
**game's** job after launch.

### 1. Login (WebView → custom scheme)

The login flow runs in a **separate subprocess** so the WebView's native event loop
stays isolated from the GUI:

- The launcher re-execs itself with `--login-webview <loginUrl>` (`main.rs` →
  `login::run_webview`). That child opens the server's `loginUrl` in a native
  WebView (`tao` event loop + `wry`).
- The child watches navigations for the custom scheme
  **`ffxiv://login_success?sessionId=<56-hex-chars>`**
  (`src/login/webview.rs`). On a match it prints a sentinel line to stdout:
  - `SESSION_ID=<id>` on success (`SESSION_PREFIX`),
  - `LOGIN_CANCELLED` if the user closes the window (`CANCEL_SENTINEL`),
  - `LOGIN_ERROR=<msg>` on failure (`ERROR_PREFIX`).
- The parent reads the child's stdout line by line on a worker thread and turns the
  sentinel into a `LoginOutcome` delivered to the GUI over an `mpsc` channel. The
  session id is a fixed **56** characters (`SESSION_ID_LEN`).

That `sessionId` is minted by the server's web auth (`web-server`), exactly as the
server's lobby will later expect — see the server architecture doc.

### 2. Patching (magnet endpoint + BitTorrent)

`ureq` is only used to fetch the magnet link from the patch-distribution endpoint
(`https://www.stegall.me/ffxiv/1.0/patches/torrent`); the ZiPatch payload itself
arrives over BitTorrent (or from a user-supplied local folder), not HTTP. This is
independent of which game server you pick.

### 3. Launch handoff (encrypted argv)

The launcher never opens a game socket itself. Instead it bakes the destination and
session into the game it starts:

- `crypto/` (`build_launch_arguments`) builds the game's launch argument string
  (language/region/server-UTC/**session id**), derives a **Blowfish** key from a
  tick count, encrypts the 8-byte blocks, base64-url-encodes the result, and wraps it
  as `sqex0002<base64>!////` — the exact format `ffxivgame.exe` decodes.
- `launcher/pe_patch.rs` writes the selected server's **lobby hostname** into the PE.
- The game then connects to that server's **lobby (54994) → world (54992) → map
  (1989)** with the handed-off session. The hard client-version gate is satisfied
  because the install was patched to `2012.09.19.0001`.

---

## Platform support

`platform/` abstracts the one genuinely OS-specific step — actually starting the
game:

| | Windows | macOS | Linux |
|---|---|---|---|
| Game runtime | Native Win32 (no Wine) | **Bundled, auto-downloaded** Wine runtime | **System** Wine (Wine 7+) |
| Wine prefix | n/a | managed under `data_dir/prefix/` | managed under `data_dir/prefix/` |
| Detect install | Registry (HKLM uninstall key) | `ffxivboot.exe` in the managed install | `ffxivboot.exe` in the install |
| Patch method | In-memory (`CreateProcess` suspended + `WriteProcessMemory`) | On-disk copy `ffxivgame.patched.exe` | On-disk copy |
| Notes | Builds **32-bit** (`i686`) to read the game's thread context; caps CPU affinity (1.x crashes with ≥16 logical CPUs) | Apple Silicon needs Rosetta 2 for the x86 Wine engine | needs the GTK3/WebKit2GTK stack |

The shared Wine plumbing lives in `platform/wine.rs` (`WineRuntime`, prefix init via
`wineboot --init`, the on-disk patch set, and the game launch that redirects Wine's
output to `data_dir/logs/wine.log`). macOS/Linux additionally support an optional
**winsock-tracing proxy** (`ws2_32-proxy/`, a separately-built DLL) gated behind a
developer preference — handy for inspecting the game's network traffic.

---

## Where to read next

- [`dev-environment.md`](dev-environment.md) — build/run, logging, launcher state, and
  running against a local Garlemald-Server.
- [`agents.md`](agents.md) — running an AI coding agent against an issue.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — pick up an issue and open a PR.
- [Garlemald-Server `docs/architecture.md`](https://github.com/swstegall/Garlemald-Server/blob/develop/docs/architecture.md)
  — the server side of the handoff.
