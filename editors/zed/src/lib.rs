//! Zed extension for keron: registers the `.keron` language and
//! spawns `keron lsp` from the user's PATH as its language server.
//! Compiled by Zed to wasm32-wasip1; not part of the keron workspace.

use zed_extension_api::{self as zed, Result};

struct KeronExtension;

impl zed::Extension for KeronExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = worktree.which("keron").ok_or_else(|| {
            "`keron` was not found on PATH; install keron and restart Zed".to_string()
        })?;
        Ok(zed::Command {
            command,
            args: vec!["lsp".to_string()],
            env: Vec::new(),
        })
    }
}

zed::register_extension!(KeronExtension);
