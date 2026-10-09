//! Turning a chat's facts into higher-level insights, with the model.
//!
//! The extraction writer ([`writer`](crate::memory::writer)) stores what a chat
//! *said*; this pass stores what those facts *add up to*. When enough new facts
//! have landed, it hands the model the chat's live facts and asks for a batch of
//! insights that follow from combining them — recurring preferences, durable
//! relationships, long-running plans — each citing the facts it came from.
//!
//! The model only *proposes*. As in extraction, every field that carries
//! privilege or time is set here, in code: the visibility label comes from the
//! chat's type, the validity start from the cited facts, and an op that cites a
//! fact this chat does not have is dropped. A model cannot widen a reflection's
//! reach or ground it in a source that does not exist.
//!
//! A pass runs when enough new facts have accumulated since the last one, and it
//! advances its own per-chat cursor on any model answer — including one that
//! stores nothing — so a chat with nothing to reflect on is not asked again
//! until new facts arrive. It is one model call and one store write, and it runs
//! inside the per-chat turn lock so it never races the turn that reads the same
//! chat's memory.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::config::AgentConfig;
use crate::context::{Visibility, truncate};
use crate::event::{ChatId, ChatType};
use crate::llm::{LlmClient, LlmError, LlmRequest};
use crate::memory::consolidate;
use crate::memory::store::{MemoryKind, MemoryOp, MemoryRecord, MemoryStore, NewFact};
use crate::memory::writer::{MAX_CONTENT_CHARS, RawOp, parse_ops, render_facts};

/// Runs a consolidation pass for `chat` when one is due.
///
/// Returns how many insights were stored, or `0` when no pass was due, the pass
/// is disabled, or no model is configured. A transport failure is returned as an
/// error and leaves the cursor where it was, so the pass is retried; a model that
/// answered but produced unparseable output advances the cursor and stores
/// nothing, so a bad reply cannot wedge the chat into retrying it forever.
pub async fn reflect_if_due(
    llm: &dyn LlmClient,
    memory: &MemoryStore,
    config: &AgentConfig,
    chat: &ChatId,
    chat_type: ChatType,
) -> Result<usize, String> {
    if !config.reflection_enabled {
        return Ok(0);
    }

    let all = memory.records()?;
    let start = memory
        .reflection_cursor(chat)?
        .and_then(|cursor| all.iter().position(|record| record.id == cursor))
        .map_or(0, |index| index + 1);
    let is_live_fact = |record: &MemoryRecord| {
        record.kind == MemoryKind::Fact
            && record.origin_chat == *chat
            && record.valid_to.is_none()
            && !record.forgotten
    };
    // A fact already reflected on is behind the cursor; only new facts count
    // toward the threshold, so a chat is not asked to reflect on every turn.
    // A batch of zero is read as one: the pass needs at least one new fact to
    // have anything to consolidate, so it can never re-fire on an unchanged chat.
    let new_facts = all[start..]
        .iter()
        .filter(|record| is_live_fact(record))
        .count();
    if new_facts < config.reflection_batch.max(1) {
        return Ok(0);
    }

    // The corpus is every live fact of this chat, newest end kept, so the model
    // can combine a new fact with older ones it has not seen together before.
    let mut corpus: Vec<MemoryRecord> = all
        .iter()
        .filter(|record| is_live_fact(record))
        .cloned()
        .collect();
    if corpus.len() > config.reflection_max_facts {
        corpus.drain(0..corpus.len() - config.reflection_max_facts);
    }

    let (system, user) = reflection_prompt(&corpus, chat_type);
    let request = LlmRequest {
        model: config.model.clone(),
        system,
        user,
        max_tokens: config.reflection_max_tokens,
        tools: Vec::new(),
        messages: Vec::new(),
    };

    let response = match llm.complete(request).await {
        Ok(response) => response,
        // No model: nothing to reflect, and no point retrying every message.
        Err(LlmError::Disabled) => return Ok(0),
        Err(error) => return Err(error.to_string()),
    };

    let live_by_id: HashMap<&str, &MemoryRecord> = corpus
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect();
    let ops: Vec<MemoryOp> = parse_ops(&response.text)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|raw| validate_reflection(raw, chat_type, &live_by_id))
        .collect();
    // Dedup against every live record of this chat — the corpus holds only
    // facts, but an insight must also not repeat a reflection already stored.
    let live: Vec<MemoryRecord> = all
        .iter()
        .filter(|record| {
            record.origin_chat == *chat && record.valid_to.is_none() && !record.forgotten
        })
        .cloned()
        .collect();
    let ops = consolidate::dedup_adds(ops, &live, config.dedup_threshold);

    // The cursor is the newest live fact the pass saw. Records are never
    // deleted, so the id always resolves on the next pass.
    let cursor = corpus.last().map(|record| record.id.clone());
    memory.apply_reflections(chat, &ops, cursor)
}

/// Validates one raw reflection op against the chat's live facts.
///
/// As in extraction, everything the model could get wrong is checked here: the
/// op must be `ADD` or `NOOP`, an add must carry content, and every evidence id
/// must name a live fact of this chat. The visibility is derived from the chat,
/// never read from the model.
fn validate_reflection(
    raw: RawOp,
    chat_type: ChatType,
    live: &HashMap<&str, &MemoryRecord>,
) -> Option<MemoryOp> {
    match raw.op.trim().to_ascii_uppercase().as_str() {
        "NOOP" => Some(MemoryOp::Noop),
        "ADD" => {
            let content = raw.content?;
            let content = content.trim();
            if content.is_empty() {
                return None;
            }
            Some(MemoryOp::Reflect(NewFact {
                content: truncate(content, MAX_CONTENT_CHARS),
                visibility: visibility_for(chat_type),
                // A reflection is about the chat, not a person, so it carries no
                // subject — the model cannot attribute an insight to someone.
                subject: None,
                confidence: raw.confidence.unwrap_or(0.5).clamp(0.0, 1.0),
                importance: raw.importance.unwrap_or(0.5).clamp(0.0, 1.0),
                valid_from: earliest_evidence(&raw.evidence, live)?,
                evidence: raw.evidence,
            }))
        }
        _ => None,
    }
}

/// The visibility a fact or reflection learned in `chat_type` gets.
///
/// Derived from the chat, never from the model, so an insight can never be
/// widened past the boundary the facts behind it already live behind.
fn visibility_for(chat_type: ChatType) -> Visibility {
    match chat_type {
        ChatType::Private => Visibility::Private,
        ChatType::Group => Visibility::Chat,
    }
}

/// The earliest `valid_from` among `ids`, or `None` if any id is not a live fact.
///
/// Requiring every id to resolve is what grounds a reflection in its sources: it
/// cannot be stamped with a moment its cited facts do not contain, and it cannot
/// cite a fact this chat does not have.
fn earliest_evidence(ids: &[String], live: &HashMap<&str, &MemoryRecord>) -> Option<DateTime<Utc>> {
    if ids.is_empty() {
        return None;
    }
    let mut earliest: Option<DateTime<Utc>> = None;
    for id in ids {
        let valid_from = live.get(id.as_str())?.valid_from;
        earliest = Some(match earliest {
            Some(current) => current.min(valid_from),
            None => valid_from,
        });
    }
    earliest
}

/// The consolidation prompt: the chat's facts, and the ask for insights.
fn reflection_prompt(corpus: &[MemoryRecord], chat_type: ChatType) -> (String, String) {
    let where_ = match chat_type {
        ChatType::Group => "a group chat",
        ChatType::Private => "a private one-on-one chat",
    };
    let system = format!(
        "You consolidate the durable facts stored for {where_} into higher-level insights. You \
         are given the facts already stored for this chat. Reply with a JSON array of operations \
         and nothing else.\n\n\
         Operations:\n\
         - {{\"op\":\"ADD\",\"content\":\"...\",\"confidence\":0.0-1.0,\"importance\":0.0-1.0,\"evidence\":[\"<stored fact id>\", ...]}}\n\
         - {{\"op\":\"NOOP\"}}\n\n\
         Rules: add only durable, general insights that follow from combining the cited facts — \
         recurring preferences, relationships, patterns, or long-running plans. Do not restate a \
         single stored fact. Write each insight self-contained and in the third person. Every ADD \
         must cite in `evidence` the stored fact ids it was derived from. When nothing new can be \
         inferred, reply exactly [{{\"op\":\"NOOP\"}}]. Reply with only the JSON array."
    );
    (system, render_facts(corpus))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmResponse, ScriptedLlm};
    use crate::memory::store::{MemoryOp, NewFact};
    use chrono::Utc;

    fn new_fact(content: &str) -> NewFact {
        NewFact {
            content: content.into(),
            visibility: Visibility::Chat,
            subject: None,
            confidence: 0.8,
            importance: 0.5,
            valid_from: Utc::now(),
            evidence: vec!["m1".into()],
        }
    }

    /// A store with `count` live facts, returning the store and the chat.
    fn seeded(count: usize) -> (MemoryStore, ChatId) {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        for i in 0..count {
            store
                .apply(
                    &chat,
                    &[MemoryOp::Add(new_fact(&format!("fact {i}")))],
                    None,
                )
                .unwrap();
        }
        (store, chat)
    }

    fn scripted(text: &str) -> ScriptedLlm {
        ScriptedLlm::new([LlmResponse {
            text: text.into(),
            ..Default::default()
        }])
    }

    fn config() -> AgentConfig {
        let mut config = AgentConfig::for_test();
        config.reflection_enabled = true;
        config.reflection_batch = 2;
        config
    }

    #[tokio::test]
    async fn a_pass_stores_an_insight_citing_its_sources() {
        let (store, chat) = seeded(2);
        let ids: Vec<String> = store
            .records()
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        let reply = format!(
            r#"[{{"op":"ADD","content":"They keep meeting on Fridays.","confidence":0.7,"importance":0.6,"evidence":["{}","{}"]}}]"#,
            ids[0], ids[1]
        );
        let applied = reflect_if_due(&scripted(&reply), &store, &config(), &chat, ChatType::Group)
            .await
            .unwrap();

        assert_eq!(applied, 1);
        let records = store.records().unwrap();
        let reflection = records
            .iter()
            .find(|r| r.kind == MemoryKind::Reflection)
            .unwrap();
        assert_eq!(reflection.content, "They keep meeting on Fridays.");
        assert_eq!(reflection.visibility, Visibility::Chat);
        assert_eq!(reflection.source_message_ids, ids);
    }

    #[tokio::test]
    async fn an_insight_already_stored_is_not_stored_again() {
        // Two passes over the same facts: the second proposes the same insight,
        // which dedup drops against the reflection the first pass stored.
        let (store, chat) = seeded(2);
        let ids: Vec<String> = store
            .records()
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        let reply = format!(
            r#"[{{"op":"ADD","content":"They keep meeting on Fridays.","evidence":["{}"]}}]"#,
            ids[0]
        );
        reflect_if_due(&scripted(&reply), &store, &config(), &chat, ChatType::Group)
            .await
            .unwrap();
        // Two new facts re-trigger the pass (the batch is 2), which repeats the
        // insight the first pass already stored.
        store
            .apply(
                &chat,
                &[
                    MemoryOp::Add(new_fact("a third fact")),
                    MemoryOp::Add(new_fact("a fourth fact")),
                ],
                None,
            )
            .unwrap();
        let applied = reflect_if_due(&scripted(&reply), &store, &config(), &chat, ChatType::Group)
            .await
            .unwrap();
        assert_eq!(applied, 0, "the repeated insight should have been deduped");

        let reflections = store
            .records()
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MemoryKind::Reflection)
            .count();
        assert_eq!(reflections, 1, "the repeated insight was stored twice");
    }

    #[tokio::test]
    async fn below_the_batch_no_pass_runs() {
        let (store, chat) = seeded(1);
        let llm = scripted("unused");
        let applied = reflect_if_due(&llm, &store, &config(), &chat, ChatType::Group)
            .await
            .unwrap();
        assert_eq!(applied, 0);
        assert!(llm.requests().is_empty());
    }

    #[tokio::test]
    async fn an_op_citing_a_fact_that_does_not_exist_is_dropped() {
        let (store, chat) = seeded(2);
        let reply = r#"[{"op":"ADD","content":"Bogus insight.","evidence":["ghost"]}]"#;
        let applied = reflect_if_due(&scripted(reply), &store, &config(), &chat, ChatType::Group)
            .await
            .unwrap();
        assert_eq!(applied, 0);
        assert_eq!(store.len().unwrap(), 2, "only the seeded facts remain");
    }

    #[tokio::test]
    async fn a_private_chat_stores_a_private_reflection() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("dm");
        store
            .apply(
                &chat,
                &[MemoryOp::Add(new_fact("a")), MemoryOp::Add(new_fact("b"))],
                None,
            )
            .unwrap();
        let ids: Vec<String> = store
            .records()
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        let reply = format!(
            r#"[{{"op":"ADD","content":"An insight.","evidence":["{}"]}}]"#,
            ids[0]
        );
        reflect_if_due(
            &scripted(&reply),
            &store,
            &config(),
            &chat,
            ChatType::Private,
        )
        .await
        .unwrap();
        let reflection = store
            .records()
            .unwrap()
            .into_iter()
            .find(|r| r.kind == MemoryKind::Reflection)
            .unwrap();
        assert_eq!(reflection.visibility, Visibility::Private);
    }

    #[tokio::test]
    async fn a_noop_advances_the_cursor_so_the_pass_does_not_spin() {
        let (store, chat) = seeded(2);
        let llm = scripted(r#"[{"op":"NOOP"}]"#);
        let applied = reflect_if_due(&llm, &store, &config(), &chat, ChatType::Group)
            .await
            .unwrap();
        assert_eq!(applied, 0);
        assert_eq!(llm.requests().len(), 1);
        // A second call with no new facts makes no second model call.
        reflect_if_due(&llm, &store, &config(), &chat, ChatType::Group)
            .await
            .unwrap();
        assert_eq!(llm.requests().len(), 1, "the cursor stopped the re-run");
    }

    #[tokio::test]
    async fn reflections_off_makes_no_call() {
        let (store, chat) = seeded(2);
        let llm = scripted("unused");
        let mut config = config();
        config.reflection_enabled = false;
        assert_eq!(
            reflect_if_due(&llm, &store, &config, &chat, ChatType::Group)
                .await
                .unwrap(),
            0
        );
        assert!(llm.requests().is_empty());
    }
}
