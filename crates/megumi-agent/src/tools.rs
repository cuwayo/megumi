//! The tools the model may call, and the registry that holds them.
//!
//! A tool is a named capability the model can ask for mid-turn: it advertises a
//! [`ToolSpec`] (a name, a description, and a JSON Schema of its arguments) and,
//! when called, returns a short text result the model reasons over. Two tools
//! exist so far: `search_memory`, which is built per turn from the reader's
//! [`ReaderContext`](crate::context::ReaderContext) so the privacy boundary
//! holds, and `web_search`, which is reader-independent and lives in the
//! registry.
//!
//! The registry is deliberately thin. It maps a name to a tool and advertises
//! every spec; the per-turn `search_memory` tool is not in it because it needs
//! the turn's reader, which the registry does not have.

use std::sync::Arc;

use serde::Deserialize;

use crate::config::AgentConfig;
use crate::context::truncate;
use crate::llm::{BoxFuture, ToolSpec};

/// A capability the model may call by name.
pub trait Tool: Send + Sync {
    /// What the model is told about this tool.
    fn spec(&self) -> ToolSpec;
    /// Runs the tool with the model's arguments, returning text or an error.
    ///
    /// A failure is a string, not a panic: the agent hands it back to the model
    /// as the call's result so the turn can still answer.
    fn call(&self, arguments: &serde_json::Value) -> BoxFuture<Result<String, String>>;

    /// The question to ask the chat before running this tool, or `None` when it
    /// is safe to run on the model's word alone.
    ///
    /// The default is `None` — a read-only tool needs no confirmation. A tool
    /// that changes state (sends something, writes somewhere, spends something)
    /// overrides this to return the question; the agent then holds the call and
    /// runs it only after the user agrees. The decision is the tool's, not the
    /// model's, so the model cannot talk its way past it.
    fn confirmation(&self, _arguments: &serde_json::Value) -> Option<String> {
        None
    }
}

/// The reader-independent tools a turn may call.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// A registry over `tools`.
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }

    /// The specs of every registered tool.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    /// The tool named `name`, when one is registered.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .cloned()
    }
}

/// The Tavily web-search tool.
///
/// One POST to `{base}/search` with a bearer token, flattened to a compact text
/// block. The key is read from `TAVILY_API_KEY`; without it the tool does not
/// exist, so an unconfigured bot simply has no web search.
pub struct WebSearch {
    api_key: String,
    api_base: String,
    http: reqwest::Client,
    max_results: usize,
    max_chars: usize,
}

impl WebSearch {
    /// Builds the tool from the environment and `config`, or `None` without a key.
    pub fn from_env(config: &AgentConfig) -> Option<Self> {
        let api_key = std::env::var("TAVILY_API_KEY").ok()?;
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return None;
        }
        Some(Self::new(
            api_key,
            &config.web_search_base,
            config.web_search_results,
            config.web_search_max_chars,
        ))
    }

    /// Builds the tool against an explicit key and settings.
    pub fn new(
        api_key: impl Into<String>,
        api_base: impl Into<String>,
        max_results: usize,
        max_chars: usize,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            api_base: api_base.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("megumi-agent")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            max_results,
            max_chars,
        }
    }
}

impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description: "Search the web for current information. Use this for facts you do \
                          not know or that may have changed — news, weather, prices, people, \
                          events. Returns ranked results with a title, a URL, and a snippet."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn call(&self, arguments: &serde_json::Value) -> BoxFuture<Result<String, String>> {
        // The future is `'static`, so it owns everything rather than borrowing.
        let api_key = self.api_key.clone();
        let api_base = self.api_base.clone();
        let http = self.http.clone();
        let max_results = self.max_results;
        let max_chars = self.max_chars;
        let query = arguments
            .get("query")
            .and_then(|query| query.as_str())
            .map(str::to_string);
        Box::pin(async move {
            let query = query.ok_or_else(|| "web_search needs a `query` argument".to_string())?;
            search(&http, &api_key, &api_base, &query, max_results, max_chars).await
        })
    }
}

/// A Tavily search response, reduced to what the model needs.
#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

/// Runs one web search and flattens the results.
async fn search(
    http: &reqwest::Client,
    api_key: &str,
    api_base: &str,
    query: &str,
    max_results: usize,
    max_chars: usize,
) -> Result<String, String> {
    let response = http
        .post(search_url(api_base))
        .header("authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({ "query": query, "max_results": max_results }))
        .send()
        .await
        .map_err(|error| format!("the web search request failed: {error}"))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "the web search returned HTTP {}: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        ));
    }
    let parsed: TavilyResponse = serde_json::from_str(&body)
        .map_err(|error| format!("the web search response could not be read: {error}"))?;
    Ok(format_results(&parsed, max_chars))
}

/// The search endpoint for `api_base`, which may or may not already end in
/// `/search`.
fn search_url(api_base: &str) -> String {
    let base = api_base.trim_end_matches('/');
    if base.ends_with("/search") {
        base.to_string()
    } else {
        format!("{base}/search")
    }
}

/// The results as a numbered text block, capped at `max_chars`.
fn format_results(response: &TavilyResponse, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, result) in response.results.iter().enumerate() {
        if result.url.is_empty() && result.content.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{}. {}\n{}\n{}\n\n",
            index + 1,
            result.title,
            result.url,
            result.content
        ));
    }
    if out.trim().is_empty() {
        return "No results found.".to_string();
    }
    truncate(&out, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_web_search_spec_advertises_a_required_query() {
        let tool = WebSearch::new("key", "https://api.tavily.com", 5, 4_000);
        let spec = tool.spec();
        assert_eq!(spec.name, "web_search");
        assert_eq!(spec.parameters["required"][0], "query");
    }

    #[test]
    fn a_base_without_the_search_segment_gets_it() {
        assert_eq!(
            search_url("https://api.tavily.com"),
            "https://api.tavily.com/search"
        );
        assert_eq!(
            search_url("https://api.tavily.com/search"),
            "https://api.tavily.com/search"
        );
        assert_eq!(
            search_url("https://proxy.example/v1/"),
            "https://proxy.example/v1/search"
        );
    }

    #[test]
    fn results_are_flattened_and_capped() {
        let response: TavilyResponse = serde_json::from_str(
            r#"{"results":[
                {"title":"A","url":"https://a.example","content":"first"},
                {"title":"B","url":"https://b.example","content":"second"}
            ]}"#,
        )
        .unwrap();
        let text = format_results(&response, 4_000);
        assert!(text.contains("1. A"), "{text}");
        assert!(text.contains("https://a.example"), "{text}");
        assert!(text.contains("second"), "{text}");

        let short = format_results(&response, 5);
        assert_eq!(short.chars().count(), 5);
    }

    #[test]
    fn an_empty_result_set_says_so() {
        let response: TavilyResponse = serde_json::from_str(r#"{"results":[]}"#).unwrap();
        assert_eq!(format_results(&response, 4_000), "No results found.");
    }

    #[test]
    fn a_read_only_tool_needs_no_confirmation() {
        // The default is `None`, so an existing tool like `web_search` is
        // unaffected by the confirmation gate.
        let tool = WebSearch::new("key", "https://api.tavily.com", 5, 4_000);
        assert!(tool.confirmation(&serde_json::json!({})).is_none());
    }

    #[test]
    fn the_registry_advertises_and_resolves_tools() {
        let registry = ToolRegistry::new(vec![Arc::new(WebSearch::new(
            "key",
            "https://api.tavily.com",
            5,
            4_000,
        ))]);
        assert_eq!(registry.specs().len(), 1);
        assert!(registry.get("web_search").is_some());
        assert!(registry.get("nope").is_none());
    }
}
