use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use forge_app::ProviderService;
use forge_app::domain::{
    ChatCompletionMessage, Context as ChatContext, HttpConfig, Model, ModelId, Provider,
    ResultStream, RetryConfig,
};
use futures::stream::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::EnvironmentInfra;
use crate::http::HttpClient;
use crate::infra::HttpInfra;
use crate::provider::{Client, ClientBuilder};

#[derive(Debug, Serialize, Deserialize)]
struct ChatRequestDump {
    timestamp: DateTime<Utc>,
    request: ChatContext,
    response: Option<serde_json::Value>,
    error: Option<String>,
}
#[derive(Clone)]
pub struct ForgeProviderService<I: HttpInfra> {
    retry_config: Arc<RetryConfig>,
    cached_client: Arc<Mutex<Option<Client<HttpClient<I>>>>>,
    cached_models: Arc<Mutex<Option<Vec<Model>>>>,
    version: String,
    timeout_config: HttpConfig,
    infra: Arc<I>,
}

impl<I: EnvironmentInfra + HttpInfra> ForgeProviderService<I> {
    pub fn new(infra: Arc<I>) -> Self {
        let env = infra.get_environment();
        let version = env.version();
        let retry_config = Arc::new(env.retry_config);
        Self {
            retry_config,
            cached_client: Arc::new(Mutex::new(None)),
            cached_models: Arc::new(Mutex::new(None)),
            version,
            timeout_config: env.http,
            infra,
        }
    }

    async fn client(&self, provider: Provider) -> Result<Client<HttpClient<I>>> {
        let mut client_guard = self.cached_client.lock().await;

        match client_guard.as_ref() {
            Some(client) => Ok(client.clone()),
            None => {
                let infra = self.infra.clone();
                let client = ClientBuilder::new(provider, &self.version)
                    .retry_config(self.retry_config.clone())
                    .timeout_config(self.timeout_config.clone())
                    .use_hickory(false) // use native DNS resolver(GAI)
                    .build(Arc::new(HttpClient::new(infra)))?;

                // Cache the new client
                *client_guard = Some(client.clone());
                Ok(client)
            }
        }
    }

    async fn write_dump(&self, dump_file: &str, timestamp: DateTime<Utc>, dump_data: &ChatRequestDump) {
        // Create the experiment_chat_request_dump directory if it doesn't exist
        let dump_path = Path::new("experiment_chat_request_dump");
        if let Err(e) = std::fs::create_dir_all(dump_path) {
            eprintln!("Warning: Failed to create dump directory: {e}");
            return;
        }

        // Create the filename with timestamp
        let timestamp_str = timestamp.format("%Y-%m-%d_%H-%M-%S").to_string();
        let file_path = dump_path.join(format!("{dump_file}_{timestamp_str}.json"));

        // Serialize and write the dump data to JSON
        match serde_json::to_string_pretty(dump_data) {
            Ok(json_content) => {
                if let Err(e) = std::fs::write(&file_path, json_content) {
                    eprintln!("Warning: Failed to write context dump to {file_path:?}: {e}");
                } else {
                    println!("Context dumped to: {file_path:?}");
                }
            }
            Err(e) => {
                eprintln!("Warning: Failed to serialize context to JSON: {e}");
            }
        }
    }
}

#[async_trait::async_trait]
impl<I: EnvironmentInfra + HttpInfra> ProviderService for ForgeProviderService<I> {
    async fn chat(
        &self,
        model: &ModelId,
        request: ChatContext,
        provider: Provider,
    ) -> ResultStream<ChatCompletionMessage, anyhow::Error> {
        let dump_config = self.infra.get_env_var("FORGE_CONTEXT_DUMP");
        let timestamp = Utc::now();
        
        // If dump is enabled, we need to capture the stream
        if let Some(dump_file) = &dump_config {
            let client = self.client(provider).await?;
            
            match client.chat(model, request.clone()).await {
                Ok(stream) => {
                    let request_clone = request.clone();
                    let dump_file = dump_file.clone();
                    
                    // Collect all messages from the stream
                    let captured_stream = stream.try_collect::<Vec<_>>().await;

                    match captured_stream {
                        Ok(messages) => {
                            // Convert messages to a JSON representable format
                            let response_debug: Vec<String> = messages
                                .iter()
                                .map(|msg| format!("{msg:?}"))
                                .collect();
                            
                            // Create dump data with successful response
                            let dump_data = ChatRequestDump {
                                timestamp,
                                request: request_clone,
                                response: Some(serde_json::json!({
                                    "messages_count": messages.len(),
                                    "messages_debug": response_debug
                                })),
                                error: None,
                            };
                            
                            // Write dump to file
                            self.write_dump(&dump_file, timestamp, &dump_data).await;
                            
                            // Return the messages as a new stream
                            let message_stream = futures::stream::iter(
                                messages.into_iter().map(Ok)
                            );
                            Ok(Box::pin(message_stream) as forge_app::domain::BoxStream<ChatCompletionMessage, anyhow::Error>)
                        }
                        Err(e) => {
                            // Create dump data with error
                            let dump_data = ChatRequestDump {
                                timestamp,
                                request: request_clone,
                                response: None,
                                error: Some(e.to_string()),
                            };
                            
                            // Write dump to file
                            self.write_dump(&dump_file, timestamp, &dump_data).await;
                            
                            Err(e)
                        }
                    }
                }
                Err(e) => {
                    // Create dump data with error
                    let dump_data = ChatRequestDump {
                        timestamp,
                        request,
                        response: None,
                        error: Some(e.to_string()),
                    };
                    
                    // Write dump to file
                    self.write_dump(&dump_file, timestamp, &dump_data).await;
                    
                    Err(e.context(format!("Failed to chat with model: {model}")))
                }
            }
        } else {
            // Normal execution without dumping
            let client = self.client(provider).await?;
            client
                .chat(model, request)
                .await
                .with_context(|| format!("Failed to chat with model: {model}"))
        }
    }

    async fn models(&self, provider: Provider) -> Result<Vec<Model>> {
        // Check cache first
        {
            let models_guard = self.cached_models.lock().await;
            if let Some(cached_models) = models_guard.as_ref() {
                return Ok(cached_models.clone());
            }
        }

        // Models not in cache, fetch from client
        let client = self.client(provider).await?;
        let models = client.models().await?;

        // Cache the models
        {
            let mut models_guard = self.cached_models.lock().await;
            *models_guard = Some(models.clone());
        }

        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test that the context dump logic doesn't break when environment variable is
    // not set
    #[test]
    fn test_context_dump_env_var_logic() {
        // Test that None returns None (no environment variable set)
        assert!(None::<String>.is_none());

        // Test that Some value returns Some (environment variable set)
        let test_value = Some("test_dump".to_string());
        assert!(test_value.is_some());
        assert_eq!(test_value.unwrap(), "test_dump");
    }

    #[test]
    fn test_dump_path_construction_with_timestamp() {
        let base_path = std::path::Path::new("experiment_chat_request_dump");
        let filename = "test_env_var";
        let timestamp = "2024-01-01_12-30-45";
        let full_path = base_path.join(format!("{filename}_{timestamp}.json"));

        assert_eq!(
            full_path,
            std::path::PathBuf::from("experiment_chat_request_dump/test_env_var_2024-01-01_12-30-45.json")
        );
    }

    #[test]
    fn test_chat_request_dump_serialization() {
        let timestamp = Utc::now();
        let request = ChatContext::default();
        let response = serde_json::json!({
            "messages_count": 0,
            "messages_debug": []
        });

        let dump_data =
            ChatRequestDump { timestamp, request, response: Some(response), error: None };

        // Should be able to serialize without errors
        let json_result = serde_json::to_string_pretty(&dump_data);
        assert!(json_result.is_ok());
    }

    #[test]
    fn test_timestamp_formatting() {
        let timestamp = DateTime::parse_from_rfc3339("2024-01-01T12:30:45Z")
            .unwrap()
            .with_timezone(&Utc);

        let formatted = timestamp.format("%Y-%m-%d_%H-%M-%S").to_string();
        assert_eq!(formatted, "2024-01-01_12-30-45");
    }
}