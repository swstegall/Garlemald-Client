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

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_SERVERS_TOML: &str = include_str!("default_servers.toml");

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerDefinition {
    pub name: String,
    pub address: String,
    pub login_url: String,
    /// Base URL of a bahamut-style JSON auth API (e.g.
    /// `https://host/api/v1`) for servers with no web login page. When
    /// `login_url` is empty and this is set, the launcher shows its native
    /// login/sign-up form instead of the login webview.
    #[serde(default)]
    pub api_url: String,
}

/// Order-preserving: `iter()` yields entries in the order they appear in the
/// TOML file, which is also the dropdown order and the fresh-install default
/// (first entry).
#[derive(Debug, Clone, Default)]
pub struct ServerDefinitions {
    servers: Vec<ServerDefinition>,
}

#[derive(Debug, Deserialize, Default)]
struct ServersFile {
    #[serde(default, rename = "server")]
    servers: Vec<ServerDefinition>,
}

impl ServerDefinitions {
    pub fn load_from_path(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading servers file {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn load_default() -> Result<Self> {
        Self::parse(DEFAULT_SERVERS_TOML)
    }

    pub fn parse(toml_text: &str) -> Result<Self> {
        let parsed: ServersFile = toml::from_str(toml_text).context("parsing servers TOML")?;
        let mut servers: Vec<ServerDefinition> = Vec::new();
        for server in parsed.servers {
            if server.name.is_empty() {
                continue;
            }
            // Duplicate names keep the first occurrence's position but take
            // the later definition, matching the previous map semantics.
            match servers.iter_mut().find(|s| s.name == server.name) {
                Some(existing) => *existing = server,
                None => servers.push(server),
            }
        }
        Ok(Self { servers })
    }

    pub fn iter(&self) -> impl Iterator<Item = &ServerDefinition> {
        self.servers.iter()
    }

    pub fn get(&self, name: &str) -> Option<&ServerDefinition> {
        self.servers.iter().find(|s| s.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_servers() {
        let defs = ServerDefinitions::load_default().unwrap();
        let local = defs.get("Localhost").expect("Localhost present");
        assert_eq!(local.address, "127.0.0.1");
        assert_eq!(local.login_url, "http://127.0.0.1:54993/login");
        // Bahamut (main) is deliberately first: file order is the dropdown
        // order and the fresh-install default selection.
        let first = defs.iter().next().expect("at least one server");
        assert_eq!(first.name, "Bahamut (main)");

        // Project Meteor's PHP login_su runs at 8080, not 54993 (which is
        // the Map Server's game-protocol TCP listener in that codebase).
        let meteor = defs.get("Project Meteor").expect("Project Meteor present");
        assert_eq!(meteor.login_url, "http://127.0.0.1:8080/login.php");
    }

    #[test]
    fn parses_multiple_servers() {
        let toml_text = r#"
[[server]]
name = "A"
address = "a.example"
login_url = "https://a/login"

[[server]]
name = "B"
address = "b.example"
login_url = "https://b/login"
"#;
        let defs = ServerDefinitions::parse(toml_text).unwrap();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs.get("A").unwrap().address, "a.example");
        assert_eq!(defs.get("B").unwrap().login_url, "https://b/login");
    }

    #[test]
    fn empty_document_is_empty_set() {
        let defs = ServerDefinitions::parse("").unwrap();
        assert!(defs.is_empty());
    }

    #[test]
    fn iteration_preserves_file_order() {
        let toml_text = r#"
[[server]]
name = "Zeta"
address = "z.example"
login_url = "https://z/login"

[[server]]
name = "Alpha"
address = "a.example"
login_url = "https://a/login"
"#;
        let defs = ServerDefinitions::parse(toml_text).unwrap();
        let names: Vec<&str> = defs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Zeta", "Alpha"]);
    }

    #[test]
    fn bahamut_main_is_default_and_uses_the_native_json_login() {
        let defs = ServerDefinitions::load_default().unwrap();
        let entry = defs.get("Bahamut (main)").expect("Bahamut (main) present");
        assert_eq!(entry.address, "bahamut.stegall.me");
        // The bahamut auth service is a JSON API with no web login page:
        // empty login_url plus an api_url routes the Login button to the
        // native login/sign-up form. https because the client refuses to
        // send passwords over plain http to non-loopback hosts.
        assert_eq!(entry.login_url, "");
        assert_eq!(entry.api_url, "https://bahamut.stegall.me/api/v1");
    }

    #[test]
    fn api_url_defaults_to_empty_for_webview_servers() {
        let defs = ServerDefinitions::load_default().unwrap();
        assert_eq!(defs.get("Localhost").unwrap().api_url, "");
    }

    #[test]
    fn aetherxiv_13_entry_points_at_docker_login() {
        let defs = ServerDefinitions::load_default().unwrap();
        let entry = defs
            .get("AetherXIV 1.3 (Docker Local)")
            .expect("AetherXIV 1.3 present");
        assert_eq!(entry.address, "127.0.0.1");
        assert_eq!(entry.login_url, "http://127.0.0.1:8080/login/index.php");
    }
}
