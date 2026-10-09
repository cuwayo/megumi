//! The tools the model may call, and the registry that holds them.
//!
//! A tool is a named capability the model can ask for mid-turn: it advertises a
//! [`ToolSpec`] (a name, a description, and a JSON Schema of its arguments) and,
//! when called, returns a short text result the model reasons over. Two tools
//! live here: [`SearchMemory`], which reads a chat's facts through the turn's
//! privacy boundary, and [`WebSearch`], which reads the web. Both are ordinary
//! registry entries; a tool that needs to know *which* turn it is in — the chat,
//! the asker, the reader — takes that from the [`ToolContext`] the agent passes
//! to [`Tool::call`], because the registry itself is shared across every chat.
//!
//! The registry is deliberately thin: it maps a name to a tool and advertises
//! every spec.

use std::sync::Arc;

use serde::Deserialize;

use crate::config::AgentConfig;
use crate::context::{ReaderContext, render_memory, truncate};
use crate::event::{ChatId, InboundEvent};
use crate::llm::{BoxFuture, ToolSpec};
use crate::memory::{MemoryStore, retrieval};

/// The turn a tool is running in: which chat it was called in, and the privacy
/// boundary memory reads pass through.
///
/// The registry is shared across every chat and turn, so it cannot carry the
/// turn's identity; this is built fresh for each turn and handed to
/// [`Tool::call`]. A tool that acts on the conversation — sets a reminder in
/// this chat — reads the chat here. It wraps the turn's [`ReaderContext`], so a
/// tool that reads memory reads it through the same filter the prompt does, and
/// the private-to-group boundary holds on the tool path too.
#[derive(Clone, Debug)]
pub struct ToolContext {
    reader: ReaderContext,
}

impl ToolContext {
    /// A context over an already-built reader.
    pub fn new(reader: ReaderContext) -> Self {
        Self { reader }
    }

    /// The context of a turn triggered by `event`.
    pub fn for_event(event: &InboundEvent) -> Self {
        Self::new(ReaderContext::for_event(event))
    }

    /// The privacy boundary and identity of the turn.
    pub fn reader(&self) -> &ReaderContext {
        &self.reader
    }

    /// The chat the tool was called in.
    pub fn chat(&self) -> &ChatId {
        &self.reader.chat
    }
}

/// A capability the model may call by name.
pub trait Tool: Send + Sync {
    /// What the model is told about this tool.
    fn spec(&self) -> ToolSpec;
    /// Runs the tool with the turn's `context` and the model's arguments,
    /// returning text or an error.
    ///
    /// The context carries the chat and the reader, so a tool can act on the
    /// conversation it was called in. A failure is a string, not a panic: the
    /// agent hands it back to the model as the call's result so the turn can
    /// still answer.
    fn call(
        &self,
        context: &ToolContext,
        arguments: &serde_json::Value,
    ) -> BoxFuture<Result<String, String>>;

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

/// The tools a turn may call, by name.
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

/// The name of the memory-search tool.
///
/// The tool registers itself under this name; a test that asserts on the
/// advertised tool, or a caller that wants to recognise it, compares against
/// this rather than repeating the literal.
pub const SEARCH_MEMORY: &str = "search_memory";

/// The memory-search tool: the facts the turn's reader may see for a query.
///
/// It reads through [`retrieval::search`], so it filters by the turn's
/// [`ReaderContext`] before it ranks — the same boundary the prompt uses. A fact
/// the prompt would not show cannot be reached through the tool either. It is
/// read-only, so it needs no confirmation.
pub struct SearchMemory {
    store: Arc<MemoryStore>,
    config: AgentConfig,
}

impl SearchMemory {
    /// A tool over `store`, ranking with `config`.
    pub fn new(store: Arc<MemoryStore>, config: AgentConfig) -> Self {
        Self { store, config }
    }
}

impl Tool for SearchMemory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SEARCH_MEMORY.to_string(),
            description: "Search the facts this conversation has stored about people, places, \
                          plans, and preferences. Use it when the answer may depend on something \
                          said earlier that is no longer in the recent messages."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to look up in memory."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn call(
        &self,
        context: &ToolContext,
        arguments: &serde_json::Value,
    ) -> BoxFuture<Result<String, String>> {
        // The future is `'static`, so it owns the query and clones what it needs
        // rather than borrowing the arguments or the tool.
        let query = arguments
            .get("query")
            .and_then(|query| query.as_str())
            .map(str::to_string);
        let reader = context.reader().clone();
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        Box::pin(async move {
            let query =
                query.ok_or_else(|| "search_memory needs a `query` argument".to_string())?;
            let memories = retrieval::search(&reader, &query, &store, &config, chrono::Utc::now())?;
            if memories.is_empty() {
                return Ok("No stored facts matched.".to_string());
            }
            Ok(memories
                .iter()
                .map(render_memory)
                .collect::<Vec<_>>()
                .join("\n"))
        })
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

    fn call(
        &self,
        _context: &ToolContext,
        arguments: &serde_json::Value,
    ) -> BoxFuture<Result<String, String>> {
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
    fn the_memory_search_spec_advertises_a_required_query() {
        let tool = SearchMemory::new(
            Arc::new(MemoryStore::open(":memory:").unwrap()),
            AgentConfig::for_test(),
        );
        let spec = tool.spec();
        assert_eq!(spec.name, SEARCH_MEMORY);
        assert_eq!(spec.parameters["required"][0], "query");
        // Read-only, so it never needs a confirmation.
        assert!(tool.confirmation(&serde_json::json!({})).is_none());
    }

    #[tokio::test]
    async fn memory_search_reads_through_the_turn_context() {
        use crate::context::Visibility;
        use crate::event::{ChatId, ChatType, SenderId};
        use crate::memory::store::{MemoryOp, NewFact};

        let store = Arc::new(MemoryStore::open(":memory:").unwrap());
        store
            .apply(
                &ChatId::new("gA"),
                &[MemoryOp::Add(NewFact {
                    content: "The venue is the old hall.".into(),
                    visibility: Visibility::Chat,
                    subject: None,
                    confidence: 0.9,
                    importance: 0.5,
                    valid_from: chrono::Utc::now(),
                    evidence: vec!["m1".into()],
                })],
                None,
            )
            .unwrap();
        let tool = SearchMemory::new(Arc::clone(&store), AgentConfig::for_test());

        // The turn's context is a member of gA, so the group fact is visible.
        let in_group = ToolContext::new(ReaderContext {
            chat: ChatId::new("gA"),
            chat_type: ChatType::Group,
            requester: SenderId::new("u1"),
            member_of: vec![ChatId::new("gA")],
        });
        let result = tool
            .call(&in_group, &serde_json::json!({ "query": "venue" }))
            .await
            .unwrap();
        assert!(result.contains("old hall"), "{result}");

        // A different group cannot see it.
        let elsewhere = ToolContext::new(ReaderContext {
            chat: ChatId::new("gB"),
            chat_type: ChatType::Group,
            requester: SenderId::new("u2"),
            member_of: vec![ChatId::new("gB")],
        });
        let result = tool
            .call(&elsewhere, &serde_json::json!({ "query": "venue" }))
            .await
            .unwrap();
        assert_eq!(result, "No stored facts matched.");
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
