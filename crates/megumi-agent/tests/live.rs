//! A live smoke test against whatever model endpoint is configured.
//!
//! Ignored by default so CI needs no credential or network. Run it by hand to
//! confirm the endpoint, auth, and model work end to end:
//!
//! ```text
//! cargo test -p megumi-agent --test live -- --ignored --nocapture
//! ```
//!
//! It reads `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`,
//! and `ANTHROPIC_MODEL`, so it exercises the same path the bot uses. The tool
//! smoke test additionally reads `TAVILY_API_KEY` and `TAVILY_API_BASE`.

use megumi_agent::{AgentConfig, AnthropicLlm, LlmClient, LlmRequest, Tool, WebSearch};

#[tokio::test]
#[ignore = "needs a live model endpoint and credential"]
async fn the_configured_endpoint_answers() {
    let config = AgentConfig::from_env();
    let llm = AnthropicLlm::from_env(&config.api_base)
        .expect("ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY must be set");

    let response = llm
        .complete(LlmRequest {
            model: config.model.clone(),
            system: "Reply with exactly one word: pong".to_string(),
            user: "ping".to_string(),
            max_tokens: 64,
            tools: Vec::new(),
            messages: Vec::new(),
        })
        .await
        .expect("the model call should succeed");

    println!(
        "model={} base={} reply={:?} tokens={:?}/{:?}",
        config.model, config.api_base, response.text, response.input_tokens, response.output_tokens
    );
    assert!(
        !response.text.trim().is_empty(),
        "the reply should not be empty"
    );
}

#[tokio::test]
#[ignore = "needs a live web-search endpoint and credential"]
async fn the_configured_web_search_answers() {
    let config = AgentConfig::from_env();
    let tool = WebSearch::from_env(&config).expect("TAVILY_API_KEY must be set");

    let result = tool
        .call(&serde_json::json!({ "query": "who is the current secretary-general of the UN?" }))
        .await
        .expect("the web search should succeed");

    println!("web_search result:\n{result}");
    assert!(
        !result.trim().is_empty(),
        "the search should return something"
    );
}
