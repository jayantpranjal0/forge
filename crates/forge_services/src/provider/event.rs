use anyhow::Context;
use forge_app::domain::ChatCompletionMessage;
use forge_app::dto::openai::Error;
use reqwest::Url;
use reqwest_eventsource::{Event, EventSource};
use serde::de::DeserializeOwned;
use tokio_stream::{Stream, StreamExt};
use tracing::debug;

use super::utils::format_http_context;

pub fn into_chat_completion_message<Response>(
    url: Url,
    source: EventSource,
) -> impl Stream<Item = anyhow::Result<ChatCompletionMessage>>
where
    Response: DeserializeOwned,
    ChatCompletionMessage: TryFrom<Response, Error = anyhow::Error>,
{
    source
            .take_while(|message| !matches!(message, Err(reqwest_eventsource::Error::StreamEnded)))
            .then(|event| async {
                // tracing::debug!(event = ?event, "Received event from OpenAI");
                match event {
                    Ok(event) => match event {
                        Event::Open => None,
                        Event::Message(event) if ["[DONE]", ""].contains(&event.data.as_str()) => {

                            debug!("Received completion from Upstream");
                            None
                        }
                        Event::Message(message) => Some(
                            serde_json::from_str::<Response>(&message.data)
                                .with_context(|| {
                                    format!(
                                        "Failed to parse provider response: {}",
                                        message.data
                                    )
                                })
                                .and_then(|response| {
                                    ChatCompletionMessage::try_from(response).with_context(
                                        || {
                                            format!(
                                                "Failed to create completion message: {}",
                                                message.data
                                            )
                                        },
                                    )
                                }),
                        ),
                    },
                    Err(error) => match error {
                        reqwest_eventsource::Error::StreamEnded => None,
                        reqwest_eventsource::Error::InvalidStatusCode(_, response) => {
                            let status = response.status();
                            let body = response.text().await.ok();
                            Some(Err(Error::InvalidStatusCode(status.as_u16())).with_context(
                                || match body {
                                    Some(body) => {
                                        format!("{status} Reason: {body}")
                                    }
                                    None => {
                                        format!("{status} Reason: [Unknown]")
                                    }
                                },
                            ))
                        }
                        reqwest_eventsource::Error::InvalidContentType(_, response) => {
                            let status_code = response.status();
                            debug!(response = ?response, "Invalid content type, attempting to parse as JSON response");

                            // Try to read the response body as JSON
                            match response.text().await {
                                Ok(body) => {
                                    match serde_json::from_str::<Response>(&body) {
                                        Ok(parsed_response) => {
                                            // Successfully parsed as JSON, convert to ChatCompletionMessage
                                            match ChatCompletionMessage::try_from(parsed_response) {
                                                Ok(message) => Some(Ok(message)),
                                                Err(conversion_error) => {
                                                    debug!(error = ?conversion_error, body = %body, "Failed to convert JSON response to ChatCompletionMessage");
                                                    Some(Err(conversion_error))
                                                }
                                            }
                                        }
                                        Err(parse_error) => {
                                            debug!(error = ?parse_error, body = %body, "Failed to parse response body as JSON");
                                            Some(Err(anyhow::anyhow!("Invalid content type with non-JSON body")).with_context(|| format!("Http Status: {status_code}, Invalid JSON: {parse_error}")))
                                        }
                                    }
                                }
                                Err(read_error) => {
                                    debug!(error = ?read_error, "Failed to read response body");
                                    Some(Err(anyhow::anyhow!("Invalid content type and failed to read body")).with_context(|| format!("Http Status: {status_code}, Failed to read body: {read_error}")))
                                }
                            }
                        }
                        error => {
                            tracing::error!(error = ?error, "Failed to receive chat completion event");
                            Some(Err(error.into()))
                        }
                    },
                }
            })
            .filter_map(move |response| {
                response
                    .map(|result| result.with_context(|| format_http_context(None, "POST", url.clone())))
            })
}
