//! Durable facts: writing them, and reading them back.
//!
//! The message store remembers what a chat *said*; this remembers what it
//! *means* — the facts worth keeping after the transcript has scrolled past the
//! window. A fact is a [`MemoryRecord`] with provenance (the messages it came
//! from), bi-temporal validity (when it was true, and whether it still is), and
//! a [`Visibility`](crate::context::Visibility) label.
//!
//! The three pieces are separate so each is testable on its own:
//!
//! - `store` - [`MemoryRecord`], [`MemoryOp`], and the [`MemoryStore`]
//! - `writer` - extracting new facts from a chat's messages with the model
//! - `retrieval` - ranking the facts a reader may see for a question
//!
//! Retrieval filters through the same [`ReaderContext`](crate::context::ReaderContext)
//! boundary the context builder uses, so a private fact cannot reach a group
//! through memory any more than through the message window.

pub mod retrieval;
pub mod store;
pub mod writer;

pub use store::{MemoryOp, MemoryRecord, MemoryStore};
