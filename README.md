# Garlemald Client

[![License: AGPL v3](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE.md)
[![Rust](https://img.shields.io/badge/rust-1.95-orange.svg)](rust-toolchain.toml)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#build)
[![Discord](https://img.shields.io/badge/discord-join-5865F2.svg)](https://discord.gg/CVjwWs6jnX)

A cross-platform launcher for **FINAL FANTASY XIV v1.23b** — the
original 1.0 iteration of the game, not A Realm Reborn.

Garlemald Client detects an installed 1.x client, patches it forward to
`2012.09.19.0001`, runs the login handshake against a private server,
and launches `ffxivgame.exe` from the same Rust codebase on macOS
(including Apple Silicon), Linux, and Windows. On macOS and Linux it
also downloads and manages its own Wine runtime, so there is nothing
to install beyond the launcher itself.

> Created with [Claude](https://claude.ai/).

## Highlights

- Single Rust codebase targeting macOS (Apple Silicon and Intel),
  Linux, and Windows
- Automatic detection of existing FFXIV 1.x installs — CrossOver
  bottles, Whisky prefixes, and manual Wine installs all work
- Self-managed Wine runtime on non-Windows hosts; no external Wine
  setup required
- CRC32-verified patch download and ZiPatch apply, `2010.09.18.0000` →
  `2012.09.19.0001`
- Embedded WebView login flow with session-token handoff to the game
  binary
- Lobby hostname injected into the PE at launch time, so the same
  client binary can target any private server

## Build

Requires Rust 1.95.0 (pinned in `rust-toolchain.toml`; `rustup`
installs it automatically on first build). On Linux, `gtk3` and
`webkit2gtk-4.1` runtime libraries are needed for the login WebView.

```bash
cargo build --release
cargo run --release
```

For a distributable macOS `.app` bundle:

```bash
scripts/package-macos.sh               # host arch, ad-hoc signed
scripts/package-macos.sh --universal   # x86_64 + aarch64 fat binary
```

On Windows, the launcher must be built as 32-bit (`i686`) so it can
read the suspended thread context of the 32-bit `ffxivgame.exe` and
patch it at launch — a 64-bit build is rejected at compile time.
Requires the MSVC C++ x86 build tools and [NASM](https://www.nasm.us/)
on `PATH` (`winget install nasm`), which `aws-lc-sys` uses to assemble
its crypto.

```powershell
rustup target add i686-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc
cargo run   --release --target i686-pc-windows-msvc
```

> Tip: create a local (gitignored) `.cargo/config.toml` with
> `[build]` → `target = "i686-pc-windows-msvc"` so plain `cargo build` /
> `cargo run` default to 32-bit. See [docs/dev-environment.md](docs/dev-environment.md).

## Documentation

New contributor? These docs take you from zero to building the launcher to opening a
pull request:

- **[Contributing guide](CONTRIBUTING.md)** — request access, pick an issue, fork,
  and open a PR (start here).
- **[Architecture](docs/architecture.md)** — the launcher pipeline (detect → patch →
  Wine → WebView login → launch), the module map, and how the client talks to a server.
- **[Developer environment](docs/dev-environment.md)** — per-OS build/run, `RUST_LOG`
  and Wine-log toggles, where launcher state lives + how to reset it, and running
  against a local Garlemald-Server.
- **[Working an issue with an AI agent](docs/agents.md)** — Claude / OpenAI setup; the
  in-repo [`CLAUDE.md`](CLAUDE.md) / [`AGENTS.md`](AGENTS.md) tell the agent the house
  rules.
- **[Releasing](docs/RELEASING.md)** — the `develop` → `main` branching model and
  release automation.

## Contributing

Contributions are welcome. The short version: ask for collaborator + project-board
access on [Discord](https://discord.gg/CVjwWs6jnX), pick an issue from the board's
**Ready** column, branch off **`develop`**, keep CI green
(`fmt` / `clippy` / `build` / `test`), and open a PR into **`develop`**. The full
walkthrough is in **[`CONTRIBUTING.md`](CONTRIBUTING.md)**.

## Attribution and licensing

Garlemald Client derives from upstream projects under copyleft and
permissive licenses. See [`NOTICE.md`](NOTICE.md) for attribution to
Project Meteor Server, Seventh Umbral, and the wider FFXIV 1.0
preservationist community — plus a companion acknowledgment of
[LandSandBoat](https://github.com/LandSandBoat/server) (and its
**DarkStar Project** ancestor), the FFXI server emulator that the
sister project `garlemald-server` uses as its structural reference
for the XI-inherited portions of FFXIV 1.x gameplay. See
[`LICENSE.md`](LICENSE.md) for the full terms of the
**GNU Affero General Public License, version 3 or later**, under
which this project is distributed.

## Sister projects

- **[meteor-decomp](https://github.com/swstegall/meteor-decomp)** —
  clean-room decompilation of the FFXIV 1.23b Windows client binaries,
  producing byte-identical recompiles and validated wire-protocol
  ground truth.
- **[decomp-agents](https://github.com/swstegall/decomp-agents)** —
  parallel autonomous Claude agents that grind through meteor-decomp's
  per-function matching workflow via a shared claim queue.
- **[Garlemald Server](https://github.com/swstegall/Garlemald-Server)** —
  the Rust FFXIV 1.23b server (lobby / world / map) this launcher is
  designed to connect to.
- **[XIV 1.0 Apple Silicon Installer](https://github.com/swstegall/XIV-1.0-Apple-Silicon-Installer)** —
  helper for getting a working FFXIV 1.x install on Apple Silicon Macs,
  which Garlemald Client can then detect and drive.
- **[XIV 1.0 Linux Installer](https://github.com/swstegall/XIV-1.0-Linux-Installer)** —
  helper for getting a working FFXIV 1.x install on x86_64 Linux, which
  Garlemald Client can then detect and drive.

## Community

Questions, bug reports, or just want to talk about the project?
Join the Discord:

<https://discord.gg/CVjwWs6jnX>
