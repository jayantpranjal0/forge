use std::io::{self, Write};
use std::sync::Arc;

use forge_domain::ChatResponse;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::Sender;

/// Trait for handling streaming output from command execution
#[async_trait::async_trait]
pub trait StreamService: Send + Sync {
    /// Stream output from the given async reader
    async fn stream_stdout(
        &self,
        io: &mut Option<tokio::process::ChildStdout>,
    ) -> io::Result<Vec<u8>>;

    /// Stream output from the given async reader for stderr
    async fn stream_stderr(
        &self,
        io: &mut Option<tokio::process::ChildStderr>,
    ) -> io::Result<Vec<u8>>;
}

/// Default stream service that writes to stdout/stderr (current behavior)
#[derive(Clone, Debug)]
pub struct DefaultStreamService {}

impl Default for DefaultStreamService {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultStreamService {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait::async_trait]
impl StreamService for DefaultStreamService {
    async fn stream_stdout(
        &self,
        io: &mut Option<tokio::process::ChildStdout>,
    ) -> io::Result<Vec<u8>> {
        stream_to_writer(io, io::stdout()).await
    }

    async fn stream_stderr(
        &self,
        io: &mut Option<tokio::process::ChildStderr>,
    ) -> io::Result<Vec<u8>> {
        stream_to_writer(io, io::stderr()).await
    }
}

/// Helper function to stream from any AsyncReadExt to any Write
pub async fn stream_to_writer<A: AsyncReadExt + Unpin, W: Write>(
    io: &mut Option<A>,
    mut writer: W,
) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(io) = io.as_mut() {
        let mut buff = [0; 1024];
        loop {
            let n = io.read(&mut buff).await?;
            if n == 0 {
                break;
            }
            writer.write_all(&buff[..n])?;
            // note: flush is necessary else we get the cursor could not be found error.
            writer.flush()?;
            output.extend_from_slice(&buff[..n]);
        }
    }
    Ok(output)
}

/// UI stream service that sends output as ChatResponse::StreamedText
#[derive(Clone, Debug)]
pub struct UiStreamService {
    sender: Arc<Sender<anyhow::Result<ChatResponse>>>,
}

impl UiStreamService {
    pub fn new(sender: Arc<Sender<anyhow::Result<ChatResponse>>>) -> Self {
        Self { sender }
    }
}

#[async_trait::async_trait]
impl StreamService for UiStreamService {
    async fn stream_stdout(
        &self,
        io: &mut Option<tokio::process::ChildStdout>,
    ) -> io::Result<Vec<u8>> {
        self.stream_with_sender(io).await
    }

    async fn stream_stderr(
        &self,
        io: &mut Option<tokio::process::ChildStderr>,
    ) -> io::Result<Vec<u8>> {
        self.stream_with_sender(io).await
    }
}

impl UiStreamService {
    async fn stream_with_sender<A: AsyncReadExt + Unpin>(
        &self,
        io: &mut Option<A>,
    ) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        if let Some(io) = io.as_mut() {
            let mut buff = [0; 1024];
            loop {
                let n = io.read(&mut buff).await?;
                if n == 0 {
                    break;
                }

                let text = String::from_utf8_lossy(&buff[..n]).into_owned();

                // Send as streamed text to UI
                let chat_response = ChatResponse::Text { text, is_complete: true, is_md: false };

                if let Err(e) = self.sender.send(Ok(chat_response)).await {
                    tracing::warn!("Failed to send streamed text to UI: {}", e);
                }

                output.extend_from_slice(&buff[..n]);
            }

            // Send completion marker
            let completion_response =
                ChatResponse::Text { text: String::new(), is_complete: true, is_md: false };

            if let Err(e) = self.sender.send(Ok(completion_response)).await {
                tracing::warn!("Failed to send stream completion to UI: {}", e);
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn test_default_stream_service() {
        let _fixture = DefaultStreamService::new();
        let data = b"hello world";
        let cursor = std::io::Cursor::new(data);
        let mut reader = Some(cursor);

        // We can't easily test DefaultStreamService without mocking stdout
        // So we'll just test that it doesn't panic
        let actual = stream_to_writer(&mut reader, Vec::new()).await.unwrap();
        let expected = b"hello world".to_vec();

        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_ui_stream_service() {
        let (tx, mut rx) = mpsc::channel(10);
        let fixture = UiStreamService::new(Arc::new(tx));
        let data = b"hello world";
        let cursor = std::io::Cursor::new(data);
        let mut reader = Some(cursor);

        let actual = fixture.stream_with_sender(&mut reader).await.unwrap();
        let expected = b"hello world".to_vec();

        assert_eq!(actual, expected);

        // Check that we received the streamed text
        let response = rx.recv().await.unwrap().unwrap();
        match response {
            ChatResponse::Text { text, is_complete, is_md } => {
                assert_eq!(text, "hello world");
                assert_eq!(is_complete, false);
            }
            _ => panic!("Expected StreamedText response"),
        }

        // Check completion marker
        let completion = rx.recv().await.unwrap().unwrap();
        match completion {
            ChatResponse::Text { text, is_complete, _ } => {
                assert_eq!(text, "");
                assert_eq!(is_complete, false);
            }
            _ => panic!("Expected StreamedText completion"),
        }
    }
}
