//! Minimal Zed extension: register the `keron-lsp` language server
//! by spawning `keron lsp`. The user is expected to have the keron
//! binary on their `PATH` (e.g. via `cargo install --path
//! crates/keron-cli`); the extension does not download or build
//! anything itself.

use zed_extension_api as zed;

struct KeronExtension;

impl zed::Extension for KeronExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let _ = (language_server_id, worktree);
        let path = worktree
            .which("keron")
            .ok_or_else(|| "`keron` binary not found on PATH".to_string())?;
        Ok(zed::Command {
            command: path,
            args: vec!["lsp".into()],
            env: Vec::new(),
        })
    }
}

zed::register_extension!(KeronExtension);
