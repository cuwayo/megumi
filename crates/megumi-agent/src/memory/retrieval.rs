//! Ranking the facts a reader may see for a question.
//!
//! Retrieval is deliberately two-phase. The privacy filter runs **first**, so a
//! fact the reader may not see is never scored, never ranked, and never handed
//! to the context builder — the leak cannot happen upstream of the filter
//! because there is no upstream. Only the facts that survive are ranked.
//!
//! The rank is a weighted sum of four signals: how well the fact matches the
//! question, how recent it is, how important it was judged, and how confident
//! the extraction was. The weights are constants here, not configuration: they
//! are a starting point the design's evals will tune, and moving them into
//! [`AgentConfig`](crate::config::AgentConfig) before there is evidence for
//! specific values would be guessing in a more elaborate way.
//!
//! "Match" is lexical overlap, not an embedding. The configured provider is the
//! Anthropic Messages API, which has no embeddings endpoint, so v1 compares
//! words and a later milestone can replace [`similarity`] with a vector score
//! without touching the callers. `docs/AGENT.md` records that seam.

use chrono::{DateTime, Utc};

use crate::config::AgentConfig;
use crate::context::{ReaderContext, RecalledMemory};
use crate::memory::store::{MemoryRecord, MemoryStore};

/// How much a fact's lexical match with the question counts.
const W_SIMILARITY: f32 = 0.5;
/// How much a fact's recency counts.
const W_RECENCY: f32 = 0.2;
/// How much a fact's judged importance counts.
const W_IMPORTANCE: f32 = 0.2;
/// How much the extraction's confidence counts.
const W_CONFIDENCE: f32 = 0.1;

/// Words that mark a question as being about the past.
///
/// When one appears, superseded facts are recalled alongside the current ones —
/// "where was the venue before?" needs the old value — and each is rendered with
/// the window it held. Kept small and matched as substrings of the lowercased
/// question; a false positive only adds a little history to the prompt.
const PAST_CUES: &[&str] = &[
    "used to",
    "previously",
    "before",
    "no longer",
    "originally",
    "back then",
    "earlier",
    "at first",
    "changed from",
    "was",
];

/// The memories `reader` may see for `query`, best first.
///
/// Superseded facts are included only when `query` reads as a question about the
/// past. The list is capped at [`AgentConfig::memory_recall_top`], so a chat
/// with a long history still hands the model a small, relevant set.
pub fn search(
    reader: &ReaderContext,
    query: &str,
    store: &MemoryStore,
    config: &AgentConfig,
    now: DateTime<Utc>,
) -> Result<Vec<RecalledMemory>, String> {
    let include_past = is_past_question(query);
    let mut scored: Vec<(f32, MemoryRecord)> = store
        .records()?
        .into_iter()
        // The privacy boundary first: a record the reader may not see is never
        // ranked, so it cannot be recalled by a lucky score.
        .filter(|record| reader.permits(&recalled(record)))
        .filter(|record| record.valid_to.is_none() || include_past)
        .map(|record| {
            let score = W_SIMILARITY * similarity(query, &record.content)
                + W_RECENCY * recency(record.valid_from, now)
                + W_IMPORTANCE * record.importance
                + W_CONFIDENCE * record.confidence;
            (score, record)
        })
        .collect();

    // Ties are broken by newest-first and then id, so the same store always
    // ranks the same way.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.valid_from.cmp(&a.1.valid_from))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    Ok(scored
        .into_iter()
        .take(config.memory_recall_top)
        .map(|(_, record)| recalled(&record))
        .collect())
}

/// A record as the context builder takes it, before the reader filter.
fn recalled(record: &MemoryRecord) -> RecalledMemory {
    RecalledMemory {
        content: record.content.clone(),
        visibility: record.visibility,
        origin_chat: record.origin_chat.clone(),
        subject: record.subject.clone(),
        valid_from: record.valid_from,
        valid_to: record.valid_to,
    }
}

/// How well `content` matches `query`, as the fraction of the query's words it
/// contains.
///
/// Lexical, not semantic: the v1 stand-in for an embedding, to be replaced when
/// a vector score is available. A query with no words matches nothing.
fn similarity(query: &str, content: &str) -> f32 {
    let terms = terms(query);
    if terms.is_empty() {
        return 0.0;
    }
    let content = content.to_lowercase();
    let hits = terms
        .iter()
        .filter(|term| content.contains(term.as_str()))
        .count();
    hits as f32 / terms.len() as f32
}

/// The lowercased word-like terms in `text`.
fn terms(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A fact's recency, in `0.0..=1.0`: `1` today, halving as it ages.
fn recency(valid_from: DateTime<Utc>, now: DateTime<Utc>) -> f32 {
    let age_days = now.signed_duration_since(valid_from).num_seconds().max(0) as f32 / 86_400.0;
    1.0 / (1.0 + age_days)
}

/// Whether `query` reads as a question about the past.
///
/// A one-word cue must be a whole word, so "was" does not match "wash"; a
/// multi-word cue is matched as a substring, which is enough for the short
/// phrases here.
fn is_past_question(query: &str) -> bool {
    let lower = query.to_lowercase();
    let words = terms(query);
    PAST_CUES.iter().any(|cue| {
        if cue.contains(' ') {
            lower.contains(cue)
        } else {
            words.iter().any(|word| word == cue)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Visibility;
    use crate::event::{ChatId, ChatType, SenderId};
    use crate::memory::store::{MemoryOp, NewFact};

    fn reader(chat_type: ChatType, chat: &str, member_of: &[&str]) -> ReaderContext {
        ReaderContext {
            chat: ChatId::new(chat),
            chat_type,
            requester: SenderId::new("u1"),
            member_of: member_of.iter().map(|c| ChatId::new(*c)).collect(),
        }
    }

    fn fact(content: &str, visibility: Visibility, subject: Option<&str>) -> NewFact {
        NewFact {
            content: content.into(),
            visibility,
            subject: subject.map(SenderId::new),
            confidence: 0.8,
            importance: 0.5,
            valid_from: Utc::now(),
            evidence: vec!["m1".into()],
        }
    }

    #[test]
    fn a_private_fact_is_never_returned_to_a_group_reader() {
        let store = MemoryStore::open(":memory:").unwrap();
        store
            .apply(
                &ChatId::new("dm"),
                &[MemoryOp::Add(fact(
                    "Budi lives in Bandung.",
                    Visibility::Private,
                    None,
                ))],
                None,
            )
            .unwrap();
        let config = AgentConfig::for_test();
        let group = reader(ChatType::Group, "gA", &["gA"]);
        assert!(
            search(&group, "where does Budi live?", &store, &config, Utc::now())
                .unwrap()
                .is_empty()
        );
        // The owner's own private chat does see it.
        let dm = reader(ChatType::Private, "dm", &[]);
        assert_eq!(
            search(&dm, "where does Budi live?", &store, &config, Utc::now())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_superseded_fact_is_recalled_only_for_a_question_about_the_past() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(
                &chat,
                &[MemoryOp::Add(fact(
                    "The venue is the old hall.",
                    Visibility::Chat,
                    None,
                ))],
                None,
            )
            .unwrap();
        let old_id = store.records().unwrap()[0].id.clone();
        store
            .apply(
                &chat,
                &[MemoryOp::Update {
                    target: old_id,
                    fact: fact("The venue is the new hall.", Visibility::Chat, None),
                }],
                None,
            )
            .unwrap();

        let config = AgentConfig::for_test();
        let group = reader(ChatType::Group, "gA", &["gA"]);
        let present = search(&group, "where is the venue?", &store, &config, Utc::now()).unwrap();
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].content, "The venue is the new hall.");

        let past = search(
            &group,
            "where was the venue before?",
            &store,
            &config,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(past.len(), 2);
        assert!(past.iter().any(|memory| memory.valid_to.is_some()));
    }

    #[test]
    fn results_are_capped_and_ordered_by_score() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        for i in 0..10 {
            store
                .apply(
                    &chat,
                    &[MemoryOp::Add(fact(
                        &format!("Fact number {i} about the venue."),
                        Visibility::Chat,
                        None,
                    ))],
                    None,
                )
                .unwrap();
        }
        let mut config = AgentConfig::for_test();
        config.memory_recall_top = 3;
        let group = reader(ChatType::Group, "gA", &["gA"]);
        let results = search(&group, "the venue", &store, &config, Utc::now()).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn a_query_with_no_terms_matches_nothing_by_similarity() {
        assert_eq!(similarity("!!!", "anything"), 0.0);
        assert_eq!(similarity("the venue", "the venue is here"), 1.0);
        assert_eq!(similarity("the venue", "nothing relevant"), 0.0);
    }

    #[test]
    fn past_cues_are_recognized() {
        assert!(is_past_question("where was it before?"));
        assert!(is_past_question("what did it used to be"));
        assert!(!is_past_question("where is it now?"));
    }
}
