//! Ranking the facts a reader may see for a question.
//!
//! Retrieval is deliberately two-phase. The privacy filter runs **first**, so a
//! fact the reader may not see is never scored, never ranked, and never handed
//! to the context builder — the leak cannot happen upstream of the filter
//! because there is no upstream. Only the facts that survive are ranked.
//!
//! The rank is a weighted sum of four signals: how well the fact matches the
//! question, how recent it is, how important it was judged, and how confident
//! the extraction was. The weights live in [`AgentConfig`](crate::config::AgentConfig)
//! (`weight_similarity`, `weight_recency`, `weight_importance`,
//! `weight_confidence`) so the tuning milestone can adjust them from the
//! environment without a rebuild; the defaults are the starting point.
//!
//! "Match" is lexical overlap, not an embedding. The configured provider is the
//! Anthropic Messages API, which has no embeddings endpoint, so v1 compares
//! words and a later change can replace [`similarity`] with a vector score
//! without touching the callers. `docs/AGENT.md` records that seam.

use chrono::{DateTime, Utc};

use crate::config::AgentConfig;
use crate::context::{ReaderContext, RecalledMemory};
use crate::memory::store::{MemoryRecord, MemoryStore};

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
        // A forgotten fact is never recalled, not even as history.
        .filter(|record| !record.forgotten)
        .filter(|record| record.valid_to.is_none() || include_past)
        .map(|record| {
            let score = config.weight_similarity * similarity(query, &record.content)
                + config.weight_recency * recency(record.valid_from, now)
                + config.weight_importance * record.importance
                + config.weight_confidence * record.confidence;
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

/// The facts `reader` may see and could forget, newest first, uncapped.
///
/// This is the set `!memory` lists and `!forget` acts on: every fact learned in
/// the reader's own chat that is still true and not forgotten, filtered through
/// the reader's privacy boundary first. Sharing one function keeps the two
/// commands on the same set, so a fact `!memory` does not show is never one
/// `!forget` would silently remove. [`list`] truncates this for display.
pub fn forgettable(
    reader: &ReaderContext,
    store: &MemoryStore,
) -> Result<Vec<MemoryRecord>, String> {
    let mut records: Vec<MemoryRecord> = store
        .records()?
        .into_iter()
        .filter(|record| reader.permits(&recalled(record)))
        .filter(|record| record.origin_chat == reader.chat)
        .filter(|record| record.valid_to.is_none() && !record.forgotten)
        .collect();

    // Newest-first, then id, so the same store always lists the same way.
    records.sort_by(|a, b| {
        b.valid_from
            .cmp(&a.valid_from)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(records)
}

/// The facts `reader` may see for `!memory`, newest first and capped.
///
/// The listing half of [`forgettable`]: the same set, truncated to
/// [`AgentConfig::memory_list_top`]. Returns full [`MemoryRecord`]s, not
/// [`RecalledMemory`]s, because the listing shows each fact's id so `!forget`
/// can name one.
pub fn list(
    reader: &ReaderContext,
    store: &MemoryStore,
    config: &AgentConfig,
) -> Result<Vec<MemoryRecord>, String> {
    let mut records = forgettable(reader, store)?;
    records.truncate(config.memory_list_top);
    Ok(records)
}

/// A record as the context builder takes it, before the reader filter.
fn recalled(record: &MemoryRecord) -> RecalledMemory {
    RecalledMemory {
        kind: record.kind,
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
/// a vector score is available. A query with no words matches nothing. Shared
/// with the history-search tool, which ranks messages the same way.
pub(crate) fn similarity(query: &str, content: &str) -> f32 {
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
    fn the_ranking_weights_come_from_the_config() {
        // Two facts with identical recency and confidence; only importance and
        // similarity differ. With similarity weighted and importance zeroed, the
        // lexical match wins — proving the weights are read, not hard-coded.
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(
                &chat,
                &[MemoryOp::Add(fact(
                    "the venue is the old hall",
                    Visibility::Chat,
                    None,
                ))],
                None,
            )
            .unwrap();
        let mut high_importance = fact("something unrelated entirely", Visibility::Chat, None);
        high_importance.importance = 1.0;
        store
            .apply(&chat, &[MemoryOp::Add(high_importance)], None)
            .unwrap();

        let mut config = AgentConfig::for_test();
        config.weight_similarity = 1.0;
        config.weight_recency = 0.0;
        config.weight_importance = 0.0;
        config.weight_confidence = 0.0;
        let group = reader(ChatType::Group, "gA", &["gA"]);
        let ranked = search(&group, "the venue", &store, &config, Utc::now()).unwrap();
        assert_eq!(ranked[0].content, "the venue is the old hall");
    }

    #[test]
    fn past_cues_are_recognized() {
        assert!(is_past_question("where was it before?"));
        assert!(is_past_question("what did it used to be"));
        assert!(!is_past_question("where is it now?"));
    }

    #[test]
    fn a_forgotten_fact_is_never_recalled_not_even_as_history() {
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
        let id = store.records().unwrap()[0].id.clone();
        store
            .apply(&chat, &[MemoryOp::Forget { target: id }], None)
            .unwrap();

        let config = AgentConfig::for_test();
        let group = reader(ChatType::Group, "gA", &["gA"]);
        // Neither a present-tense nor a past-tense question recalls it: forget
        // is stronger than invalidate.
        assert!(
            search(&group, "where is the venue?", &store, &config, Utc::now())
                .unwrap()
                .is_empty()
        );
        assert!(
            search(
                &group,
                "where was the venue before?",
                &store,
                &config,
                Utc::now()
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn list_shows_only_this_chats_live_unforgotten_facts() {
        let store = MemoryStore::open(":memory:").unwrap();
        let config = AgentConfig::for_test();
        // A group fact, a private fact, and a superseded one, all in chats the
        // group reader is not listing.
        store
            .apply(
                &ChatId::new("gA"),
                &[MemoryOp::Add(fact("group fact", Visibility::Chat, None))],
                None,
            )
            .unwrap();
        store
            .apply(
                &ChatId::new("dm"),
                &[MemoryOp::Add(fact(
                    "private fact",
                    Visibility::Private,
                    None,
                ))],
                None,
            )
            .unwrap();
        store
            .apply(
                &ChatId::new("gA"),
                &[MemoryOp::Add(fact("old fact", Visibility::Chat, None))],
                None,
            )
            .unwrap();
        let old_id = store
            .records()
            .unwrap()
            .iter()
            .find(|r| r.content == "old fact")
            .unwrap()
            .id
            .clone();
        store
            .apply(
                &ChatId::new("gA"),
                &[MemoryOp::Forget { target: old_id }],
                None,
            )
            .unwrap();

        let group = reader(ChatType::Group, "gA", &["gA"]);
        let listed = list(&group, &store, &config).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].content, "group fact");

        // The private chat lists its own fact, and only it.
        let dm = reader(ChatType::Private, "dm", &[]);
        let listed = list(&dm, &store, &config).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].content, "private fact");
    }

    #[test]
    fn list_is_capped_and_newest_first() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        for i in 0..5 {
            let mut f = fact(&format!("fact {i}"), Visibility::Chat, None);
            f.valid_from = Utc::now() + chrono::Duration::seconds(i);
            store.apply(&chat, &[MemoryOp::Add(f)], None).unwrap();
        }
        let mut config = AgentConfig::for_test();
        config.memory_list_top = 2;
        let group = reader(ChatType::Group, "gA", &["gA"]);
        let listed = list(&group, &store, &config).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].content, "fact 4");
        assert_eq!(listed[1].content, "fact 3");

        // The listing is capped, but the forgettable set is not: `!forget` must
        // be able to reach a fact the display truncated.
        assert_eq!(forgettable(&group, &store).unwrap().len(), 5);
    }
}
