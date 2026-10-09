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
| 5. Tool loop | **done** (`search_memory`, `web_search`) |
| 6. Safety layer (output guard, confirmation gates, injection suite) | **done** |
| 7. Commands + rate limits (the deterministic command router) | **done** (`!ask`, `!summary`, `!memory`, `!forget`, `!remind`) |
| 8. Planner/evaluator | not started |
| 9. Consolidation, reflections, tuning | not started |

### Where things live

- `crates/megumi-agent/` — the platform-agnostic core (lib `megumi_agent`). No
  `whatsapp-rust` dependency. Modules: `event`, `config`, `store`, `queues`,
  `gate`, `context`, `memory/`, `llm/`, `trace`, `agent`.
- `crates/megumi-agent/src/memory/` — durable facts: `store` (records + the JSON
  store), `writer` (extraction), `retrieval` (ranking behind the privacy filter).
- `crates/megumi-agent/src/tools.rs` — the `Tool` trait, the `ToolRegistry`, and
  the `WebSearch` tool.
- `crates/megumi-agent/src/safety.rs` — the output guard, the confirmation gate,
  and the pending-confirmation store.
- `src/agent/mod.rs` — the WhatsApp adapter (`InboundMessage` → `InboundEvent`,
  mention/reply/identity detection, sending actions). `event_from_parts` is the
  shared core both a live message and a command's `MessageContext` go through.
- `src/commands/{ask,summary,memory,forget,remind}/` — the assistant commands
  (the `assistant` group), the deterministic entry points to the agent.
- `src/reminders/` — `!remind`'s store, duration parser, and scheduler loop
  (mirrors `src/news/`).
- `src/events.rs` — the single framework `event_handler`, fanning out to the news
  digest, the reminders, and the agent.
- `crates/megumi-agent/tests/{pipeline,eval,memory,tools,injection,router,live}.rs` —
  end-to-end, eval, memory, tool-loop, prompt-injection, and live smoke tests.

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

## Milestone 5 — the tool loop (done)

Goal: let the model call tools mid-turn, with the loop bounded and every call
traced. Milestone 4's `retrieval::search` is the seam the first tool built on.
Built in `crates/megumi-agent/src/tools.rs` and `agent.rs`:

- `tools.rs` — the `Tool` trait (`spec` + `call`), `ToolRegistry` (name → tool),
  and `WebSearch`, which POSTs to Tavily's `/search` and flattens the results.
  `WebSearch::from_env` returns `None` without `TAVILY_API_KEY`, so an
  unconfigured bot simply has no web search. The base is `TAVILY_API_BASE`
  (default `https://api.tavily.com`).
- `llm/mod.rs` — `LlmRequest` gained `tools` and `messages`; `LlmResponse`
  gained `tool_calls` and `stop_reason`. `LlmMessage` is the three-shape turn
  (text, tool calls, tool results) the Messages API needs, so a call and its
  result round-trip by id. `anthropic.rs` renders them and parses `tool_use`
  blocks and OpenAI `tool_calls` alike.
- `agent.rs::run_turn` — a bounded loop: call the model, run any tool calls,
  append the results, repeat up to `max_tool_iterations`. The last iteration is
  a final chance to answer; its calls are not run, so a model that keeps calling
  tools is cut off rather than looped. Tokens are summed over the calls, and the
  whole loop is one trace.
- `search_memory` is built **per turn** from the turn's `ReaderContext`, not the
  registry, because it must filter through `retrieval::search`'s privacy
  boundary. It is advertised only when `max_tool_iterations > 0`.
- The `TurnTrace` gained `tool_calls: Vec<ToolCallTrace>` (`#[serde(default)]`,
  so an old `traces.json` still loads). Evals: `tests/tools.rs` (a call runs and
  answers, the loop is bounded, a private fact never reaches a group tool result,
  a failing tool still answers, tools off is a single call).

**Do not regress:** the loop is bounded (a tool-calling model cannot spin); a
tool failure is a *result* handed to the model, never a turn failure; and
`search_memory` reads through the same `ReaderContext` filter as the prompt, so
the tool cannot leak a fact the prompt would not. The extraction path is
unchanged: it sends no `tools`, so its request body is byte-identical.

## Milestone 6 — the safety layer (done)

Goal: an output guard, confirmation gates for consequential tool calls, and a
prompt-injection suite. Built in `crates/megumi-agent/src/safety.rs`, with the
gate wired into `agent.rs` and `tools.rs`:

- `safety.rs::screen_reply` — the output guard. The single place a model reply
  becomes sendable: it drops an empty or `NO_REPLY` reply, one carrying the
  prompt's own trust tags (`<chat_message>`, `<memories>`, …), or one reciting a
  ≥40-character sentence of the system prompt, and truncates the rest to
  `max_reply_chars`. The tag and system-prompt checks are the signature of an
  injection that worked; the guard fails closed.
- `tools.rs::Tool::confirmation` — a defaulted trait method returning the
  question to ask before a tool runs, or `None`. A read-only tool
  (`web_search`, `search_memory`) leaves the default; a state-changing tool
  overrides it. The decision is the **tool's**, not the model's.
- `safety.rs::PendingConfirmations` — the held calls, one per chat, in memory
  (like `ChatQueues`), expiring after `confirmation_ttl`. `classify_confirmation`
  reads a message as Yes/No/Unrelated from a small fixed word list.
- `agent.rs` — a consequential call in a tool response is **held, not run**: the
  question goes to the chat, the call is recorded `(awaiting confirmation)`, and
  the turn ends. The next message resolves it *before the gate* (a bare "yes"
  would otherwise be an acknowledgement or a no-trigger), and **only when it is
  directed at the bot** — a mention or a reply, or any private message — so an
  unrelated "ok" in a busy group cannot run a state-changing tool. Yes runs the
  tool and seeds a final narration turn, No replies `Okay, cancelled.`, and any
  other message leaves the call pending for its TTL. `run_turn` takes an optional
  seed, so the confirmation path reuses the same loop, trace, and guard with the
  held call and its result seeded in and no tools advertised.
- Evals: `tests/injection.rs` (an injected tag is escaped in the prompt and in a
  memory; a reply reciting the prompt or echoing the tags is dropped; an
  over-long reply is truncated; an ordinary one passes) and confirmation-gate
  cases in `tests/tools.rs` (held until confirmed, runs on yes with its result
  reaching the model, cancelled on no, dropped by an unrelated message, dropped
  when expired).

**Do not regress:** the guard and the gate are code, never model behaviour, and
both fail closed. A consequential call is never run on the model's word — it runs
in a separate turn, only after a directed yes. A held call is per chat and in
memory, so a restart forgets an unanswered question rather than running a stale
action. The confirmation answer resolves before `gate::decide`, so a private
"ok" is not swallowed as an acknowledgement, and in a group only a message
directed at the bot (mention or reply) can answer it. With no consequential tool
registered and no pending call, the gate and `NO_REPLY` behaviour are exactly as
before.

## Milestone 7 — the command router (done)

Goal: a deterministic entry point to the agent, so its memory and answers are
reachable without relying on the model to decide to speak, plus the framework's
own rate limits on the model-backed commands. Built as an `assistant` command
group in `src/commands/{ask,summary,memory,forget,remind}/`, with two new agent
seams and a reminder subsystem:

- **`!ask <question>`** (`user_cooldown = 5`) — forces an agent turn. A command
  message is `is_command`, so `gate::decide` stays silent on it by design;
  `Agent::answer_command` is the seam that skips the gate and runs the turn
  directly, labelled `Trigger::Command`, mirroring how `confirm_turn` calls
  `run_turn`. The command builds an event from its `MessageContext` (via the
  adapter's `event_from_context`), **keeps the original message id**, overrides
  the text with the question and `is_command = false`, and stores it before the
  turn — so the model sees the question, and the adapter's later ingest of the
  raw `!ask …` is a no-op by id.
- **`!summary`** (`channel_cooldown = 30`) — `Agent::summarize` makes one model
  call shaped like the memory writer's extraction pass (no tools, not a turn),
  screens the reply with the output guard, and stores it with
  `MessageStore::set_summary` — which finally feeds the context builder's
  `<conversation_summary>` layer.
- **`!memory`** / **`!forget <id|all>`** — plain code, no model call.
  `Agent::list_memory` and `Agent::forget_memory` take the per-chat lock and go
  through `ReaderContext`, so a listing never shows another chat's fact and a
  forget cannot reach one. `retrieval::list` filters by `reader.permits` first,
  keeps only this chat's still-true, non-forgotten facts, and returns the full
  `MemoryRecord`s so the ids are shown.
- **`!remind set <duration> <text>`** with `list` and `cancel <id>` — a persisted
  schedule in `src/reminders/`, mirroring `src/news/`: a JSON store, a duration
  parser (`10m`, `1h30m`, `2d`), and a loop started on `Connected` / stopped on
  `Disconnected` with the same reconnect discipline. `!remind` is structured like
  `!group`: a `subcommand_required` parent whose body never runs, so a bare
  `!remind` tells you it needs a subcommand and every child holds its own logic.
- Evals: `crates/megumi-agent/tests/router.rs` (a command forces a turn the gate
  would refuse; the question reaches the prompt; a summary reaches the next
  turn's summary layer; listing and forgetting respect the chat and the reader;
  a forgotten fact is unreachable even for a past-cue question), `tests/routing.rs`
  (the group is registered and the model-backed commands carry cooldowns), and
  `tests/reminders.rs`.

**Do not regress:** a command never answers through `gate::decide` — it forces
its own turn through `answer_command`, which keeps the gate's silence on commands
intact for the trigger path. The command stores its event under the **original
message id** before the turn, so the question is in the window exactly once and
the adapter's ingest deduplicates. Plain commands do no model work and take the
per-chat lock, so they cannot race an extraction pass. **`!forget` is a
never-recall flag, not `Invalidate`:** `Invalidate` closes a window but leaves a
fact answerable as history, and retrieval re-includes superseded facts for
past-cue questions, so `MemoryOp::Forget` sets `MemoryRecord::forgotten` and
retrieval drops forgotten records in both current and past modes. Reminders are
per chat, so a DM cannot list or cancel another chat's.

## First steps in a new session

1. Read `CLAUDE.md` (architecture) and this file (state + next steps).
2. Read `crates/megumi-agent/src/{lib,agent,context,tools,safety,store}.rs` to
   see the seams.
3. Run `cargo test --workspace` to confirm a green baseline.
4. Start milestone 8 (the planner/evaluator) — the next unfinished milestone.
