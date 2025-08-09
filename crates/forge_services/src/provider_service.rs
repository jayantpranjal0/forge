use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use forge_app::ProviderService;
use forge_app::domain::{
    ChatCompletionMessage, Context as ChatContext, HttpConfig, Model, ModelId, Provider,
    ResultStream, RetryConfig,
};
use tokio::sync::Mutex;

use crate::EnvironmentInfra;
use crate::http::HttpClient;
use crate::infra::HttpInfra;
use crate::provider::{Client, ClientBuilder};
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
            infra: infra,
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
}

#[async_trait::async_trait]
impl<I: EnvironmentInfra + HttpInfra> ProviderService for ForgeProviderService<I> {
    async fn chat(
        &self,
        model: &ModelId,
        request: ChatContext,
        provider: Provider,
    ) -> ResultStream<ChatCompletionMessage, anyhow::Error> {
        if let Some(dump_file) = self.infra.get_env_var("FORGE_CONTEXT_DUMP") {
            let dump_path = Path::new("experiment_chat_request_dump");
            if let Err(e) = std::fs::create_dir_all(dump_path) {
                eprintln!("Warning: Failed to create dump directory: {e}");
            } else {
                // Create the filename from the environment variable
                let file_path = dump_path.join(format!("{dump_file}.json"));

                // Serialize and write the context to JSON
                match serde_json::to_string_pretty(&request) {
                    Ok(json_content) => {
                        if let Err(e) = std::fs::write(&file_path, json_content) {
                            eprintln!(
                                "Warning: Failed to write context dump to {file_path:?}: {e}"
                            );
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

        let client = self.client(provider).await?;

        client
            .chat(model, request)
            .await
            .with_context(|| format!("Failed to chat with model: {model}"))
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
    fn test_dump_path_construction() {
        let base_path = std::path::Path::new("experiment_chat_request_dump");
        let filename = "test_env_var";
        let full_path = base_path.join(format!("{}.json", filename));

        assert_eq!(
            full_path,
            std::path::PathBuf::from("experiment_chat_request_dump/test_env_var.json")
        );
    }
}
