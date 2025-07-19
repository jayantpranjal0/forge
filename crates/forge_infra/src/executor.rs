use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_domain::{CommandOutput, Environment};
use forge_services::CommandInfra;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::stream_service::{StreamService, stream_to_writer};

/// Service for executing shell commands
#[derive(Clone)]
pub struct ForgeCommandExecutorService {
    restricted: bool,
    env: Environment,
    stdout_stream_service: Option<Arc<dyn StreamService>>,
    stderr_stream_service: Option<Arc<dyn StreamService>>,

    // Mutex to ensure that only one command is executed at a time
    ready: Arc<Mutex<()>>,
}

impl ForgeCommandExecutorService {
    pub fn new(restricted: bool, env: Environment) -> Self {
        Self {
            restricted,
            env,
            stdout_stream_service: None,
            stderr_stream_service: None,
            ready: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_stream_services(
        restricted: bool,
        env: Environment,
        stdout_stream_service: Option<Arc<dyn StreamService>>,
        stderr_stream_service: Option<Arc<dyn StreamService>>,
    ) -> Self {
        Self {
            restricted,
            env,
            stdout_stream_service,
            stderr_stream_service,
            ready: Arc::new(Mutex::new(())),
        }
    }

    fn prepare_command(&self, command_str: &str, working_dir: Option<&Path>) -> Command {
        // Create a basic command
        let is_windows = cfg!(target_os = "windows");
        let shell = if self.restricted && !is_windows {
            "rbash"
        } else {
            self.env.shell.as_str()
        };
        let mut command = Command::new(shell);

        // Core color settings for general commands
        command
            .env("CLICOLOR_FORCE", "1")
            .env("FORCE_COLOR", "true")
            .env_remove("NO_COLOR");

        // Language/program specific color settings
        command
            .env("SBT_OPTS", "-Dsbt.color=always")
            .env("JAVA_OPTS", "-Dsbt.color=always");

        // enabled Git colors
        command.env("GIT_CONFIG_PARAMETERS", "'color.ui=always'");

        // Other common tools
        command.env("GREP_OPTIONS", "--color=always"); // GNU grep

        let parameter = if is_windows { "/C" } else { "-c" };
        command.arg(parameter);

        #[cfg(windows)]
        command.raw_arg(command_str);
        #[cfg(unix)]
        command.arg(command_str);

        tracing::info!(command = command_str, "Executing command");

        command.kill_on_drop(true);

        // Set the working directory
        if let Some(working_dir) = working_dir {
            command.current_dir(working_dir);
        }

        // Configure the command for output
        command
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        command
    }

    /// Internal method to execute commands with streaming to console
    async fn execute_command_internal(
        &self,
        command: String,
        working_dir: &Path,
    ) -> anyhow::Result<CommandOutput> {
        let ready = self.ready.lock().await;

        let mut prepared_command = self.prepare_command(&command, Some(working_dir));

        // Spawn the command
        let mut child = prepared_command.spawn()?;

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        // Stream the output of the command using stream services or default behavior
        let (status, stdout_buffer, stderr_buffer) = tokio::try_join!(
            child.wait(),
            self.handle_stdout_stream(&mut stdout_pipe),
            self.handle_stderr_stream(&mut stderr_pipe)
        )?;

        // Drop happens after `try_join` due to <https://github.com/tokio-rs/tokio/issues/4309>
        drop(stdout_pipe);
        drop(stderr_pipe);
        drop(ready);

        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout_buffer).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_buffer).into_owned(),
            exit_code: status.code(),
            command,
        })
    }

    async fn handle_stdout_stream(
        &self,
        io: &mut Option<tokio::process::ChildStdout>,
    ) -> io::Result<Vec<u8>> {
        if let Some(ref stream_service) = self.stdout_stream_service {
            stream_service.stream_stdout(io).await
        } else {
            stream_to_writer(io, io::stdout()).await
        }
    }

    async fn handle_stderr_stream(
        &self,
        io: &mut Option<tokio::process::ChildStderr>,
    ) -> io::Result<Vec<u8>> {
        if let Some(ref stream_service) = self.stderr_stream_service {
            stream_service.stream_stderr(io).await
        } else {
            stream_to_writer(io, io::stderr()).await
        }
    }
}

/// The implementation for CommandExecutorService
#[async_trait::async_trait]
impl CommandInfra for ForgeCommandExecutorService {
    async fn execute_command(
        &self,
        command: String,
        working_dir: PathBuf,
    ) -> anyhow::Result<CommandOutput> {
        self.execute_command_internal(command, &working_dir).await
    }

    async fn execute_command_raw(&self, command: &str) -> anyhow::Result<std::process::ExitStatus> {
        let mut prepared_command = self.prepare_command(command, None);

        // overwrite the stdin, stdout and stderr to inherit
        prepared_command
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        Ok(prepared_command.spawn()?.wait().await?)
    }
}

#[cfg(test)]
mod tests {

    use pretty_assertions::assert_eq;
    use reqwest::Url;

    use super::*;

    fn test_env() -> Environment {
        Environment {
            os: "test".to_string(),
            pid: 12345,
            cwd: PathBuf::from("/test"),
            home: Some(PathBuf::from("/home/test")),
            shell: if cfg!(target_os = "windows") {
                "cmd"
            } else {
                "bash"
            }
            .to_string(),
            base_path: PathBuf::from("/base"),
            retry_config: Default::default(),
            fetch_truncation_limit: 0,
            stdout_max_prefix_length: 0,
            max_search_lines: 0,
            max_read_size: 0,
            stdout_max_suffix_length: 0,
            http: Default::default(),
            max_file_size: 10_000_000,
            forge_api_url: Url::parse("http://forgecode.dev/api").unwrap(),
        }
    }

    #[tokio::test]
    async fn test_command_executor() {
        let fixture = ForgeCommandExecutorService::new(false, test_env());
        let cmd = "echo 'hello world'";
        let dir = ".";

        let actual = fixture
            .execute_command(cmd.to_string(), PathBuf::new().join(dir))
            .await
            .unwrap();

        let mut expected = CommandOutput {
            stdout: "hello world\n".to_string(),
            stderr: "".to_string(),
            command: "echo \"hello world\"".into(),
            exit_code: Some(0),
        };

        if cfg!(target_os = "windows") {
            expected.stdout = format!("'{}'", expected.stdout);
        }

        assert_eq!(actual.stdout.trim(), expected.stdout.trim());
        assert_eq!(actual.stderr, expected.stderr);
        assert_eq!(actual.success(), expected.success());
    }
}
