# The AI agent — status and roadmap

This document is the handoff for continuing the agent work. It records what is
built, the decisions that are easy to undo by accident, and the next milestones
in order. It complements `CLAUDE.md` (which describes the architecture) with
*state* and *what comes next*.

The original design spec is a long document describing the whole agent (decision,
memory, context, reasoning, tools, safety, evals). We are implementing it in the
milestones that spec lays out, and its own rule applies: **do not start a later
milestone until the earlier one's exit criteria pass.**

## What is built (milestones 1–13)

| Milestone | Status |
|---|---|
| 1. Skeleton — types, per-chat serialization, message store, trigger gate, single-call reply | **done** |
| 2. Tracing + eval harness | **done** (trace log + deterministic evals) |
| 3. Context builder, mode profiles, `ReaderContext` + visibility labels | **done** |
| 4. Semantic memory (writer, retrieval, validity windows) | **done** |
| 5. Tool loop | **done** (`search_memory`, `web_search`) |
| 6. Safety layer (output guard, confirmation gates, injection suite) | **done** |
| 7. Commands + rate limits (the deterministic command router) | **done** (`!ask`, `!summary`, `!memory`, `!forget`, `!remind`) |
| 8. Planner/evaluator | **done** (plan before, evaluate + bounded revise after) |
| 9. Consolidation, reflections, tuning | **done** (reflections, dedup, tunable weights) |
| 10. Media understanding (voice transcription + image description) | **done** (adapter-side, OpenAI-compatible, optional) |
| 11. Tool context + the first consequential tool | **done** (`ToolContext`; `search_memory` as a real tool; `set_reminder` behind the confirmation gate) |
| 12. Grounding — the agent gets a clock | **done** (a `<now>` prompt layer from the trigger's timestamp; guard drops an echoed one) |
| 13. History search — the model can reach past the prompt window | **done** (`search_history` over the message store, scoped to the turn's chat) |
| 14. Reminder management — the model can see and cancel reminders | **done** (`list_reminders` + `cancel_reminder` over the bot's reminder store, the second behind the confirmation gate) |

### Where things live

- `crates/megumi-agent/` — the platform-agnostic core (lib `megumi_agent`). No
  `whatsapp-rust` dependency. Modules: `event`, `config`, `store`, `queues`,
  `gate`, `context`, `memory/`, `llm/`, `trace`, `agent`.
- `crates/megumi-agent/src/memory/` — durable facts: `store` (records + the JSON
  store), `writer` (extraction), `reflection` (consolidation into insights),
  `consolidate` (duplicate suppression), `retrieval` (ranking behind the privacy
  filter).
- `crates/megumi-agent/src/tools.rs` — the `Tool` trait, `ToolContext`, the
  `ToolRegistry`, and the `SearchMemory`, `SearchHistory`, and `WebSearch` tools.
- `crates/megumi-agent/src/safety.rs` — the output guard, the confirmation gate,
  and the pending-confirmation store.
- `crates/megumi-agent/src/reasoning.rs` — the planner and the evaluator, the two
  passes that bracket a turn.
- `src/agent/mod.rs` — the WhatsApp adapter (`InboundMessage` → `InboundEvent`,
  mention/reply/identity detection, sending actions). `event_from_parts` is the
  shared core both a live message and a command's `MessageContext` go through.
- `src/agent/media.rs` — the optional OpenAI-compatible media provider the
  adapter transcribes voice notes and describes images with (milestone 10).
- `src/agent/tools.rs` — the bot-side tools that need one of the bot's stores,
  all over the reminder store: `SetReminder` schedules into it (milestone 11),
  `ListReminders` reads it, and `CancelReminder` writes to it behind the same
  confirmation gate (milestone 14).
- `src/commands/{ask,summary,memory,forget,remind}/` — the assistant commands
  (the `assistant` group), the deterministic entry points to the agent.
- `src/reminders/` — `!remind`'s store, duration parser, and scheduler loop
  (mirrors `src/news/`).
- `src/events.rs` — the single framework `event_handler`, fanning out to the news
  digest, the reminders, and the agent.
- `crates/megumi-agent/tests/{pipeline,eval,memory,tools,history,injection,router,reasoning,reflection,media,live}.rs` —
  end-to-end, eval, memory, tool-loop, history-search, prompt-injection,
  reasoning, reflection, media, and live smoke tests. The bot-side reminder
  tools' flows are in the bot crate's `tests/agent.rs`.

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
10. **Media is described by the adapter, and `Attachment::description` carries
    only the provider's output.** The agent core never fetches or decodes media;
    the adapter is the one place with both the bytes and the client. A caption is
    not copied into `description` — it already rides in the message text — so
    without a provider a message renders exactly as before media understanding
    existed. The adapter reads only the message's *own* media
    (`Attachment::own`), never a quoted message's, so a text reply to an image
    does not get the image's description attributed to it.
11. **A tool learns which turn it is in from the `ToolContext`, never from global
    state.** The registry is shared across every chat, so a tool that acts on the
    conversation reads the chat and the reader from the context the agent hands
    `Tool::call`. A confirmation turn is built with `ToolContext::for_event`, so a
    held call runs against the chat it was held in. The `search_memory` tool reads
    memory through that context's reader, so the privacy boundary holds on the
    tool path exactly as it does on the prompt path.

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
it, and do not add a vector database before then. The four ranking weights are no
longer constants — milestone 9 moved them into `AgentConfig` (see below).

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

## Milestone 8 — the planner/evaluator (done)

Goal: a reasoning pass before a turn and a critique after it, both optional and
both failing open. Built in `crates/megumi-agent/src/reasoning.rs`, with the
passes wired into `agent.rs::run_turn`:

- `reasoning.rs::plan` — one model call before the turn, returning a short list
  of steps that the context builder renders as a `<plan>` layer. It runs only
  when `planner_enabled` is set **and** the request has at least `plan_min_words`
  words, so a greeting is not planned. The prompt is shaped like the memory
  writer's extraction prompt (a stable instruction plus the trust-tagged,
  escaped request); the reply is parsed with the same fence-strip + bracket-slice
  robustness as `writer::parse_ops`, now shared as `context::strip_code_fence`.
- `reasoning.rs::evaluate` — one model call after a draft exists, returning
  `{"verdict":"ACCEPT"|"REVISE","reason":"..."}`. A verdict that cannot be read
  is `None`, which the caller treats as **ACCEPT**: the evaluator is a quality
  gate, not a safety gate, so it can only ever improve a reply, never suppress
  one.
- `reasoning.rs::revision_seed` — on `REVISE`, the caller runs one more turn
  through the same `model_loop`, seeded with the draft as an assistant turn and
  the critique as the next user turn, so the model improves the reply. Revisions
  are bounded by `max_revisions` (default 1; 0 disables the evaluator), so a
  model that keeps asking for changes is cut off rather than looped.
- `agent.rs::run_turn` now calls `model_loop` (the extracted bounded tool loop)
  for the ordinary turn and again for a revision. Planning and evaluation run on
  the ordinary path only — a `Trigger::Confirmation` narration turn is neither
  planned nor judged, and a reply that is silent (`NO_REPLY`/empty) is not
  evaluated. The output guard still runs last on whatever reply wins.
- `TurnTrace` gained `plan`, `revisions`, and `verdict` (`#[serde(default)]`, so
  an old `traces.json` still loads), and `safety::INTERNAL_TAGS` gained `<plan>`
  so a reply echoing the new scaffolding is dropped.
- Config knobs (env-overridable, defaults ON, `for_test` OFF):
  `planner_enabled` (`AGENT_PLANNING_ENABLED`), `plan_min_words`
  (`AGENT_PLAN_MIN_WORDS`, default 12), `plan_max_tokens` (`AGENT_PLAN_TOKENS`),
  `max_revisions` (`AGENT_MAX_REVISIONS`, default 1), `evaluator_max_tokens`
  (`AGENT_EVAL_TOKENS`).
- Evals: `crates/megumi-agent/tests/reasoning.rs` (a non-trivial request is
  planned and the plan reaches the prompt; a short request is not planned; an
  ACCEPT sends the draft unchanged; a REVISE rewrites it and the critique reaches
  the revision call; a malformed verdict fails open; a `NO_REPLY` is never
  evaluated; revisions are bounded; a reply echoing `<plan>` is dropped; the plan
  and verdict calls read no memory and advertise no tools), plus unit tests in
  `reasoning.rs` for the plan/verdict parsers.

**Do not regress:** the two passes fail **open** — a plan or a verdict that
cannot be read changes nothing, so reasoning can never suppress a reply the agent
would otherwise send. Planning and evaluation run only on the ordinary path, so
the confirmation narration turn is untouched, and the output guard still runs
last. The plan and verdict calls read no memory and take no `ReaderContext`, so
there is no new privacy surface; the revision reuses the turn's own prompt and so
carries exactly what the main turn already showed the same reader. The planner
and evaluator must stay off in `AgentConfig::for_test()` — the tool and eval
tests assert exact request counts, and a stray plan or verdict call would break
them.

## Milestone 9 — consolidation, reflections, tuning (done)

Goal: let a chat's facts add up to higher-level knowledge, stop near-duplicate
facts accumulating, and move the retrieval weights out of code. Built in
`crates/megumi-agent/src/memory/{reflection,consolidate}.rs`, with the pass wired
into `agent.rs` and the store and config extended:

- `memory/store.rs` — `MemoryKind::{Fact, Reflection}` (externally tagged, no
  fallback, defaulting to `Fact` so an old memory file still loads) and a
  `MemoryRecord::kind`. `MemoryOp::Reflect(NewFact)` stores a reflection; its
  `source_message_ids` name the **fact ids** it was derived from. `State` gained a
  per-chat `reflection_cursor`, and `apply` was split into a private `apply_with`
  so `apply` (extraction) and the new `apply_reflections` each move only their own
  cursor, in one write.
- `memory/reflection.rs::reflect_if_due` — one model call when a chat has
  `reflection_batch` (10) new live facts. It shows the chat's live facts and asks
  for `ADD`/`NOOP` insights; `validate_reflection` derives visibility from the
  chat type and `valid_from` from the cited facts, and drops an op citing a fact
  the chat does not have. The reply is parsed with the writer's shared
  `parse_ops`. The cursor is the newest live fact id, advanced on any model answer
  (including an unparseable one) but not on a transport failure, so a pass that
  stores nothing does not re-fire every turn.
- `memory/consolidate.rs::dedup_adds` — a pure lexical near-duplicate check
  (Jaccard over lowercased word tokens, threshold `dedup_threshold`, default
  0.85). It runs in the writer (before `apply`) and in the reflection pass, and
  only ever drops an `Add`/`Reflect` against a live record **of the same kind**,
  so a reflection paraphrasing a fact is kept. It is deliberately *not* in
  `MemoryStore::apply`: dedup is a config-tuned heuristic and `apply` must stay a
  predictable op-applier.
- `memory/retrieval.rs` — the four ranking weights moved from `const`s into
  `AgentConfig` (`weight_similarity` 0.5, `weight_recency` 0.2,
  `weight_importance` 0.2, `weight_confidence` 0.1; env-overridable). This is the
  "tuning" the milestone names.
- `context.rs` — `RecalledMemory` gained `kind`, and `render_memory` marks a
  reflection `(insight) ` so the model reads it as the agent's own inference, not
  a stated fact. The `search_memory` tool renders it the same way.
- Config knobs (env-overridable, defaults ON, `for_test` OFF):
  `reflection_enabled` (`AGENT_REFLECTION_ENABLED`), `reflection_batch`
  (`AGENT_REFLECTION_BATCH`, 10), `reflection_max_facts`
  (`AGENT_REFLECTION_MAX_FACTS`, 50), `reflection_max_tokens`
  (`AGENT_REFLECTION_TOKENS`), `dedup_threshold` (`AGENT_DEDUP_THRESHOLD`, 0.85),
  and `AGENT_WEIGHT_{SIMILARITY,RECENCY,IMPORTANCE,CONFIDENCE}`. A new `env_f32`
  helper parses the fractional settings without rounding them to integers.
- `agent.rs::reflect_memory` runs right after `extract_memory` in `respond` and
  `answer_command`, so a chat's new facts are reflected on with its older ones. A
  failed pass is logged, never fatal.
- Evals: `crates/megumi-agent/tests/reflection.rs` (enough facts store a
  reflection citing them; an op citing an unknown fact is dropped; a private
  reflection never reaches a group prompt; a duplicate the extraction proposes is
  not stored twice; reflections off makes no call; a NOOP advances the cursor so
  the pass does not re-fire), plus unit tests in `reflection.rs`,
  `consolidate.rs`, `retrieval.rs`, `store.rs`, and `context.rs`.

**Do not regress:** a reflection is a distinct kind, never a plain fact, and it is
marked `(insight)` in the prompt so an inference is not read as ground truth. The
model never sets a reflection's privilege or time — visibility comes from the chat
type and `valid_from` from the cited facts, and evidence must cite live facts of
this chat — so there is no new privacy surface and a private reflection cannot
reach a group (retrieval still runs `reader.permits` first). Dedup lives in the
config-aware pass, never in `apply`. The pass must stay off in
`AgentConfig::for_test()`: the tool, reasoning, and router tests assert exact
request counts, and a stray reflection call would break them.

## Milestone 10 — media understanding (done)

Goal: turn a voice note or an image into text so the model — and memory
extraction and `!summary` — can use it. Built in `src/agent/media.rs` (the
provider) and `src/agent/mod.rs` (the adapter), with the agent core persisting
and rendering the result:

- `src/agent/media.rs` — `OpenAiMedia`, an OpenAI-compatible provider.
  `from_env` reads `MEDIA_API_KEY` then `OPENAI_API_KEY` and returns `None`
  without one, so an unconfigured bot has no media understanding.
  `transcribe` POSTs a multipart body to `{base}/audio/transcriptions`;
  `describe_image` POSTs a base64 data URI to `{base}/chat/completions`. Both cap
  their reply (`MEDIA_MAX_CHARS`), and `max_bytes` guards the download. It is a
  concrete type in the bot crate, not a trait in `megumi-agent`: its only caller
  is the adapter, so a core abstraction would be a module the pipeline never
  calls.
- `src/agent/mod.rs::attachments` — reads the message's **own** media
  (`megumi::Attachment::own`, not `from_message`, so a quoted image is not
  attributed to the reply quoting it), refuses anything over `max_bytes`, then
  downloads via `client.download` and calls the provider. The result is the
  attachment's `description`; any failure logs and leaves it `None`. Only audio
  and images are sent — video, documents, and stickers keep their kind alone.
- `megumi-agent` core — `StoredMessage` gained `#[serde(default)] attachments`
  (`Attachment` gained `PartialEq, Eq` to hold it), `Agent::ingest` copies them,
  and `context::attachment_note` renders a described attachment as an escaped
  `[kind: …]` prefix inside the message's own `<chat_message>` line. That helper
  is reused by the extraction prompt and `!summary`, so media can become a fact
  or a summary too. The note is a plain prefix, not a tag, so `INTERNAL_TAGS` and
  the system prompt are unchanged.
- `gate.rs` — a private message whose only content is a **described** attachment
  is answered rather than swallowed as an acknowledgement; an undescribed one
  stays silent, as before.
- Evals: `crates/megumi-agent/tests/media.rs` (a description reaches the prompt;
  a described attachment makes a text-less DM answerable and can become a fact;
  an undescribed one stays silent and costs no call; a description cannot close a
  trust tag), plus unit tests in `media.rs`, `context.rs`, and `gate.rs`.

**Do not regress:** the provider is optional and the no-key path is byte-identical
to the pre-milestone behaviour. `Attachment::description` is provider output
only — never the caption, which already rides in the message text — so nothing is
duplicated and a provider failure degrades to the kind alone. The adapter reads
only the message's own media. The description is untrusted, so it is escaped like
any other text and rendered as a plain prefix, not a trust tag.

## Milestone 11 — the tool context and the first consequential tool (done)

Goal: let a tool act on the conversation it was called in, and finally exercise
the milestone-6 confirmation gate with a real tool. Before this, the registry was
shared across every chat but a tool had no way to know which chat it was in — the
one reader-dependent tool (`search_memory`) was special-cased inside the agent —
and the only production tool, `web_search`, was read-only, so the hold-and-confirm
machinery had never run outside a test double. Built in
`crates/megumi-agent/src/tools.rs` and `src/agent/tools.rs`:

- **`ToolContext`** (`tools.rs`) wraps the turn's `ReaderContext`, exposing
  `chat()`, `chat_type()`, `sender()`, and `reader()`.
  It is built fresh per turn in `Agent::model_loop` and handed to every
  `Tool::call`, so the registry stays shared while a tool still knows the chat
  and the reader. A confirmation turn builds the same context
  (`ToolContext::for_event`), so a held call runs against the chat it was held in.
- **`Tool::call` gained the context parameter.** Every tool now takes
  `(&ToolContext, &serde_json::Value)`.
- **`SearchMemory` is a normal registry tool** holding `Arc<MemoryStore>` +
  `AgentConfig`. It reads through `retrieval::search` with the turn's reader, so
  the privacy boundary holds on the tool path exactly as it does on the prompt
  path. The agent's `search_memory` special case — the `SEARCH_MEMORY` constant in
  `agent.rs`, `memory_tool_spec`, `Agent::search_memory`, and the branch in
  `confirmation_for` — is gone; the tool is registered like any other.
- **`SetReminder`** (`src/agent/tools.rs`) holds `Arc<ReminderStore>` and, on a
  call, parses the model's `delay` with the reminder parser and schedules the
  reminder **in the turn's chat** (`context.chat()`). It overrides
  `Tool::confirmation`, so the agent holds the call and runs it only after the
  user's "yes" — the milestone-6 gate, now driven by a real tool. `build_agent`
  registers `SearchMemory` and `SetReminder` (and `web_search` when
  `TAVILY_API_KEY` is set).
- Evals: `crates/megumi-agent/tests/tools.rs` (the memory search reads through the
  turn context and cannot cross the privacy boundary; a confirmed call runs in the
  turn's chat) and `tests/agent.rs` in the bot crate (a `set_reminder` call is held
  — the store stays empty — and the user's "yes" writes the reminder into the bot's
  store, traced as a confirmation turn), plus unit tests in `tools.rs` and
  `src/agent/tools.rs`.

**Do not regress:** a tool reads the chat and the reader from the `ToolContext`,
never from global state, so the registry stays safe to share across chats. A
confirmation turn is built with `ToolContext::for_event`, so a held call runs
against the chat it was held in, not whichever message answered it. `SearchMemory`
still reads through `retrieval::search`, so the private-to-group boundary holds on
the tool path. `set_reminder` is the tool that owns the confirmation decision —
the model cannot talk past it — and it schedules only into the turn's chat.

## Milestone 12 — grounding, the agent gets a clock (done)

Goal: give the model the current time so relative words in a message ("today",
"tomorrow", "in an hour", "next Friday") resolve against something concrete
instead of being guessed. Built in `crates/megumi-agent/src/context.rs`, with a
one-line addition to the output guard:

- `context.rs::render_now` — a `<now>` layer at the **head** of the prompt body,
  so it frames every layer below it, message timestamps included. The instant is
  the **trigger message's own `timestamp`**, not `Utc::now()`: a turn is about
  the message that prompted it, and anchoring to that message keeps a replayed or
  re-graded transcript deterministic (and makes the eval a fixed assertion rather
  than a moving one). It is rendered in the process's **local** timezone with the
  offset and weekday (`2026-10-09T15:30:00+08:00 (Friday)`), because "tomorrow"
  is a local-calendar question.
- `context.rs::system_prompt` — a stable sentence tells the model what `<now>` is
  for and that message timestamps are given alongside each message. The sentence
  is part of the byte-identical system prefix, so the prompt stays cacheable.
- `safety.rs::INTERNAL_TAGS` — gained `<now`/`</now`, so a reply that echoes the
  clock scaffolding is dropped like any other internal tag (the milestone-6
  guard, unchanged in spirit).
- Evals: a `the_current_time_reaches_the_context` case in `tests/eval.rs` (the
  trigger's local instant reaches the prompt and the system prompt explains the
  tag), plus unit tests in `context.rs` (the layer heads the body, carries the
  trigger's instant, offset, and weekday) and `safety.rs` (an echoed `<now>` is
  suppressed).

**Do not regress:** the clock is the **trigger's timestamp**, not the wall clock,
so a stored or replayed message renders the same prompt. It is a plain `<now>`
value, not a trust tag, and it is escaped/rendered like the rest of the
scaffolding — the model reads it as data. The system prompt stays byte-identical
across turns (the `<now>` value lives in the volatile user body, never in the
system prompt), so the cacheable prefix is unchanged. The planner, the memory
writer, and `!summary` do **not** get a clock: they do not resolve relative dates
(the writer stamps `valid_from` in code from the evidence messages), so adding
one there would be surface without a need.

## Milestone 13 — history search (done)

Goal: let the model reach a message older than the prompt's window. The store
keeps `max_stored_messages` (200) per chat but the prompt carries only
`group_window`/`private_window` (30/40), so older messages are durable yet
invisible. Built in `crates/megumi-agent/src/tools.rs`, on the `SearchMemory`
template:

- **`SearchHistory`** holds `Arc<MessageStore>` + `AgentConfig` and registers as
  `search_history`. On a call it reads `store.recent(context.chat(),
  max_stored_messages)` — the whole stored window, not the prompt's smaller one —
  scores each message with `retrieval::similarity` (promoted to `pub(crate)`, the
  same lexical v1 stand-in facts use), over a haystack of the message's text plus
  its described-attachment note, keeps only `score > 0`, sorts best-first (ties
  newest-first, then id), takes `history_search_results` (8), and renders them as
  plain escaped lines capped at `history_search_max_chars` (4000). Read-only, so
  it leaves `Tool::confirmation` at its `None` default.
- **Rendering is untagged**, unlike the prompt's own `render_message`: a tool
  result carrying the prompt's internal `<chat_message>` tags risks the model
  echoing them, which the output guard would then drop. Each line is
  `` - {name} ({timestamp}): {text} `` with every field escaped, mirroring how
  `render_memory` feeds untagged lines to the `search_memory` tool channel.
- **Scoping is structural.** The message store is keyed by `ChatId` and the tool
  reads only `context.chat()`, so a group turn cannot reach a private chat's or
  another group's messages — the same boundary the prompt has, and decision #11
  (a tool learns its turn from the `ToolContext`). A canary test locks it in.
- **Config knobs** (env-overridable): `history_search_results`
  (`AGENT_HISTORY_SEARCH_RESULTS`, 8), `history_search_max_chars`
  (`AGENT_HISTORY_SEARCH_MAX_CHARS`, 4000). `for_test` is unchanged — it keeps
  `max_tool_iterations = 0`, so no exact-request-count test is disturbed.
- Evals: `crates/megumi-agent/tests/history.rs` (a message outside the prompt
  window is absent from the prompt but reachable through the tool and its content
  reaches the model; a private chat's message is not reachable from a group
  turn), plus unit tests in `tools.rs` (matching, no-match, missing query, cap and
  order, attachment-description match, chat scoping, truncation).

**Do not regress:** the tool reads only `context.chat()` from the store, so the
per-chat scoping the prompt has holds on the tool path — a group turn cannot read
a DM's or another group's history. It is read-only, so it needs no confirmation
and adds no state-changing surface. Its results are untagged plain lines, so a
tool result cannot smuggle the prompt's internal tags into a reply the output
guard would then drop.

## Milestone 14 — reminder management (done)

Goal: let the model *manage* the reminders it can already create. Milestone 11
gave it `set_reminder`, but a reminder it set was then invisible to it — "what
did I ask you to remind me about?" and "cancel that" both failed, because the
model had no way to read or remove one. Built in `src/agent/tools.rs`, on the
`SetReminder` template:

- **`ListReminders`** (`list_reminders`) — read-only, no arguments. On a call it
  reads `store.list(context.chat())` and renders the chat's reminders as plain
  escaped lines, each `` - {short id}: {text} (due {rfc3339}) ``, or
  `No reminders are set in this chat.` when there are none. It leaves
  `Tool::confirmation` at its `None` default.
- **`CancelReminder`** (`cancel_reminder`) — state-changing, so it overrides
  `Tool::confirmation` and the agent **holds** the call until the user agrees,
  exactly as `set_reminder` does. On a call it removes the reminder whose id
  starts with the model's `id`, **in the turn's chat** (`store.cancel(context.chat(), …)`);
  an unknown id or an ambiguous prefix is an error result handed back to the
  model, not a failed turn.
- **Rendering is untagged and escaped**, like every tool result: the reminder's
  text is the *user's* words, so it could carry a trust tag; `escape` (promoted
  to `pub` in `context.rs`, so a bot-side tool escapes exactly as the prompt
  does) renders it inert, and the line carries no internal tag the model could
  echo into a reply the output guard would drop. The id is the same eight-character
  short form `!remind` shows, so the model and the user name a reminder the same way.
- `build_agent` registers both tools over the same `ReminderStore` it already
  gives `SetReminder`.
- Evals: the two new flows through the production wiring in `tests/agent.rs` (the
  list tool reads the turn's own chat and its result reaches the model; a cancel
  call is held — the reminder survives — and the user's "yes" removes it, traced
  as a confirmation turn), plus unit tests in `src/agent/tools.rs` (the specs,
  the confirmation questions, chat scoping, the empty case, escaping, an unknown
  id, and a missing id).

**Do not regress:** both tools read and write only `context.chat()`, so the
per-chat scoping the store enforces holds on the tool path — a group turn cannot
see or cancel a private chat's or another group's reminders. `cancel_reminder` is
state-changing, so it is **never** run on the model's word: the agent holds the
call and runs it in a separate turn only after a directed yes, the same gate
`set_reminder` uses. A tool result is untagged plain lines with the user's text
escaped, so it cannot smuggle a trust tag into a reply.

## First steps in a new session

1. Read `CLAUDE.md` (architecture) and this file (state + next steps).
2. Read `crates/megumi-agent/src/{lib,agent,context,tools,safety,reasoning,store}.rs`
   to see the seams.
3. Run `cargo test --workspace` to confirm a green baseline.
4. Milestones 1–14 are done. The original design spec is not in the repo, so any
   further work is a new interpretation: add more bot-side tools (the reminder
   tools are the template — a store the bot owns, a read-only tool, and a
   `confirmation` on each that changes state), extend the evals, tune the
   reflection, retrieval, and media knobs against real traffic, or add
   video/document understanding to the adapter's `media` seam. A tool over the
   *news* store (subscribe/unsubscribe from a turn) would mirror this milestone,
   but the news subscription is a group-admin setting today, so it would need a
   permission check the agent's `ToolContext` does not yet carry.
