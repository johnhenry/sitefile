//! Starts `sitefile lsp` for Sitefiles.
//!
//! The binary is `lsp.sitefile-lsp.binary.path` from the Zed settings if set,
//! otherwise `sitefile` found on the worktree's PATH. Arguments default to
//! `["lsp"]` (`lsp.sitefile-lsp.binary.arguments` overrides them).

use zed_extension_api::{self as zed, settings::LspSettings, LanguageServerId, Result};

const BINARY: &str = "sitefile";
const DEFAULT_ARGS: &[&str] = &["lsp"];

struct SitefileExtension;

impl zed::Extension for SitefileExtension {
    fn new() -> Self {
        SitefileExtension
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|settings| settings.binary);
        let configured_path = binary.as_ref().and_then(|b| b.path.clone());
        let arguments = binary.as_ref().and_then(|b| b.arguments.clone());
        let extra_env = binary.and_then(|b| b.env);

        let command = match configured_path {
            Some(path) => path,
            None => worktree.which(BINARY).ok_or_else(|| {
                format!(
                    "`{BINARY}` was not found on PATH. The Sitefile language server is the Rust \
                     binary: install it with `cargo install --path rust` from a clone of \
                     https://github.com/johnhenry/sitefile, or set \
                     `lsp.{id}.binary.path` in your Zed settings.",
                    id = language_server_id.as_ref()
                )
            })?,
        };

        let mut env = worktree.shell_env();
        if let Some(extra) = extra_env {
            env.extend(extra);
        }

        Ok(zed::Command {
            command,
            args: arguments.unwrap_or_else(|| DEFAULT_ARGS.iter().map(|s| s.to_string()).collect()),
            env,
        })
    }
}

zed::register_extension!(SitefileExtension);
