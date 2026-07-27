use zed_extension_api::{self as zed, Result, settings::LspSettings};

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
            return Err(format!("unsupported language server: {language_server_id}"));
        }

        let settings = LspSettings::for_worktree(LANGUAGE_SERVER_ID, worktree)?;

        let binary = settings.binary.as_ref();

        let command = match binary.and_then(|binary| binary.path.clone()) {
            Some(path) => path,

            None => worktree.which(ADAPTER_EXECUTABLE).ok_or_else(|| {
                format!(
                    "{ADAPTER_EXECUTABLE} was not found on PATH. \
                     Install it with `cargo install --path bridge`, \
                     or configure lsp.{LANGUAGE_SERVER_ID}.binary.path"
                )
            })?,
        };

        let mut args = binary
            .and_then(|binary| binary.arguments.clone())
            .unwrap_or_else(|| vec!["launch".into()]);

        if args.is_empty() {
            args.push("launch".into());
        }

        if args.first().map(String::as_str) == Some("launch")
            && !contains_option(&args, "--workspace")
        {
            args.push("--workspace".into());
            args.push(worktree.root_path());
        }

        Ok(zed::Command {
            command,
            args,
            env: worktree.shell_env(),
        })
    }
}

fn contains_option(arguments: &[String], option: &str) -> bool {
    arguments.iter().any(|argument| {
        argument == option
            || argument
                .strip_prefix(option)
                .is_some_and(|suffix| suffix.starts_with('='))
    })
}

zed::register_extension!(FourDExtension);
