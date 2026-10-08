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
| 4. Semantic memory (writer, retrieval, validity windows) | not started |
| 5. Tool loop | not started |
| 6. Safety layer (output guard, confirmation gates, injection suite) | not started |
| 7. Commands + rate limits (the deterministic command router) | not started |
| 8. Planner/evaluator | not started |
| 9. Consolidation, reflections, tuning | not started |

### Where things live

- `crates/megumi-agent/` — the platform-agnostic core (lib `megumi_agent`). No
  `whatsapp-rust` dependency. Modules: `event`, `config`, `store`, `queues`,
  `gate`, `context`, `llm/`, `trace`, `agent`.
- `src/agent/mod.rs` — the WhatsApp adapter (`InboundMessage` → `InboundEvent`,
  mention/reply/identity detection, sending actions).
- `src/events.rs` — the single framework `event_handler`, fanning out to the news
  digest and the agent.
- `crates/megumi-agent/tests/{pipeline,eval,live}.rs` — end-to-end, eval, and
  live smoke tests.

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

## Milestone 4 — semantic memory (the next one)

Goal: durable facts with provenance, bi-temporal validity, and hybrid retrieval,
read through the existing `ReaderContext` boundary.

What already exists to build on:

- `context.rs` has `RecalledMemory { content, visibility, origin_chat, subject }`
  and `ReaderContext::visible`, and `ContextBuilder::build` already takes a
  `&[RecalledMemory]` and renders a `<memories>` layer. Today the agent passes an
  empty slice (`agent.rs::run_turn`); wire retrieval in there.
- `store.rs` is the model for a JSON store (atomic write, `:memory:` sentinel,
  open at build time). A memory store should mirror it.

Exit criteria (from the spec): recall and knowledge-update suites pass — a fact
stated hundreds of messages ago is retrieved; after a fact changes, the newest
value is used and the old one is still answerable as history.

Suggested order, each step testable on its own:

1. `MemoryRecord` + a `MemoryStore` (JSON, per the `store.rs` pattern) with the
   bi-temporal fields: `valid_from`/`valid_to` (null = still true), `recorded_at`,
   `superseded_by`, `source_message_ids`, `confidence`, `importance`,
   `visibility`, `origin_chat`, `subject`.
2. A write pipeline (`memory/writer.rs`): collect new messages since the last
   pass, ask the model for `ADD | UPDATE | INVALIDATE | NOOP` operations with
   evidence ids (structured output), validate in code, apply transactionally.
   Trigger: every N messages or after idle (defaults: 25 messages / 10 min).
3. Retrieval (`memory/retrieval.rs`): hybrid (vector + keyword + metadata
   filters), filter by `ReaderContext` **first**, then rank by
   `w1·similarity + w2·recency + w3·importance + w4·confidence`; only
   `valid_to IS NULL` unless the question is about the past. Inject the top 5–8.
4. Wire retrieval into `run_turn` and add a `search_memory` tool seam (the tool
   loop itself is milestone 5).
5. Evals: seeded memory recall, knowledge update, temporal resolution, and a
   canary that a private memory still cannot reach a group.

Embeddings: v1 can brute-force cosine over arrays stored in the JSON; do not add
a vector database until an eval shows it is needed.

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
