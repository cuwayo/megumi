//! The Anthropic Messages API, over raw HTTP.
//!
//! Rust has no official Anthropic SDK, so this speaks the Messages API directly
//! with the same `reqwest` client the rest of the bot uses. The request is the
//! Anthropic shape — a `system` prompt and a `messages` array — which both the
//! real API and a gateway that fronts it accept.
//!
//! The base URL is configurable, so the endpoint may be the real host or a
//! self-hosted gateway, and the two differ in small ways this module absorbs:
//!
//! - **Path**: a base may or may not end in `/v1`, so only `/messages` is
//!   appended when it already does.
//! - **Auth**: the real API reads `x-api-key`; a gateway typically reads
//!   `Authorization: Bearer`. Both headers carry the key, so either works.
//! - **Response**: the real API answers with `content` blocks, but a gateway may
//!   answer in the OpenAI shape (`choices`). Both are parsed.
//!
//! A 429 or a 5xx is retried once after a short pause; anything else is surfaced
//! as an [`LlmError`].

use std::time::Duration;

use serde::Deserialize;
use tracing::warn;

use super::{BoxFuture, LlmClient, LlmError, LlmMessage, LlmRequest, LlmResponse, LlmToolCall};

/// The `anthropic-version` header the Messages API requires.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How many times a retryable failure is retried.
const MAX_ATTEMPTS: u32 = 2;

/// An Anthropic Messages API client.
pub struct AnthropicLlm {
    api_key: String,
    api_base: String,
    http: reqwest::Client,
}

impl AnthropicLlm {
    /// Builds a client for `api_base` from the environment's credential.
    ///
    /// Reads `ANTHROPIC_AUTH_TOKEN` first (the token Claude Code and most
    /// gateways use, sent as `Authorization: Bearer`) and falls back to
    /// `ANTHROPIC_API_KEY`. Returns `None` when neither is set, so the caller
    /// can fall back to [`DisabledLlm`](super::DisabledLlm) rather than failing
    /// to start. The base URL and the model are not read here — the caller
    /// passes the base and every [`LlmRequest`] carries its own model.
    pub fn from_env(api_base: impl Into<String>) -> Option<Self> {
        let api_key = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
            .iter()
            .find_map(|key| std::env::var(key).ok())
            .filter(|key| !key.trim().is_empty())?;
        Some(Self::new(api_key, api_base))
    }

    /// Builds a client against an explicit key and base URL.
    pub fn new(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent("megumi-agent")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            api_key: api_key.into(),
            api_base: api_base.into(),
            http,
        }
    }
}

/// A response that may be the Anthropic shape, the OpenAI shape, or an error.
#[derive(Deserialize)]
struct ApiResponse {
    /// Anthropic: the reply as content blocks.
    #[serde(default)]
    content: Vec<ContentBlock>,
    /// OpenAI: the reply as choices.
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
    /// Some gateways answer HTTP 200 with an error envelope.
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    /// A `tool_use` block's call id, echoed back with the result.
    #[serde(default)]
    id: String,
    /// A `tool_use` block's tool name.
    #[serde(default)]
    name: String,
    /// A `tool_use` block's arguments, as a JSON object.
    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<ChoiceMessage>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiToolCall>,
}

#[derive(Deserialize)]
struct OpenAiToolCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: Option<OpenAiFunction>,
}

#[derive(Deserialize)]
struct OpenAiFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct Usage {
    /// Anthropic's names.
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    /// OpenAI's names.
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
    #[serde(rename = "type", default)]
    kind: String,
}

impl ApiResponse {
    /// The reply text, from whichever shape the provider used.
    fn reply_text(&self) -> String {
        let anthropic: String = self
            .content
            .iter()
            .filter(|block| block.kind == "text" || block.kind.is_empty())
            .map(|block| block.text.as_str())
            .collect();
        if !anthropic.is_empty() {
            return anthropic;
        }
        self.choices
            .first()
            .and_then(|choice| choice.message.as_ref())
            .and_then(|message| message.content.clone())
            .unwrap_or_default()
    }

    /// The input and output token counts, under either naming.
    fn tokens(&self) -> (Option<u32>, Option<u32>) {
        match &self.usage {
            Some(usage) => (
                usage.input_tokens.or(usage.prompt_tokens),
                usage.output_tokens.or(usage.completion_tokens),
            ),
            None => (None, None),
        }
    }

    /// The tool calls the model asked for, from whichever shape it used.
    ///
    /// Anthropic carries them as `tool_use` content blocks; OpenAI as a
    /// `tool_calls` array on the choice's message. The arguments are rendered
    /// back to a JSON string, which is how the agent stores and replays them.
    fn tool_calls(&self) -> Vec<LlmToolCall> {
        let anthropic: Vec<LlmToolCall> = self
            .content
            .iter()
            .filter(|block| block.kind == "tool_use")
            .map(|block| LlmToolCall {
                id: block.id.clone(),
                name: block.name.clone(),
                arguments: block
                    .input
                    .as_ref()
                    .map(serde_json::Value::to_string)
                    .unwrap_or_default(),
            })
            .collect();
        if !anthropic.is_empty() {
            return anthropic;
        }
        self.choices
            .first()
            .and_then(|choice| choice.message.as_ref())
            .map(|message| {
                message
                    .tool_calls
                    .iter()
                    .map(|call| LlmToolCall {
                        id: call.id.clone(),
                        name: call
                            .function
                            .as_ref()
                            .map(|function| function.name.clone())
                            .unwrap_or_default(),
                        arguments: call
                            .function
                            .as_ref()
                            .map(|function| function.arguments.clone())
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl LlmClient for AnthropicLlm {
    fn complete(&self, request: LlmRequest) -> BoxFuture<Result<LlmResponse, LlmError>> {
        // The trait's future is `'static`, so the request owns everything it
        // needs rather than borrowing `self`.
        let api_key = self.api_key.clone();
        let api_base = self.api_base.clone();
        let http = self.http.clone();
        Box::pin(async move { send(&http, &api_key, &api_base, &request).await })
    }
}

async fn send(
    http: &reqwest::Client,
    api_key: &str,
    api_base: &str,
    request: &LlmRequest,
) -> Result<LlmResponse, LlmError> {
    let url = messages_url(api_base);
    let mut body = serde_json::json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "system": request.system,
        "messages": messages_body(request),
    });
    // Sent only when there are tools, so the extraction pass and a plain turn
    // emit exactly the body they emitted before the tool loop existed.
    if !request.tools.is_empty() {
        body["tools"] = serde_json::Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect(),
        );
    }

    let mut attempt = 0;
    loop {
        attempt += 1;
        let response = http
            .post(&url)
            .header("x-api-key", api_key)
            .header("authorization", format!("Bearer {api_key}"))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|error| LlmError::Transport(error.to_string()))?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();

        if status.is_success() {
            let parsed: ApiResponse = serde_json::from_str(&body_text).map_err(|error| {
                LlmError::Decode(format!(
                    "{error}: {}",
                    body_text.chars().take(200).collect::<String>()
                ))
            })?;
            if let Some(error) = parsed.error {
                return Err(LlmError::Provider(format!(
                    "{}: {}",
                    error.kind, error.message
                )));
            }
            let (input_tokens, output_tokens) = parsed.tokens();
            return Ok(LlmResponse {
                text: strip_safety_artifacts(&parsed.reply_text()),
                input_tokens,
                output_tokens,
                tool_calls: parsed.tool_calls(),
            });
        }

        let retryable = status.as_u16() == 429 || status.is_server_error();
        if retryable && attempt < MAX_ATTEMPTS {
            warn!(
                status = status.as_u16(),
                "the model request failed; retrying"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        return Err(LlmError::Status {
            status: status.as_u16(),
            body: body_text.chars().take(500).collect(),
        });
    }
}

/// The request's `messages` array, from whichever shape the request carries.
///
/// An empty transcript is the single-call shape every pre-tool caller uses: one
/// user turn, the request's `user`. A tool loop fills the transcript instead,
/// and each entry is rendered as the block shape the Messages API expects.
fn messages_body(request: &LlmRequest) -> Vec<serde_json::Value> {
    if request.messages.is_empty() {
        return vec![serde_json::json!({ "role": "user", "content": request.user })];
    }
    request
        .messages
        .iter()
        .map(|message| match message {
            LlmMessage::Text { assistant, text } => serde_json::json!({
                "role": if *assistant { "assistant" } else { "user" },
                "content": text,
            }),
            LlmMessage::ToolCalls(calls) => {
                let content: Vec<serde_json::Value> = calls
                    .iter()
                    .map(|call| {
                        serde_json::json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            // The model usually emits an object, but a malformed
                            // or empty string becomes `{}` rather than failing
                            // the request, so the tool reports its own error.
                            "input": serde_json::from_str::<serde_json::Value>(&call.arguments)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        })
                    })
                    .collect();
                serde_json::json!({ "role": "assistant", "content": content })
            }
            LlmMessage::ToolResults(results) => {
                let content: Vec<serde_json::Value> = results
                    .iter()
                    .map(|result| {
                        serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": result.id,
                            "content": result.content,
                        })
                    })
                    .collect();
                serde_json::json!({ "role": "user", "content": content })
            }
        })
        .collect()
}

/// The Messages endpoint for `api_base`.
///
/// The base is whatever the caller configured, so it may or may not already end
/// in `/v1`: the real Anthropic host is `https://api.anthropic.com` and needs
/// `/v1` appended, while a gateway is often given as `https://host/v1`.
/// Appending `/v1/messages` unconditionally would double the segment for the
/// second kind, so a trailing `/v1` is recognised and only `/messages` is added.
fn messages_url(api_base: &str) -> String {
    let base = api_base.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// Removes a gateway's safety-classifier annotation from a reply.
///
/// Some gateways append their moderation verdict inside a `<ds_safety>…</ds_safety>`
/// tag in the message text. It is the provider talking to itself, not part of the
/// answer, so it is stripped before the reply reaches a chat.
fn strip_safety_artifacts(text: &str) -> String {
    const OPEN: &str = "<ds_safety>";
    const CLOSE: &str = "</ds_safety>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::super::LlmToolResult;
    use super::*;

    #[test]
    fn a_bare_host_gets_the_version_path() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn a_base_that_already_ends_in_v1_is_not_doubled() {
        assert_eq!(
            messages_url("https://rb2ledk.abc-tunnel.us/v1"),
            "https://rb2ledk.abc-tunnel.us/v1/messages"
        );
        assert_eq!(
            messages_url("https://rb2ledk.abc-tunnel.us/v1/"),
            "https://rb2ledk.abc-tunnel.us/v1/messages"
        );
    }

    #[test]
    fn an_anthropic_response_is_parsed() {
        let body = r#"{"content":[{"type":"text","text":"hello"}],
                       "usage":{"input_tokens":5,"output_tokens":2}}"#;
        let parsed: ApiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.reply_text(), "hello");
        assert_eq!(parsed.tokens(), (Some(5), Some(2)));
    }

    #[test]
    fn an_openai_shaped_response_is_parsed() {
        // A gateway may answer the Anthropic route in the OpenAI shape.
        let body = r#"{"object":"chat.completion",
                       "choices":[{"message":{"role":"assistant","content":"hi there"}}],
                       "usage":{"prompt_tokens":7,"completion_tokens":3}}"#;
        let parsed: ApiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.reply_text(), "hi there");
        assert_eq!(parsed.tokens(), (Some(7), Some(3)));
    }

    #[test]
    fn an_error_envelope_is_read() {
        let body = r#"{"error":{"message":"no credentials","type":"invalid_request_error"}}"#;
        let parsed: ApiResponse = serde_json::from_str(body).unwrap();
        let error = parsed.error.unwrap();
        assert_eq!(error.message, "no credentials");
    }

    #[test]
    fn an_anthropic_tool_use_block_is_parsed() {
        let body = r#"{"content":[
            {"type":"text","text":"Let me look that up."},
            {"type":"tool_use","id":"call_1","name":"web_search","input":{"query":"weather"}}
        ],"stop_reason":"tool_use"}"#;
        let parsed: ApiResponse = serde_json::from_str(body).unwrap();
        let calls = parsed.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].arguments, r#"{"query":"weather"}"#);
        assert_eq!(parsed.reply_text(), "Let me look that up.");
    }

    #[test]
    fn an_openai_tool_calls_reply_is_parsed() {
        let body = r#"{"choices":[{"finish_reason":"tool_calls","message":{
            "role":"assistant","content":null,
            "tool_calls":[{"id":"call_2","type":"function",
                "function":{"name":"web_search","arguments":"{\"query\":\"news\"}"}}]
        }}]}"#;
        let parsed: ApiResponse = serde_json::from_str(body).unwrap();
        let calls = parsed.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].arguments, r#"{"query":"news"}"#);
    }

    #[test]
    fn a_plain_reply_has_no_tool_calls() {
        let body = r#"{"content":[{"type":"text","text":"hello"}]}"#;
        let parsed: ApiResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.tool_calls().is_empty());
    }

    #[test]
    fn a_transcript_is_rendered_as_the_messages_array() {
        let request = LlmRequest {
            model: "m".into(),
            system: "s".into(),
            user: "first".into(),
            max_tokens: 10,
            tools: Vec::new(),
            messages: vec![
                LlmMessage::Text {
                    assistant: false,
                    text: "first".into(),
                },
                LlmMessage::ToolCalls(vec![LlmToolCall {
                    id: "c1".into(),
                    name: "web_search".into(),
                    arguments: r#"{"query":"x"}"#.into(),
                }]),
                LlmMessage::ToolResults(vec![LlmToolResult {
                    id: "c1".into(),
                    content: "a result".into(),
                }]),
            ],
        };
        let body = messages_body(&request);
        assert_eq!(body.len(), 3);
        assert_eq!(body[0]["role"], "user");
        assert_eq!(body[1]["content"][0]["type"], "tool_use");
        assert_eq!(body[1]["content"][0]["input"]["query"], "x");
        assert_eq!(body[2]["content"][0]["type"], "tool_result");
        assert_eq!(body[2]["content"][0]["tool_use_id"], "c1");
    }

    #[test]
    fn an_empty_transcript_is_the_single_user_turn() {
        let request = LlmRequest::from_prompt(
            &crate::context::Prompt {
                system: "s".into(),
                user: "u".into(),
            },
            "m",
            10,
        );
        let body = messages_body(&request);
        assert_eq!(body.len(), 1);
        assert_eq!(body[0]["role"], "user");
        assert_eq!(body[0]["content"], "u");
    }

    #[test]
    fn a_gateway_safety_annotation_is_stripped() {
        let text = "Pong!<ds_safety>[判定]safe</ds_safety>Safe";
        assert_eq!(strip_safety_artifacts(text), "Pong!Safe");
        assert_eq!(strip_safety_artifacts("just a reply"), "just a reply");
        assert_eq!(strip_safety_artifacts("cut off <ds_safety>oops"), "cut off");
    }
}
