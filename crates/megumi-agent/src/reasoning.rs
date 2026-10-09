//! The planner and the evaluator: a plan before the turn, a critique after it.
//!
//! The agent answers in one reasoning pass by default. This module adds two
//! optional ones, both quality features rather than correctness or safety
//! features, and both **failing open** — a plan or a verdict that cannot be read
//! changes nothing:
//!
//! - [`plan`] makes one model call before the turn and returns a short list of
//!   steps, which the context builder renders as a `<plan>` layer. It runs only
//!   when planning is enabled and the request is long enough to warrant it.
//! - [`evaluate`] makes one model call after a draft reply exists and returns a
//!   [`Verdict`]. On `REVISE`, the caller runs one more turn seeded by
//!   [`revision_seed`] to produce an improved reply, bounded by its revision
//!   budget.
//!
//! Neither pass reads memory or the [`ReaderContext`](crate::context::ReaderContext),
//! so there is no privacy surface here: the planner sees the trigger and the
//! stored summary, and the evaluator sees the trigger and the draft. The output
//! guard still runs last on whatever reply wins, so a plan or a critique can
//! never loosen it. Model output (the draft, the critique) is escaped before it
//! enters a later prompt, the same as any conversation text.

use serde::Deserialize;

use crate::config::AgentConfig;
use crate::context::{escape, strip_code_fence};
use crate::event::{ChatType, InboundEvent};
use crate::llm::{LlmClient, LlmMessage, LlmRequest};

/// The most steps a plan may carry into the prompt.
///
/// The token cap bounds the planner's reply, but a model can still answer with
/// many tiny steps; this keeps the `<plan>` layer from crowding the context.
const MAX_PLAN_STEPS: usize = 8;

/// The token counts one reasoning call reported, folded into the turn's totals.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    /// Input tokens the model reported, when it reported any.
    pub input: Option<u32>,
    /// Output tokens the model reported, when it reported any.
    pub output: Option<u32>,
}

/// The evaluator's read on a draft reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the draft is ready to send. `false` means revise it.
    pub accept: bool,
    /// Why, in one short sentence. Empty when the model gave no reason.
    pub reason: String,
}

/// Plans how to answer `trigger`, or `None` when no plan was made.
///
/// A plan is made only when [`AgentConfig::planner_enabled`] is set and the
/// request has at least [`AgentConfig::plan_min_words`] words — a greeting does
/// not need one. Any failure (no model, a transport error, an unreadable reply)
/// yields `None`, so planning can only ever help: a turn without a plan is the
/// turn the agent already produced.
pub async fn plan(
    llm: &dyn LlmClient,
    config: &AgentConfig,
    chat_type: ChatType,
    summary: Option<&str>,
    trigger: &InboundEvent,
) -> (Option<Vec<String>>, Usage) {
    if !config.planner_enabled
        || trigger.trimmed_text().split_whitespace().count() < config.plan_min_words
    {
        return (None, Usage::default());
    }
    let (system, user) = plan_prompt(chat_type, summary, trigger);
    let request = LlmRequest {
        model: config.model.clone(),
        system,
        user,
        max_tokens: config.plan_max_tokens,
        tools: Vec::new(),
        messages: Vec::new(),
    };
    let response = match llm.complete(request).await {
        Ok(response) => response,
        // A disabled or failing model is not a turn failure: the plan is simply
        // absent and the turn proceeds without one.
        Err(_) => return (None, Usage::default()),
    };
    let usage = Usage {
        input: response.input_tokens,
        output: response.output_tokens,
    };
    (parse_steps(&response.text), usage)
}

/// Judges whether `candidate` answers `question` well.
///
/// Returns `None` when the verdict cannot be read — no model, a transport
/// error, or a reply that is not one of the two verdicts. The caller treats
/// `None` as acceptance, so the evaluator fails open: it can only ever improve a
/// reply, never suppress one.
pub async fn evaluate(
    llm: &dyn LlmClient,
    config: &AgentConfig,
    question: &str,
    candidate: &str,
) -> (Option<Verdict>, Usage) {
    let (system, user) = evaluate_prompt(question, candidate);
    let request = LlmRequest {
        model: config.model.clone(),
        system,
        user,
        max_tokens: config.evaluator_max_tokens,
        tools: Vec::new(),
        messages: Vec::new(),
    };
    let response = match llm.complete(request).await {
        Ok(response) => response,
        Err(_) => return (None, Usage::default()),
    };
    let usage = Usage {
        input: response.input_tokens,
        output: response.output_tokens,
    };
    (parse_verdict(&response.text), usage)
}

/// The transcript a revision turn runs on top of the ordinary prompt.
///
/// The revision reuses the turn's own prompt — the same system prompt and the
/// same assembled user turn — and appends the draft as an assistant turn and the
/// critique as the next user turn. That is a valid user/assistant alternation,
/// so the model sees its own draft and the reason it fell short, then produces an
/// improved reply. The critique is model-derived text, so it is escaped before
/// it enters the prompt, the same as any conversation content.
pub fn revision_seed(candidate: &str, reason: &str) -> Vec<LlmMessage> {
    let reason = reason.trim();
    let instruction = if reason.is_empty() {
        "Rewrite the reply above so it answers the message more accurately and completely. \
         Reply with the improved reply only."
            .to_string()
    } else {
        format!(
            "<critique>{}</critique>\n\nRewrite the reply above so it addresses this critique. \
             Reply with the improved reply only.",
            escape(reason)
        )
    };
    vec![
        LlmMessage::Text {
            assistant: true,
            text: candidate.to_string(),
        },
        LlmMessage::Text {
            assistant: false,
            text: instruction,
        },
    ]
}

/// The plan as the `<plan>` layer's body: one numbered step per line.
///
/// Not escaped here — the context builder escapes the whole layer on the way in,
/// so escaping twice would show `&amp;` to the model.
pub fn render_plan(steps: &[String]) -> String {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| format!("{}. {step}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The planner's prompt: the summary, if any, and the request to plan.
fn plan_prompt(
    chat_type: ChatType,
    summary: Option<&str>,
    trigger: &InboundEvent,
) -> (String, String) {
    let where_ = match chat_type {
        ChatType::Group => "a group chat",
        ChatType::Private => "a private one-on-one chat",
    };
    let system = format!(
        "You plan how to answer a message in {where_}. Think briefly about what a complete, \
         correct answer needs, then reply with a JSON array of short steps and nothing else, \
         for example [\"...\", \"...\"]. Order the steps the way you would carry them out and \
         keep the list short. A simple request needs a single step. Reply with only the JSON \
         array."
    );

    let mut user = String::new();
    if let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) {
        user.push_str("<conversation_summary>\n");
        user.push_str(&escape(summary));
        user.push_str("\n</conversation_summary>\n\n");
    }
    user.push_str("<message>");
    user.push_str(&escape(trigger.trimmed_text()));
    user.push_str("</message>");
    (system, user)
}

/// The evaluator's prompt: the question and the draft to judge.
fn evaluate_prompt(question: &str, candidate: &str) -> (String, String) {
    let system = "You review a draft reply before it is sent to a chat. Judge whether it \
                  answers the message accurately, completely, and in a fitting tone, using \
                  only what the conversation supports. Reply with a JSON object and nothing \
                  else: {\"verdict\":\"ACCEPT\",\"reason\":\"...\"} when the draft is ready to \
                  send, or {\"verdict\":\"REVISE\",\"reason\":\"...\"} with one short, specific \
                  reason when it is not. Judge the draft; do not rewrite it."
        .to_string();
    let user = format!(
        "<message>{}</message>\n<draft>{}</draft>",
        escape(question),
        escape(candidate)
    );
    (system, user)
}

/// The steps in a planner reply, however it was wrapped.
///
/// The prompt asks for a bare JSON array of strings, but a model may fence it or
/// add a sentence around it, so the fence is stripped and, failing a direct
/// parse, the widest bracketed slice is tried. Blank steps are dropped and the
/// list is capped; `None` means nothing usable, which the caller treats as "no
/// plan".
fn parse_steps(text: &str) -> Option<Vec<String>> {
    let body = strip_code_fence(text);
    let raw = parse_string_array(body).or_else(|| {
        let start = body.find('[')?;
        let end = body.rfind(']')?;
        parse_string_array(&body[start..=end])
    })?;
    let steps: Vec<String> = raw
        .into_iter()
        .map(|step| step.trim().to_string())
        .filter(|step| !step.is_empty())
        .take(MAX_PLAN_STEPS)
        .collect();
    (!steps.is_empty()).then_some(steps)
}

/// A JSON array of strings, or `None`.
fn parse_string_array(body: &str) -> Option<Vec<String>> {
    serde_json::from_str(body).ok()
}

/// The evaluator's reply as the model wrote it, before interpretation.
#[derive(Deserialize)]
struct RawVerdict {
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    reason: String,
}

/// The verdict in an evaluator reply, however it was wrapped.
///
/// Only the two known verdicts are accepted; anything else — an unknown word, a
/// missing field, unreadable JSON — is `None`, which the caller reads as
/// acceptance so the evaluator never blocks a reply it could not judge.
fn parse_verdict(text: &str) -> Option<Verdict> {
    let body = strip_code_fence(text);
    let raw: RawVerdict = serde_json::from_str(body).ok().or_else(|| {
        let start = body.find('{')?;
        let end = body.rfind('}')?;
        serde_json::from_str(&body[start..=end]).ok()
    })?;
    match raw.verdict.trim().to_ascii_uppercase().as_str() {
        "ACCEPT" => Some(Verdict {
            accept: true,
            reason: raw.reason,
        }),
        "REVISE" => Some(Verdict {
            accept: false,
            reason: raw.reason,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_and_a_fenced_plan_both_parse() {
        assert_eq!(
            parse_steps(r#"["look it up", "answer"]"#),
            Some(vec!["look it up".to_string(), "answer".to_string()])
        );
        assert_eq!(
            parse_steps("```json\n[\"one step\"]\n```"),
            Some(vec!["one step".to_string()])
        );
        // A sentence around the array is tolerated via the bracket slice.
        assert_eq!(
            parse_steps("Here is the plan: [\"do it\"]"),
            Some(vec!["do it".to_string()])
        );
    }

    #[test]
    fn a_garbage_plan_is_none() {
        assert!(parse_steps("I could not plan this.").is_none());
        assert!(parse_steps("[]").is_none(), "an empty plan is no plan");
        assert!(parse_steps(r#"["", "  "]"#).is_none());
    }

    #[test]
    fn the_plan_is_capped() {
        let many = serde_json::to_string(&vec!["step"; MAX_PLAN_STEPS + 5]).unwrap();
        assert_eq!(parse_steps(&many).unwrap().len(), MAX_PLAN_STEPS);
    }

    #[test]
    fn both_verdicts_parse_and_anything_else_is_none() {
        assert_eq!(
            parse_verdict(r#"{"verdict":"ACCEPT","reason":"fine"}"#),
            Some(Verdict {
                accept: true,
                reason: "fine".into()
            })
        );
        assert_eq!(
            parse_verdict("```json\n{\"verdict\":\"REVISE\",\"reason\":\"too vague\"}\n```"),
            Some(Verdict {
                accept: false,
                reason: "too vague".into()
            })
        );
        // An unknown verdict, or none at all, is unreadable — the caller accepts.
        assert!(parse_verdict(r#"{"verdict":"MAYBE"}"#).is_none());
        assert!(parse_verdict("looks good to me").is_none());
    }

    #[test]
    fn render_plan_numbers_the_steps() {
        let steps = vec!["first".to_string(), "second".to_string()];
        assert_eq!(render_plan(&steps), "1. first\n2. second");
    }
}
