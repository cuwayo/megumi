//! The tools the model may call, and the registry that holds them.
//!
//! A tool is a named capability the model can ask for mid-turn: it advertises a
//! [`ToolSpec`] (a name, a description, and a JSON Schema of its arguments) and,
//! when called, returns a short text result the model reasons over. Three tools
//! live here: [`SearchMemory`], which reads a chat's facts through the turn's
//! privacy boundary; [`SearchHistory`], which reads a chat's older messages
//! past the prompt window; and [`WebSearch`], which reads the web. All are
//! ordinary registry entries; a tool that needs to know *which* turn it is in —
//! the chat, the asker, the reader — takes that from the [`ToolContext`] the
//! agent passes to [`Tool::call`], because the registry itself is shared across
//! every chat.
//!
//! The registry is deliberately thin: it maps a name to a tool and advertises
//! every spec.

use std::sync::Arc;

use serde::Deserialize;

use crate::config::AgentConfig;
use crate::context::{ReaderContext, attachment_note, escape, render_memory, truncate};
use crate::event::{ChatId, InboundEvent};
use crate::llm::{BoxFuture, ToolSpec};
use crate::memory::{MemoryStore, retrieval};
use crate::store::{MessageStore, StoredMessage};

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

/// The name of the history-search tool.
pub const SEARCH_HISTORY: &str = "search_history";

/// The history-search tool: earlier messages of the turn's chat that match a
/// query.
///
/// The prompt only carries the most recent messages, so anything older is
/// durable in the store but invisible to the model. This tool reads the whole
/// stored window for the turn's chat and ranks messages the way
/// [`retrieval::similarity`] ranks facts — lexically, the v1 stand-in for an
/// embedding. It is scoped to `context.chat()`, so a group turn cannot reach a
/// private chat's or another group's messages. Read-only, so it needs no
/// confirmation.
pub struct SearchHistory {
    store: Arc<MessageStore>,
    config: AgentConfig,
}

impl SearchHistory {
    /// A tool over `store`, ranking with `config`.
    pub fn new(store: Arc<MessageStore>, config: AgentConfig) -> Self {
        Self { store, config }
    }
}

impl Tool for SearchHistory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SEARCH_HISTORY.to_string(),
            description: "Search earlier messages in this conversation. Use it when the answer \
                          depends on something said before the recent messages, which are the \
                          only ones you can already see."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to look for in the earlier messages."
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
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        let chat = context.chat().clone();
        Box::pin(async move {
            let query =
                query.ok_or_else(|| "search_history needs a `query` argument".to_string())?;
            // The whole stored window, not the prompt's smaller one: reaching
            // past the prompt window is the point of the tool.
            let window = store.recent(&chat, config.max_stored_messages)?;
            let mut scored: Vec<(f32, StoredMessage)> = window
                .into_iter()
                .map(|message| {
                    let haystack = format!(
                        "{}{}",
                        attachment_note(&message.attachments),
                        message.text.as_deref().unwrap_or(""),
                    );
                    (retrieval::similarity(&query, &haystack), message)
                })
                // A message that does not match the query at all is not worth
                // showing; an empty query matches nothing rather than everything.
                .filter(|(score, _)| *score > 0.0)
                .collect();
            // Best match first; ties newest-first, then id, so the same store
            // always ranks the same way.
            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.1.timestamp.cmp(&a.1.timestamp))
                    .then_with(|| a.1.message_id.cmp(&b.1.message_id))
            });
            let lines: Vec<String> = scored
                .into_iter()
                .take(config.history_search_results)
                .map(|(_, message)| render_history(&message))
                .collect();
            if lines.is_empty() {
                return Ok("No earlier messages matched.".to_string());
            }
            Ok(truncate(&lines.join("\n"), config.history_search_max_chars))
        })
    }
}

/// One matched message as a plain, escaped line for the tool channel.
///
/// Untagged, unlike the prompt's own `render_message`: a tool result carrying
/// the prompt's internal tags risks the model echoing them, which the output
/// guard would then drop. It carries the same fields — who, when, and the
/// described media — so the model can tell the messages apart.
fn render_history(message: &StoredMessage) -> String {
    let name = message
        .sender_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| message.sender.as_str());
    let text = format!(
        "{}{}",
        attachment_note(&message.attachments),
        escape(message.text.as_deref().unwrap_or("")),
    );
    // One message stays one line, so a newline in the text cannot break the
    // line-oriented result apart.
    format!(
        "- {} ({}): {}",
        escape(name),
        message.timestamp.to_rfc3339(),
        text.replace(['\n', '\r'], " "),
    )
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

    /// A message by `sender` with `text`, timestamped `minutes_ago` back.
    fn stored(id: &str, sender: &str, text: &str, minutes_ago: i64) -> StoredMessage {
        StoredMessage {
            message_id: id.into(),
            sender: crate::event::SenderId::new(sender),
            sender_name: Some(sender.into()),
            text: Some(text.into()),
            attachments: Vec::new(),
            from_self: false,
            timestamp: chrono::Utc::now() - chrono::Duration::minutes(minutes_ago),
        }
    }

    /// A `ToolContext` for a turn in `chat`.
    fn context(chat: &str) -> ToolContext {
        use crate::event::{ChatId, ChatType, SenderId};
        ToolContext::new(ReaderContext {
            chat: ChatId::new(chat),
            chat_type: ChatType::Group,
            requester: SenderId::new("u1"),
            member_of: vec![ChatId::new(chat)],
        })
    }

    #[test]
    fn the_history_search_spec_advertises_a_required_query() {
        let tool = SearchHistory::new(
            Arc::new(MessageStore::open(":memory:", 200).unwrap()),
            AgentConfig::for_test(),
        );
        let spec = tool.spec();
        assert_eq!(spec.name, SEARCH_HISTORY);
        assert_eq!(spec.parameters["required"][0], "query");
        // Read-only, so it never needs a confirmation.
        assert!(tool.confirmation(&serde_json::json!({})).is_none());
    }

    #[tokio::test]
    async fn history_search_finds_a_matching_message() {
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let chat = crate::event::ChatId::new("gA");
        store
            .append(&chat, stored("m1", "Budi", "the venue is the old hall", 10))
            .unwrap();
        store
            .append(&chat, stored("m2", "Budi", "unrelated chatter", 5))
            .unwrap();
        let tool = SearchHistory::new(Arc::clone(&store), AgentConfig::for_test());

        let result = tool
            .call(&context("gA"), &serde_json::json!({ "query": "venue" }))
            .await
            .unwrap();
        assert!(result.contains("old hall"), "{result}");
        // The non-matching message is not shown.
        assert!(!result.contains("unrelated"), "{result}");
    }

    #[tokio::test]
    async fn history_search_with_no_match_says_so() {
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let chat = crate::event::ChatId::new("gA");
        store
            .append(&chat, stored("m1", "Budi", "the venue is the old hall", 10))
            .unwrap();
        let tool = SearchHistory::new(Arc::clone(&store), AgentConfig::for_test());

        // An unrelated query, and a query with no terms, both match nothing.
        for query in ["weather", "!!!"] {
            let result = tool
                .call(&context("gA"), &serde_json::json!({ "query": query }))
                .await
                .unwrap();
            assert_eq!(result, "No earlier messages matched.", "{query}");
        }
    }

    #[tokio::test]
    async fn history_search_needs_a_query() {
        let tool = SearchHistory::new(
            Arc::new(MessageStore::open(":memory:", 200).unwrap()),
            AgentConfig::for_test(),
        );
        let error = tool
            .call(&context("gA"), &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(error.contains("query"), "{error}");
    }

    #[tokio::test]
    async fn history_search_is_capped_and_ordered() {
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let chat = crate::event::ChatId::new("gA");
        // Four messages match "venue"; the newest one is the best match, and the
        // cap is one below the match count.
        store
            .append(&chat, stored("m1", "Budi", "the venue is here", 40))
            .unwrap();
        store
            .append(&chat, stored("m2", "Budi", "the venue is there", 30))
            .unwrap();
        store
            .append(&chat, stored("m3", "Budi", "the venue is elsewhere", 20))
            .unwrap();
        store
            .append(&chat, stored("m4", "Budi", "the venue is the hall", 10))
            .unwrap();
        let mut config = AgentConfig::for_test();
        config.history_search_results = 2;
        let tool = SearchHistory::new(Arc::clone(&store), config);

        let result = tool
            .call(&context("gA"), &serde_json::json!({ "query": "venue" }))
            .await
            .unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2, "{result}");
        // All four match "venue" equally; the newest two are kept.
        assert!(lines[0].contains("hall"), "{result}");
        assert!(lines[1].contains("elsewhere"), "{result}");
    }

    #[tokio::test]
    async fn history_search_matches_a_described_attachment() {
        use crate::event::Attachment;
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let chat = crate::event::ChatId::new("gA");
        let mut message = stored("m1", "Budi", "", 10);
        message.text = None;
        message.attachments = vec![Attachment {
            kind: "image".into(),
            description: Some("a red bicycle".into()),
        }];
        store.append(&chat, message).unwrap();
        let tool = SearchHistory::new(Arc::clone(&store), AgentConfig::for_test());

        let result = tool
            .call(&context("gA"), &serde_json::json!({ "query": "bicycle" }))
            .await
            .unwrap();
        assert!(result.contains("red bicycle"), "{result}");
    }

    #[tokio::test]
    async fn history_search_cannot_read_another_chat() {
        // The store is keyed by chat, so a group turn must not reach a private
        // chat's or another group's messages.
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        store
            .append(
                &crate::event::ChatId::new("dm"),
                stored("m1", "Budi", "CANARY-private-secret", 10),
            )
            .unwrap();
        let tool = SearchHistory::new(Arc::clone(&store), AgentConfig::for_test());

        let result = tool
            .call(&context("gA"), &serde_json::json!({ "query": "secret" }))
            .await
            .unwrap();
        assert!(!result.contains("CANARY"), "{result}");
        assert_eq!(result, "No earlier messages matched.");
    }

    #[tokio::test]
    async fn history_search_output_is_truncated() {
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let chat = crate::event::ChatId::new("gA");
        for i in 0..5 {
            store
                .append(
                    &chat,
                    stored(&format!("m{i}"), "Budi", "the venue is the old hall", 10),
                )
                .unwrap();
        }
        let mut config = AgentConfig::for_test();
        config.history_search_max_chars = 30;
        let tool = SearchHistory::new(Arc::clone(&store), config);

        let result = tool
            .call(&context("gA"), &serde_json::json!({ "query": "venue" }))
            .await
            .unwrap();
        assert_eq!(result.chars().count(), 30, "{result}");
    }
}
