//! Memory hygiene: dropping a fact the store already knows.
//!
//! Extraction and consolidation both read a chat repeatedly, so the same fact
//! stated twice — or a model paraphrasing what it already stored — would
//! otherwise land twice. [`dedup_adds`] is the one place that suppresses such a
//! duplicate: before a batch is written, each new fact is compared against the
//! facts already stored for the chat, and against the other new facts in the
//! same batch, and dropped when the word overlap reaches the configured
//! threshold.
//!
//! Similarity is lexical, like [`retrieval::similarity`](crate::memory::retrieval),
//! for the same reason: the configured provider has no embeddings endpoint. The
//! comparison is deliberately conservative — it only ever drops an *add*, and
//! only against a fact of the *same kind* — so a reflection paraphrasing a fact
//! is kept (they say different things), while a literal repeat is not.
//!
//! This runs in the config-aware pass, never inside
//! [`MemoryStore::apply`](crate::memory::MemoryStore::apply): dedup is a tuning
//! heuristic whose threshold is configuration, and `apply` must stay a pure,
//! predictable op-applier.

use std::collections::HashSet;

use crate::memory::store::{MemoryKind, MemoryOp, MemoryRecord};

/// Drops the adds in `ops` that near-duplicate a live record or an earlier add.
///
/// `existing` is the chat's records; only the live ones (`valid_to` open, not
/// forgotten) are compared against. `Update`, `Invalidate`, `Forget`, and
/// `Noop` pass through untouched — this only ever suppresses a new fact, so a
/// correction or a forget is never lost. The comparison is same-kind: a fact is
/// checked against facts, a reflection against reflections.
pub(crate) fn dedup_adds(
    ops: Vec<MemoryOp>,
    existing: &[MemoryRecord],
    threshold: f32,
) -> Vec<MemoryOp> {
    let threshold = threshold.clamp(0.0, 1.0);
    let mut facts: Vec<HashSet<String>> = live_contents(existing, MemoryKind::Fact);
    let mut reflections: Vec<HashSet<String>> = live_contents(existing, MemoryKind::Reflection);
    let mut kept = Vec::with_capacity(ops.len());

    for op in ops {
        let (fact, pool) = match &op {
            MemoryOp::Add(fact) => (fact, &mut facts),
            MemoryOp::Reflect(fact) => (fact, &mut reflections),
            _ => {
                kept.push(op);
                continue;
            }
        };
        let tokens = tokens(&fact.content);
        if pool.iter().any(|seen| jaccard(&tokens, seen) >= threshold) {
            continue;
        }
        pool.push(tokens);
        kept.push(op);
    }
    kept
}

/// The token sets of `existing`'s live records of `kind`.
fn live_contents(existing: &[MemoryRecord], kind: MemoryKind) -> Vec<HashSet<String>> {
    existing
        .iter()
        .filter(|record| record.kind == kind && record.valid_to.is_none() && !record.forgotten)
        .map(|record| tokens(&record.content))
        .collect()
}

/// The lowercased word tokens in `text`.
fn tokens(text: &str) -> HashSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The Jaccard similarity of two token sets, in `0.0..=1.0`.
///
/// Two empty sets are identical (1.0); the caller only ever compares validated,
/// non-empty content, so this is a defined edge rather than a reachable one.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    let union = a.union(b).count();
    if union == 0 {
        return 1.0;
    }
    a.intersection(b).count() as f32 / union as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Visibility;
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

    /// A stored record built through the real constructor path.
    fn stored(content: &str) -> MemoryRecord {
        use crate::event::ChatId;
        use crate::memory::store::MemoryStore;
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(&chat, &[MemoryOp::Add(new_fact(content))], None)
            .unwrap();
        store.records().unwrap().pop().unwrap()
    }

    #[test]
    fn an_identical_add_is_dropped() {
        let existing = vec![stored("the venue is the old hall")];
        let ops = vec![MemoryOp::Add(new_fact("The venue is the old hall"))];
        assert!(dedup_adds(ops, &existing, 0.85).is_empty());
    }

    #[test]
    fn a_distinct_add_is_kept() {
        let existing = vec![stored("the venue is the old hall")];
        let ops = vec![MemoryOp::Add(new_fact("Budi lives in Bandung"))];
        assert_eq!(dedup_adds(ops, &existing, 0.85).len(), 1);
    }

    #[test]
    fn two_identical_adds_in_one_batch_keep_only_the_first() {
        let ops = vec![
            MemoryOp::Add(new_fact("the venue is the old hall")),
            MemoryOp::Add(new_fact("the venue is the old hall")),
        ];
        assert_eq!(dedup_adds(ops, &[], 0.85).len(), 1);
    }

    #[test]
    fn a_reflection_is_not_deduped_against_a_fact() {
        // Same words, different kind: the reflection says something the fact
        // does not, so it is kept.
        let existing = vec![stored("the team meets on Fridays")];
        let ops = vec![MemoryOp::Reflect(new_fact("the team meets on Fridays"))];
        assert_eq!(dedup_adds(ops, &existing, 0.85).len(), 1);
    }

    #[test]
    fn a_superseded_record_does_not_suppress_a_new_fact() {
        use crate::event::ChatId;
        use crate::memory::store::MemoryStore;
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(
                &chat,
                &[MemoryOp::Add(new_fact("the venue is the old hall"))],
                None,
            )
            .unwrap();
        let id = store.records().unwrap()[0].id.clone();
        store
            .apply(
                &chat,
                &[MemoryOp::Invalidate {
                    target: id,
                    valid_from: Utc::now(),
                }],
                None,
            )
            .unwrap();

        // The old fact is no longer live, so restating it is not a duplicate.
        let ops = vec![MemoryOp::Add(new_fact("the venue is the old hall"))];
        assert_eq!(dedup_adds(ops, &store.records().unwrap(), 0.85).len(), 1);
    }

    #[test]
    fn a_threshold_of_one_drops_only_exact_duplicates() {
        // At the ceiling, only a literal repeat is caught; a paraphrase is not.
        let existing = vec![stored("the venue is the old hall")];
        let repeat = vec![MemoryOp::Add(new_fact("the venue is the old hall"))];
        assert!(dedup_adds(repeat, &existing, 1.0).is_empty());

        let paraphrase = vec![MemoryOp::Add(new_fact("the venue is the new hall"))];
        assert_eq!(dedup_adds(paraphrase, &existing, 1.0).len(), 1);
    }

    #[test]
    fn an_update_passes_through_even_when_it_repeats_content() {
        let existing = vec![stored("the venue is the old hall")];
        let ops = vec![MemoryOp::Update {
            target: existing[0].id.clone(),
            fact: new_fact("the venue is the old hall"),
        }];
        assert_eq!(dedup_adds(ops, &existing, 0.85).len(), 1);
    }
}
