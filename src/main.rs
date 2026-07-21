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

use anyhow::{Result, anyhow};

fn main() -> Result<()> {
    // Surface panics that happen inside FFI callbacks (e.g. AppKit's
    // `applicationDidFinishLaunching`) where `panic_cannot_unwind` would
    // otherwise abort before the message is printed.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("PANIC: {info}");
        eprintln!("{}", std::backtrace::Backtrace::force_capture());
    }));

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stdout)
        .init();

    let mut args = std::env::args().skip(1);
    if let Some(first) = args.next()
        && first == "--login-webview"
    {
        let url = args
            .next()
            .ok_or_else(|| anyhow!("--login-webview requires a URL argument"))?;
        return garlemald_client::login::run_webview(&url);
    }

    garlemald_client::run()
}
