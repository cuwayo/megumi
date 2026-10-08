//! Turning a chat's messages into durable facts, with the model.
//!
//! The message store keeps a bounded window; a fact stated hundreds of messages
//! ago is gone from it. The writer is what makes such a fact durable: it hands
//! the model the messages a chat has said since the last pass, plus the facts
//! already stored for it, and asks for a batch of [`MemoryOp`]s. The model only
//! *proposes*; every field that carries privilege or time is set here, in code:
//! the visibility label comes from the chat's type, the validity start from the
//! evidence messages, and an op whose evidence is not in the batch is dropped.
//! A model cannot widen a fact's reach or invent a source.
//!
//! A pass runs when enough new messages have accumulated, or when a chat has
//! gone quiet with a backlog. It is one model call and one store write, and it
//! runs inside the per-chat turn lock so it never races the turn that reads the
//! same chat's memory.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::AgentConfig;
use crate::context::{Visibility, escape, truncate};
use crate::event::{ChatId, ChatType, SenderId};
use crate::llm::{LlmClient, LlmError, LlmRequest};
use crate::memory::store::{MemoryOp, MemoryRecord, MemoryStore, NewFact};
use crate::store::{MessageStore, StoredMessage};

/// The longest a stored fact may be, in characters. A model that rambles is
/// truncated rather than dropped, so a useful fact is not lost to length.
const MAX_CONTENT_CHARS: usize = 500;

/// Runs an extraction pass for `chat` when one is due.
///
/// Returns how many ops changed the store, or `0` when no pass was due or no
/// model is configured. A transport failure is returned as an error and leaves
/// the cursor where it was, so the pass is retried; a model that answered but
/// produced unparseable output advances the cursor and applies nothing, so a
/// bad reply cannot wedge the chat into retrying it forever.
pub async fn extract_if_due(
    llm: &dyn LlmClient,
    messages: &MessageStore,
    memory: &MemoryStore,
    config: &AgentConfig,
    chat: &ChatId,
    chat_type: ChatType,
) -> Result<usize, String> {
    let pending = pending_messages(messages, memory, chat, config)?;
    if !is_due(&pending, config, Utc::now()) {
        return Ok(0);
    }

    // Only this chat's still-true facts are shown, and only they may be
    // targeted: a correction must not reach across chats. The list is capped so
    // a chat with a long memory does not crowd the batch out of the prompt.
    let mut existing: Vec<MemoryRecord> = memory
        .records()?
        .into_iter()
        .filter(|record| record.origin_chat == *chat && record.valid_to.is_none())
        .collect();
    if existing.len() > config.memory_extract_max_memories {
        existing.drain(0..existing.len() - config.memory_extract_max_memories);
    }
    let (system, user) = extraction_prompt(&pending, &existing, chat_type);
    let request = LlmRequest {
        model: config.model.clone(),
        system,
        user,
        max_tokens: config.memory_extract_tokens,
        tools: Vec::new(),
        messages: Vec::new(),
    };

    let response = match llm.complete(request).await {
        Ok(response) => response,
        // No model: nothing to extract, and no point retrying every message.
        Err(LlmError::Disabled) => return Ok(0),
        Err(error) => return Err(error.to_string()),
    };

    let by_id: HashMap<&str, &StoredMessage> = pending
        .iter()
        .map(|message| (message.message_id.as_str(), message))
        .collect();
    let existing_ids: HashSet<&str> = existing.iter().map(|record| record.id.as_str()).collect();
    let ops: Vec<MemoryOp> = parse_ops(&response.text)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|raw| validate_op(raw, chat_type, &by_id, &existing_ids))
        .collect();

    let cursor = pending.last().map(|message| message.message_id.clone());
    memory.apply(chat, &ops, cursor)
}

/// The messages a chat has said since its last pass, newest end kept.
///
/// A cursor that is no longer in the window — pruned before it was processed —
/// is treated as "nothing processed yet", so the whole window is reconsidered
/// rather than silently skipped. A backlog longer than the cap keeps its newest
/// end: the older unprocessed messages are being pruned anyway, and holding the
/// pass back to replay them would delay every fact behind them.
fn pending_messages(
    messages: &MessageStore,
    memory: &MemoryStore,
    chat: &ChatId,
    config: &AgentConfig,
) -> Result<Vec<StoredMessage>, String> {
    let window = messages.recent(chat, config.max_stored_messages)?;
    let start = memory
        .cursor(chat)?
        .and_then(|cursor| {
            window
                .iter()
                .position(|message| message.message_id == cursor)
                .map(|index| index + 1)
        })
        .unwrap_or(0);
    let mut pending = window[start..].to_vec();
    if pending.len() > config.memory_extract_max_messages {
        let excess = pending.len() - config.memory_extract_max_messages;
        pending.drain(0..excess);
    }
    Ok(pending)
}

/// Whether a pass is due: enough new messages, or a quiet backlog.
///
/// The idle check looks at the *second*-newest message, so the message that
/// triggered this call never counts as its own backlog — a chat that has gone
/// quiet fires, a chat that just spoke does not.
fn is_due(pending: &[StoredMessage], config: &AgentConfig, now: DateTime<Utc>) -> bool {
    if pending.len() >= config.memory_extract_batch {
        return true;
    }
    pending.len() > 1
        && now
            .signed_duration_since(pending[pending.len() - 2].timestamp)
            .to_std()
            .is_ok_and(|idle| idle >= config.memory_extract_idle)
}

/// The extraction prompt: the batch to read and the facts already stored.
fn extraction_prompt(
    pending: &[StoredMessage],
    existing: &[MemoryRecord],
    chat_type: ChatType,
) -> (String, String) {
    let where_ = match chat_type {
        ChatType::Group => "a group chat",
        ChatType::Private => "a private one-on-one chat",
    };
    let system = format!(
        "You extract durable facts from {where_} into a long-term memory store. You are \
         given the recent messages and the facts already stored for this chat. Reply with a \
         JSON array of operations and nothing else.\n\n\
         Operations:\n\
         - {{\"op\":\"ADD\",\"content\":\"...\",\"subject\":\"<sender id, or omit>\",\"confidence\":0.0-1.0,\"importance\":0.0-1.0,\"evidence\":[\"<message id>\", ...]}}\n\
         - {{\"op\":\"UPDATE\",\"target\":\"<stored fact id>\",\"content\":\"...\",\"confidence\":0.0-1.0,\"importance\":0.0-1.0,\"evidence\":[\"<message id>\", ...]}}\n\
         - {{\"op\":\"INVALIDATE\",\"target\":\"<stored fact id>\",\"evidence\":[\"<message id>\", ...]}}\n\
         - {{\"op\":\"NOOP\"}}\n\n\
         Rules: keep only durable facts — about people, places, plans, preferences, or \
         events — never small talk. Write each fact self-contained and in the third person. \
         Every ADD, UPDATE, and INVALIDATE must cite in `evidence` the message ids it came \
         from. To correct or replace a stored fact, use UPDATE with its id; to mark one no \
         longer true with no replacement, use INVALIDATE. When nothing is worth keeping, \
         reply exactly [{{\"op\":\"NOOP\"}}]. Reply with only the JSON array."
    );

    let mut user = String::new();
    if !existing.is_empty() {
        user.push_str("<stored_facts>\n");
        for record in existing {
            user.push_str("<fact id=\"");
            user.push_str(&escape(&record.id));
            user.push_str("\">");
            user.push_str(&escape(&record.content));
            user.push_str("</fact>\n");
        }
        user.push_str("</stored_facts>\n\n");
    }
    user.push_str("<recent_messages>\n");
    for message in pending {
        let name = message.sender_name.as_deref().unwrap_or("");
        user.push_str(&format!(
            "<chat_message id=\"{}\" sender=\"{}\" name=\"{}\" ts=\"{}\">{}</chat_message>\n",
            escape(&message.message_id),
            escape(message.sender.as_str()),
            escape(name),
            message.timestamp.to_rfc3339(),
            escape(message.text.as_deref().unwrap_or("")),
        ));
    }
    user.push_str("</recent_messages>");
    (system, user)
}

/// One operation as the model wrote it, before validation.
#[derive(Deserialize)]
struct RawOp {
    #[serde(default)]
    op: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    importance: Option<f32>,
    #[serde(default)]
    evidence: Vec<String>,
}

/// The JSON array in a model reply, however it was wrapped.
///
/// The prompt asks for a bare array, but a model may fence it in a code block or
/// add a sentence around it, so the fence is stripped and, failing a direct
/// parse, the widest bracketed slice is tried. `None` means nothing parseable,
/// which the caller treats as "no facts this pass".
fn parse_ops(text: &str) -> Option<Vec<RawOp>> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("```")
        .map(|rest| {
            rest.strip_prefix("json")
                .or_else(|| rest.strip_prefix("JSON"))
                .unwrap_or(rest)
        })
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);

    if let Ok(ops) = serde_json::from_str::<Vec<RawOp>>(body) {
        return Some(ops);
    }
    let start = body.find('[')?;
    let end = body.rfind(']')?;
    serde_json::from_str::<Vec<RawOp>>(&body[start..=end]).ok()
}

/// Validates one raw op against the batch and the stored facts.
///
/// Everything the model could get wrong is checked here, and a bad op is dropped
/// rather than allowed to corrupt the store: the op must be one of the four, an
/// add or update must carry content, every evidence id must be a message in this
/// batch, and an update or invalidate must name a fact that exists. The
/// visibility is derived from the chat, never read from the model.
fn validate_op(
    raw: RawOp,
    chat_type: ChatType,
    pending: &HashMap<&str, &StoredMessage>,
    existing: &HashSet<&str>,
) -> Option<MemoryOp> {
    let visibility = match chat_type {
        ChatType::Private => Visibility::Private,
        ChatType::Group => Visibility::Chat,
    };
    match raw.op.trim().to_ascii_uppercase().as_str() {
        "NOOP" => Some(MemoryOp::Noop),
        "ADD" | "UPDATE" => {
            let content = raw.content?;
            let content = content.trim();
            if content.is_empty() {
                return None;
            }
            let fact = NewFact {
                content: truncate(content, MAX_CONTENT_CHARS),
                visibility,
                // A subject is kept only when it names someone in the batch, so
                // the model cannot attribute a fact to an invented person.
                subject: raw
                    .subject
                    .filter(|subject| {
                        pending
                            .values()
                            .any(|message| message.sender.as_str() == subject)
                    })
                    .map(SenderId::new),
                confidence: raw.confidence.unwrap_or(0.5).clamp(0.0, 1.0),
                importance: raw.importance.unwrap_or(0.5).clamp(0.0, 1.0),
                valid_from: earliest_evidence(&raw.evidence, pending)?,
                evidence: raw.evidence,
            };
            if raw.op.trim().eq_ignore_ascii_case("ADD") {
                Some(MemoryOp::Add(fact))
            } else {
                let target = raw.target?;
                existing
                    .contains(target.as_str())
                    .then_some(MemoryOp::Update { target, fact })
            }
        }
        "INVALIDATE" => {
            let target = raw.target?;
            if !existing.contains(target.as_str()) {
                return None;
            }
            Some(MemoryOp::Invalidate {
                target,
                valid_from: earliest_evidence(&raw.evidence, pending)?,
            })
        }
        _ => None,
    }
}

/// The earliest timestamp among `ids`, or `None` if any is not in the batch.
///
/// Requiring every id to resolve is what grounds a fact in time: a fact cannot
/// be stamped with a moment its evidence does not contain.
fn earliest_evidence(
    ids: &[String],
    pending: &HashMap<&str, &StoredMessage>,
) -> Option<DateTime<Utc>> {
    if ids.is_empty() {
        return None;
    }
    let mut earliest: Option<DateTime<Utc>> = None;
    for id in ids {
        let timestamp = pending.get(id.as_str())?.timestamp;
        earliest = Some(match earliest {
            Some(current) => current.min(timestamp),
            None => timestamp,
        });
    }
    earliest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ScriptedLlm;
    use crate::memory::store::MemoryStore;
    use crate::store::StoredMessage;
    use chrono::Utc;

    fn stored(id: &str, sender: &str, text: &str, minutes_ago: i64) -> StoredMessage {
        StoredMessage {
            message_id: id.into(),
            sender: SenderId::new(sender),
            sender_name: Some(sender.into()),
            text: Some(text.into()),
            from_self: false,
            timestamp: Utc::now() - chrono::Duration::minutes(minutes_ago),
        }
    }

    fn scripted(text: &str) -> ScriptedLlm {
        ScriptedLlm::new([crate::llm::LlmResponse {
            text: text.into(),
            ..Default::default()
        }])
    }

    fn test_config() -> AgentConfig {
        let mut config = AgentConfig::for_test();
        config.memory_extract_batch = 2;
        config
    }

    #[tokio::test]
    async fn an_add_op_is_validated_and_applied() {
        let messages = MessageStore::open(":memory:", 200).unwrap();
        let memory = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        messages
            .append(&chat, stored("m1", "u1", "the venue is the old hall", 1))
            .unwrap();
        messages
            .append(&chat, stored("m2", "u2", "noted", 0))
            .unwrap();

        let llm = scripted(
            r#"[{"op":"ADD","content":"The venue is the old hall.","subject":"u1","confidence":0.9,"importance":0.7,"evidence":["m1"]}]"#,
        );
        let applied = extract_if_due(
            &llm,
            &messages,
            &memory,
            &test_config(),
            &chat,
            ChatType::Group,
        )
        .await
        .unwrap();

        assert_eq!(applied, 1);
        let record = &memory.records().unwrap()[0];
        assert_eq!(record.content, "The venue is the old hall.");
        assert_eq!(record.visibility, Visibility::Chat);
        assert_eq!(record.subject, Some(SenderId::new("u1")));
        assert_eq!(record.source_message_ids, ["m1"]);
        assert_eq!(memory.cursor(&chat).unwrap().as_deref(), Some("m2"));
    }

    #[tokio::test]
    async fn an_op_with_unknown_evidence_is_dropped() {
        let messages = MessageStore::open(":memory:", 200).unwrap();
        let memory = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        messages.append(&chat, stored("m1", "u1", "a", 1)).unwrap();
        messages.append(&chat, stored("m2", "u2", "b", 0)).unwrap();

        // The first op cites a message that is not in the batch; the second is
        // sound, so one lands and one does not.
        let llm = scripted(
            r#"[{"op":"ADD","content":"bogus","evidence":["ghost"]},
                {"op":"ADD","content":"The venue is the old hall.","evidence":["m1"]}]"#,
        );
        let applied = extract_if_due(
            &llm,
            &messages,
            &memory,
            &test_config(),
            &chat,
            ChatType::Group,
        )
        .await
        .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(memory.len().unwrap(), 1);
        assert_eq!(
            memory.records().unwrap()[0].content,
            "The venue is the old hall."
        );
    }

    #[tokio::test]
    async fn unparseable_output_advances_the_cursor_and_stores_nothing() {
        let messages = MessageStore::open(":memory:", 200).unwrap();
        let memory = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        messages.append(&chat, stored("m1", "u1", "a", 1)).unwrap();
        messages.append(&chat, stored("m2", "u2", "b", 0)).unwrap();

        let llm = scripted("I could not find anything worth keeping.");
        let applied = extract_if_due(
            &llm,
            &messages,
            &memory,
            &test_config(),
            &chat,
            ChatType::Group,
        )
        .await
        .unwrap();
        assert_eq!(applied, 0);
        assert!(memory.is_empty().unwrap());
        // The cursor moved, so the next call does not re-run this batch.
        assert_eq!(memory.cursor(&chat).unwrap().as_deref(), Some("m2"));
    }

    #[tokio::test]
    async fn a_fenced_json_reply_is_parsed() {
        let messages = MessageStore::open(":memory:", 200).unwrap();
        let memory = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        messages.append(&chat, stored("m1", "u1", "a", 1)).unwrap();
        messages.append(&chat, stored("m2", "u2", "b", 0)).unwrap();

        let llm = scripted(
            "Here you go:\n```json\n[{\"op\":\"ADD\",\"content\":\"A fact.\",\"evidence\":[\"m1\"]}]\n```",
        );
        let applied = extract_if_due(
            &llm,
            &messages,
            &memory,
            &test_config(),
            &chat,
            ChatType::Group,
        )
        .await
        .unwrap();
        assert_eq!(applied, 1);
    }

    #[tokio::test]
    async fn a_private_chat_writes_a_private_fact() {
        let messages = MessageStore::open(":memory:", 200).unwrap();
        let memory = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("dm");
        messages.append(&chat, stored("m1", "u1", "a", 1)).unwrap();
        messages.append(&chat, stored("m2", "u1", "b", 0)).unwrap();

        let llm =
            scripted(r#"[{"op":"ADD","content":"Budi lives in Bandung.","evidence":["m1"]}]"#);
        extract_if_due(
            &llm,
            &messages,
            &memory,
            &test_config(),
            &chat,
            ChatType::Private,
        )
        .await
        .unwrap();
        assert_eq!(memory.records().unwrap()[0].visibility, Visibility::Private);
    }

    #[tokio::test]
    async fn a_quiet_backlog_fires_but_a_lone_message_does_not() {
        let config = AgentConfig::for_test(); // idle trigger is effectively off
        let lone = vec![stored("m1", "u1", "a", 30)];
        assert!(!is_due(&lone, &config, Utc::now()));

        let mut config = AgentConfig::for_test();
        config.memory_extract_idle = std::time::Duration::from_secs(60);
        let backlog = vec![stored("m1", "u1", "a", 30), stored("m2", "u1", "b", 20)];
        assert!(is_due(&backlog, &config, Utc::now()));

        // The newest message alone is not a backlog: two messages sent together
        // are not idle.
        let fresh = vec![stored("m1", "u1", "a", 0), stored("m2", "u1", "b", 0)];
        assert!(!is_due(&fresh, &config, Utc::now()));
    }

    #[tokio::test]
    async fn a_pruned_cursor_reconsiders_the_whole_window() {
        let messages = MessageStore::open(":memory:", 2).unwrap();
        let memory = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        memory.apply(&chat, &[], Some("gone".into())).unwrap();
        for i in 0..3 {
            messages
                .append(&chat, stored(&format!("m{i}"), "u1", "x", 0))
                .unwrap();
        }
        let config = AgentConfig::for_test();
        let pending = pending_messages(&messages, &memory, &chat, &config).unwrap();
        // The cursor is not in the two-message window, so nothing is skipped.
        assert_eq!(pending.len(), 2);
    }
}
