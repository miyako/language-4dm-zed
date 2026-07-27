use std::path::Path;

use zed_extension_api::{
    self as zed, Architecture, DownloadedFileType, GithubReleaseOptions, LanguageServerId, Os,
    Result, Worktree,
};

const LANGUAGE_SERVER_ID: &str = "tool4d";
const GITHUB_REPOSITORY: &str = "miyako/language-4dm-zed";
const ADAPTER_DIRECTORY: &str = "language_servers";

struct FourDExtension {
    cached_adapter_path: Option<String>,
}

impl FourDExtension {
    fn configured_or_downloaded_adapter(
        &mut self,
        worktree: &Worktree,
    ) -> Result<(String, Vec<String>)> {
        let settings = zed::settings::LspSettings::for_worktree(LANGUAGE_SERVER_ID, worktree)?;

        let configured_binary = settings.binary;

        let configured_path = configured_binary
            .as_ref()
            .and_then(|binary| binary.path.clone());

        let configured_arguments = configured_binary
            .as_ref()
            .and_then(|binary| binary.arguments.clone())
            .unwrap_or_default();

        if let Some(path) = configured_path {
            return Ok((path, configured_arguments));
        }

        let adapter_path = self.downloaded_adapter_path()?;

        Ok((adapter_path, configured_arguments))
    }

    fn downloaded_adapter_path(&mut self) -> Result<String> {
        if let Some(path) = self.cached_adapter_path.as_ref()
            && Path::new(path).is_file()
        {
            return Ok(path.clone());
        }

        let (os, architecture) = zed::current_platform();

        let asset_name = adapter_asset_name(os, architecture)?;

        let release = zed::latest_github_release(
            GITHUB_REPOSITORY,
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "release {} of {GITHUB_REPOSITORY} does not contain \
                     the required adapter asset {asset_name}",
                    release.version
                )
            })?;

        let safe_version = sanitize_path_component(&release.version);

        let installation_directory = format!("{ADAPTER_DIRECTORY}/tool4d-lsp-stdio-{safe_version}");

        /*
         * Both release archives currently contain a top-level directory
         * named `tool4d-lsp-stdio`.
         */
        let executable_name = if os == Os::Windows {
            "tool4d-lsp-stdio.exe"
        } else {
            "tool4d-lsp-stdio"
        };

        let adapter_path = format!("{installation_directory}/tool4d-lsp-stdio/{executable_name}");

        if !Path::new(&adapter_path).is_file() {
            eprintln!(
                "4D extension: downloading adapter {} from release {}",
                asset.name, release.version
            );

            if Path::new(&installation_directory).exists() {
                std::fs::remove_dir_all(&installation_directory).map_err(|error| {
                    format!(
                        "failed to remove incomplete adapter directory \
                             {installation_directory}: {error}"
                    )
                })?;
            }

            zed::download_file(
                &asset.download_url,
                &installation_directory,
                DownloadedFileType::Zip,
            )
            .map_err(|error| format!("failed to download adapter asset {}: {error}", asset.name))?;

            if !Path::new(&adapter_path).is_file() {
                return Err(format!(
                    "adapter archive {} was downloaded, but the expected \
                     executable was not found at {adapter_path}",
                    asset.name
                ));
            }
        }

        if os != Os::Windows {
            zed::make_file_executable(&adapter_path).map_err(|error| {
                format!(
                    "failed to make adapter executable at \
                     {adapter_path}: {error}"
                )
            })?;
        }

        self.cached_adapter_path = Some(adapter_path.clone());

        Ok(adapter_path)
    }
}

impl zed::Extension for FourDExtension {
    fn new() -> Self {
        Self {
            cached_adapter_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<zed::Command> {
        if language_server_id.as_ref() != LANGUAGE_SERVER_ID {
            return Err(format!("unsupported language server: {language_server_id}"));
        }

        let (command, configured_arguments) = self.configured_or_downloaded_adapter(worktree)?;

        let mut arguments = normalize_launch_arguments(configured_arguments, worktree.root_path());

        /*
         * Avoid retaining excess capacity in data passed across the
         * WebAssembly component boundary.
         */
        arguments.shrink_to_fit();

        Ok(zed::Command {
            command,
            args: arguments,
            env: worktree.shell_env(),
        })
    }
}

fn adapter_asset_name(os: Os, architecture: Architecture) -> Result<&'static str> {
    match (os, architecture) {
        (Os::Mac, Architecture::Aarch64) => Ok("tool4d-lsp-stdio-aarch64-apple-darwin.zip"),

        (Os::Windows, Architecture::X8664) => Ok("tool4d-lsp-stdio-x86_64-pc-windows-msvc.zip"),

        (os, architecture) => Err(format!(
            "tool4d-lsp-stdio is not available for \
             {os:?}/{architecture:?}"
        )),
    }
}

fn normalize_launch_arguments(mut arguments: Vec<String>, worktree_root: String) -> Vec<String> {
    /*
     * Zed users may configure only adapter options, for example:
     *
     *   ["--tool", "/path/to/tool4d"]
     *
     * Add the `launch` subcommand automatically unless a subcommand was
     * provided explicitly.
     */
    let has_subcommand = arguments
        .first()
        .is_some_and(|argument| argument == "launch" || argument == "connect");

    if !has_subcommand {
        arguments.insert(0, "launch".to_owned());
    }

    /*
     * The connect subcommand does not accept project/workspace arguments.
     * It is intended only as an advanced manual override.
     */
    if arguments
        .first()
        .is_some_and(|argument| argument == "connect")
    {
        return arguments;
    }

    if !has_option(&arguments, "--workspace") && !has_option(&arguments, "--project") {
        arguments.push("--workspace".to_owned());
        arguments.push(worktree_root);
    }

    arguments
}

fn has_option(arguments: &[String], option: &str) -> bool {
    arguments.iter().any(|argument| {
        argument == option
            || argument
                .strip_prefix(option)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

zed::register_extension!(FourDExtension);
