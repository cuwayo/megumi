# The AI agent — status and roadmap

This document is the handoff for continuing the agent work. It records what is
built, the decisions that are easy to undo by accident, and the next milestones
in order. It complements `CLAUDE.md` (which describes the architecture) with
*state* and *what comes next*.

The original design spec is a long document describing the whole agent (decision,
memory, context, reasoning, tools, safety, evals). We are implementing it in the
milestones that spec lays out, and its own rule applies: **do not start a later
milestone until the earlier one's exit criteria pass.**

## What is built (milestones 1–3)

| Milestone | Status |
|---|---|
| 1. Skeleton — types, per-chat serialization, message store, trigger gate, single-call reply | **done** |
| 2. Tracing + eval harness | **done** (trace log + deterministic evals) |
| 3. Context builder, mode profiles, `ReaderContext` + visibility labels | **done** |
| 4. Semantic memory (writer, retrieval, validity windows) | **done** |
| 5. Tool loop | not started |
| 6. Safety layer (output guard, confirmation gates, injection suite) | not started |
| 7. Commands + rate limits (the deterministic command router) | not started |
| 8. Planner/evaluator | not started |
| 9. Consolidation, reflections, tuning | not started |

### Where things live

- `crates/megumi-agent/` — the platform-agnostic core (lib `megumi_agent`). No
  `whatsapp-rust` dependency. Modules: `event`, `config`, `store`, `queues`,
  `gate`, `context`, `memory/`, `llm/`, `trace`, `agent`.
- `crates/megumi-agent/src/memory/` — durable facts: `store` (records + the JSON
  store), `writer` (extraction), `retrieval` (ranking behind the privacy filter).
- `src/agent/mod.rs` — the WhatsApp adapter (`InboundMessage` → `InboundEvent`,
  mention/reply/identity detection, sending actions).
- `src/events.rs` — the single framework `event_handler`, fanning out to the news
  digest and the agent.
- `crates/megumi-agent/tests/{pipeline,eval,memory,live}.rs` — end-to-end, eval,
  memory, and live smoke tests.

## Decisions that must not regress

These were chosen deliberately; a later change that breaks one is a regression.

1. **The agent stores every message in every chat, always** — even when it will
   not reply. Storage is inline and unlocked (a slow turn must not delay it);
   turns are serialized per chat by `ChatQueues`. `ingest` runs before `respond`.
2. **Groups need an explicit trigger; private chats answer everything** except a
   bare acknowledgement. Commands belong to the command layer, so the agent stays
   silent on them even when they mention the bot. This is `gate::decide`.
3. **Privacy is enforced in code, at the query layer, fail-closed.**
   `ReaderContext::permits` matches `Visibility` exhaustively, so a new label
   forces a decision. A private memory never reaches a group; a group memory
   never crosses to another group. There are canary tests for this in
   `context.rs` — keep them green.
4. **Identity and mention detection compare PN *and* LID** with
   `JidExt::is_same_chat_as`, never `is_same_user_as` (which ignores the server).
   `context_info` is read from whichever base sub-message carries it (a caption
   can hold a mention).
5. **Do not combine the inline agent turn with `EventDelivery::Ordered`.** The
   default `Concurrent` spawns each event on its own task; `Ordered` would
   head-of-line-block every event. The agent assumes `Concurrent`.
6. **The model endpoint is configurable and the client is provider-tolerant.**
   `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com`; a gateway URL may
   or may not include `/v1`), `ANTHROPIC_AUTH_TOKEN` sent as `Bearer` with
   `ANTHROPIC_API_KEY` as a fallback and both headers always sent,
   `ANTHROPIC_MODEL` (default `claude-sonnet-5-5`). The request is the Anthropic
   Messages shape; the response parser accepts both Anthropic (`content`) and
   OpenAI (`choices`) shapes, because a gateway may answer the Anthropic route in
   the OpenAI shape. A `<ds_safety>` gateway annotation is stripped from replies.
7. **Config, not code.** Budgets, windows, caps, and paths live in `AgentConfig`,
   env-overridable. Add new thresholds there, not inline.
8. **A memory fact's privilege and time are set in code, never by the model.**
   The extraction writer derives `visibility` from the chat type and `valid_from`
   from the evidence messages, and drops any op whose evidence ids are not in the
   batch. A model proposes facts; it cannot widen one's reach or invent a source.
9. **Retrieval filters by `ReaderContext` before it ranks.** A fact the reader
   may not see is never scored, so the private-to-group leak cannot happen
   upstream of the filter.

## How to run and verify

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check          # nightly rustfmt; CI pins it

# the agent crate alone
cargo test -p megumi-agent

# the live smoke test (needs a credential + network); reads .env
cargo test -p megumi-agent --test live -- --ignored --nocapture
```

The three reasoning evals in `crates/megumi-agent/tests/eval.rs` are `#[ignore]`d
pending a live model; the rest run with `ScriptedLlm` and need no network.

## Milestone 4 — semantic memory (done)

Goal: durable facts with provenance, bi-temporal validity, and retrieval read
through the existing `ReaderContext` boundary. Built in `crates/megumi-agent/src/memory/`:

- `store.rs` — `MemoryRecord` (bi-temporal: `valid_from`/`valid_to`,
  `recorded_at`, `superseded_by`, `source_message_ids`, `confidence`,
  `importance`, `visibility`, `origin_chat`, `subject`), `MemoryOp`
  (`Add`/`Update`/`Invalidate`/`Noop`), and `MemoryStore` (one JSON file, atomic
  write, `:memory:` sentinel, mirroring `store.rs`). `apply` is the only mutator
  and takes a whole batch plus the new cursor, so a pass is one write.
- `writer.rs` — `extract_if_due`: collects the messages since the chat's cursor,
  asks the model for a JSON array of ops with evidence ids, validates each in
  code, and applies the batch. Trigger: 25 new messages, or a 10-minute idle
  backlog. The cursor is a message id; a transport failure leaves it unadvanced
  (retried), an unparseable reply advances it (not re-run).
- `retrieval.rs` — `search`: filter by `reader.permits` **first**, then rank
  `0.5·similarity + 0.2·recency + 0.2·importance + 0.1·confidence`, only
  still-true facts unless the question reads as being about the past, top
  `memory_recall_top` (8).
- `agent.rs` runs `extract_if_due` in `respond` before the gate (so a silent,
  busy group still extracts before the window prunes), and `run_turn` passes
  `search(...)` to the context builder instead of an empty slice.
- Evals: `crates/megumi-agent/tests/memory.rs` (recall, knowledge update with
  history, private canary, cursor durability), plus unit tests in each module.

**v1 stands in for embeddings with lexical similarity.** The configured provider
(Anthropic Messages API) has no embeddings endpoint, so `retrieval::similarity`
compares words; replace that one function with a vector score when an eval needs
it, and do not add a vector database before then.

**Do not regress:** the model never chooses `visibility` (the writer derives it
from the chat type), and every fact must cite evidence message ids that are in
the batch, so a model cannot widen a fact's reach or invent a source. Extraction
runs under the per-chat turn lock, and a failed pass is logged, never fatal.

## Milestone 5 — the tool loop (the next one)

Goal: let the model call tools mid-turn (a `search_memory` tool over
`retrieval::search`, web/command tools), with the loop bounded and every call
traced. Milestone 4's `retrieval::search` is the seam the first tool builds on.

## Milestone 7 — the command router (note)

The agent has **no `!` command today**; it is trigger-based (mention, reply,
DM). The spec's `/ask`, `/summary`, `/remind`, `/memory`, `/forget`, `/help`
commands are milestone 7, after memory and tools exist. If a command is wanted
sooner, the pattern is: a `#[command]` that builds an `InboundEvent` from the
command message and calls `ctx.data().agent.respond(...)`, then sends the result.
Keep simple commands (`/help`, `/memory`, `/forget`) as plain code with no model
call.

## First steps in a new session

1. Read `CLAUDE.md` (architecture) and this file (state + next steps).
2. Read `crates/megumi-agent/src/{lib,agent,context,store}.rs` to see the seams.
3. Run `cargo test --workspace` to confirm a green baseline.
4. Start milestone 4 at step 1 above.
