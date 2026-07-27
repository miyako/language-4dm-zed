use zed_extension_api::{self as zed, Result};

const LANGUAGE_SERVER_ID: &str = "tool4d";
const ADAPTER_EXECUTABLE: &str = "tool4d-lsp-stdio";

struct FourDExtension;

impl zed::Extension for FourDExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        if language_server_id.as_ref() != LANGUAGE_SERVER_ID {
            return Err(format!(
                "unsupported language server: {language_server_id}"
            ));
        }

        let command = worktree.which(ADAPTER_EXECUTABLE).ok_or_else(|| {
            format!(
                "{ADAPTER_EXECUTABLE} was not found on PATH. \
                 Install the adapter before starting the 4D language server."
            )
        })?;

        Ok(zed::Command {
            command,
            args: vec![
                "launch".into(),
                "--workspace".into(),
                worktree.root_path(),
            ],
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(FourDExtension);