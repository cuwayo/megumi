//! The turn pipeline: store, decide, build context, ask the model, record.
//!
//! [`Agent::ingest`] stores one message; [`Agent::respond`] runs a turn for a
//! message and returns what to send, if anything. They are separate on purpose:
//! storing must never wait on — or fail because of — a model call, so the
//! adapter stores every message first and only then runs the turns that a
//! trigger calls for. A turn is serialized per chat through [`ChatQueues`].

use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, warn};

use crate::config::AgentConfig;
use crate::context::{ContextBuilder, Prompt, ReaderContext, attachment_note, escape, truncate};
use crate::event::{ChatId, ChatType, InboundEvent, OutboundAction};
use crate::gate::{self, GateDecision, Trigger};
use crate::llm::{LlmClient, LlmError, LlmMessage, LlmRequest, LlmToolCall, LlmToolResult};
use crate::memory::store::{MemoryOp, MemoryRecord};
use crate::memory::{MemoryStore, reflection, retrieval, writer};
use crate::queues::ChatQueues;
use crate::reasoning::{self, Usage};
use crate::safety::{self, Confirmation, PendingConfirmations, PendingToolCall};
use crate::store::{MessageStore, StoredMessage};
use crate::tools::{ToolContext, ToolRegistry};
use crate::trace::{self, ToolCallTrace, TraceSink, TurnTrace};

/// The reply a code path sends when the user declines a held tool call.
const CANCELLED: &str = "Okay, cancelled.";

/// The line appended after a tool's confirmation question.
const CONFIRM_HINT: &str = "Reply \"yes\" to confirm.";

/// The result recorded in the trace for a call held for confirmation.
const AWAITING_CONFIRMATION: &str = "(awaiting confirmation)";

/// The agent: the store it reads and writes, the model it asks, and the queues
/// that keep a chat's turns in order.
pub struct Agent {
    store: Arc<MessageStore>,
    memory: Arc<MemoryStore>,
    traces: Arc<TraceSink>,
    llm: Arc<dyn LlmClient>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    queues: ChatQueues,
    /// The state-changing tool calls held for a user's confirmation, one per
    /// chat. In memory and per chat, so a restart forgets an unanswered
    /// question rather than running a stale action.
    confirmations: PendingConfirmations,
}

impl Agent {
    /// Builds an agent over an existing store, memory, trace log, model, and
    /// tool registry.
    pub fn new(
        store: Arc<MessageStore>,
        memory: Arc<MemoryStore>,
        traces: Arc<TraceSink>,
        llm: Arc<dyn LlmClient>,
        tools: Arc<ToolRegistry>,
        config: AgentConfig,
    ) -> Self {
        Self {
            store,
            memory,
            traces,
            llm,
            tools,
            config,
            queues: ChatQueues::new(),
            confirmations: PendingConfirmations::new(),
        }
    }

    /// Stores one message for `event`'s chat.
    ///
    /// Returns whether the message was new. Every message is stored, in every
    /// chat, whether or not the agent answers — that history is what context
    /// and, later, memory are built from. A duplicate id is a no-op, so
    /// redelivery never stores twice.
    pub fn ingest(&self, event: &InboundEvent) -> Result<bool, crate::Error> {
        let message = StoredMessage {
            message_id: event.message_id.clone(),
            sender: event.sender.clone(),
            sender_name: event.sender_name.clone(),
            text: event.text.clone(),
            attachments: event.attachments.clone(),
            from_self: event.from_self,
            timestamp: event.timestamp,
        };
        Ok(self.store.append(&event.chat, message)?)
    }

    /// Runs a turn for `event`, serialized per chat.
    ///
    /// Returns the action to take, or `None` when the message must not be
    /// answered — either because the gate says so, or because the model chose
    /// `NO_REPLY`. A duplicate of a message already answered is not answered
    /// again: the adapter stores messages before responding, so a redelivered
    /// trigger is already present when it arrives here.
    pub async fn respond(
        &self,
        event: &InboundEvent,
    ) -> Result<Option<OutboundAction>, crate::Error> {
        let _guard = self.queues.lock(&event.chat).await;

        // Extract before deciding whether to answer: the adapter calls this for
        // every message, so a busy group that never triggers the agent still
        // turns its messages into facts before the message window prunes them.
        self.extract_memory(event).await;
        // Consolidate after extracting, so this pass sees the facts the one
        // before it just added.
        self.reflect_memory(event).await;

        // A reply to a tool confirmation is resolved before the gate: a bare
        // "yes" would otherwise be an acknowledgement in a private chat or a
        // no-trigger in a group. In a group the answer must be directed at the
        // bot — a mention or a reply, like any other group trigger — so an
        // unrelated "ok" in a busy group cannot run a state-changing tool. A
        // message that is not a clear yes or no leaves the held call pending
        // for its TTL to expire.
        let directed =
            event.chat_type == ChatType::Private || event.mentions_self || event.is_reply_to_self;
        if !event.from_self && !event.is_command && directed {
            let answer = safety::classify_confirmation(event.trimmed_text());
            let pending = match answer {
                Confirmation::Unrelated => None,
                _ => self.confirmations.take(
                    &event.chat,
                    chrono::Utc::now(),
                    self.config.confirmation_ttl,
                ),
            };
            match (answer, pending) {
                (Confirmation::Yes, Some(pending)) => {
                    return self.confirm_turn(event, pending).await;
                }
                (Confirmation::No, Some(_)) => {
                    return Ok(Some(OutboundAction::SendText {
                        chat: event.chat.clone(),
                        text: CANCELLED.to_string(),
                    }));
                }
                _ => {}
            }
        }

        match gate::decide(event) {
            GateDecision::StaySilent(reason) => {
                debug!(chat = %event.chat, ?reason, "the agent stayed silent");
                Ok(None)
            }
            GateDecision::Respond(trigger) => {
                self.run_turn(event, trigger, Vec::new(), Vec::new(), true)
                    .await
            }
        }
    }

    /// Runs the memory extraction pass for `event`'s chat, if one is due.
    ///
    /// Called at the top of a turn, before the gate, so a busy group that never
    /// triggers the agent still turns its messages into facts before the window
    /// prunes them. A failed pass is logged, never fatal to the turn — memory is
    /// a background concern, the reply is not.
    async fn extract_memory(&self, event: &InboundEvent) {
        if let Err(error) = writer::extract_if_due(
            self.llm.as_ref(),
            &self.store,
            &self.memory,
            &self.config,
            &event.chat,
            event.chat_type,
        )
        .await
        {
            warn!(chat = %event.chat, %error, "the memory extraction pass failed");
        }
    }

    /// Runs the consolidation pass for `event`'s chat, if one is due.
    ///
    /// Called right after extraction, so a chat's new facts are reflected on
    /// together with the older ones. Like extraction, a failed pass is logged,
    /// never fatal: memory is a background concern, the reply is not.
    async fn reflect_memory(&self, event: &InboundEvent) {
        if let Err(error) = reflection::reflect_if_due(
            self.llm.as_ref(),
            &self.memory,
            &self.config,
            &event.chat,
            event.chat_type,
        )
        .await
        {
            warn!(chat = %event.chat, %error, "the consolidation pass failed");
        }
    }

    /// Runs a turn because a command asked for one, bypassing the gate.
    ///
    /// A command message is `is_command`, so [`gate::decide`] stays silent on it
    /// — the command layer, not the agent, answers commands. A command that
    /// wants the model's help (`!ask`) calls this instead: it takes the same
    /// per-chat lock and runs the same turn, labelled [`Trigger::Command`], so
    /// the trace tells a command-driven turn from a mention or a DM. It runs
    /// extraction like [`respond`](Self::respond) does, and advertises tools, so
    /// the model may search memory or the web.
    pub async fn answer_command(
        &self,
        event: &InboundEvent,
    ) -> Result<Option<OutboundAction>, crate::Error> {
        let _guard = self.queues.lock(&event.chat).await;
        self.extract_memory(event).await;
        self.reflect_memory(event).await;
        self.run_turn(event, Trigger::Command, Vec::new(), Vec::new(), true)
            .await
    }

    /// Summarises `chat`'s stored window with the model, stores the summary, and
    /// returns it.
    ///
    /// This is the `!summary` command's work: a single model call shaped like the
    /// memory writer's extraction pass, not a conversational turn, so it carries
    /// no tools and is not traced as a turn. The reply is screened by the output
    /// guard before it is stored — a summary is still model output, and one that
    /// carries the prompt's own tags must not be persisted as context either.
    /// Returns `None` when no model is configured or the guard drops the reply.
    pub async fn summarize(
        &self,
        chat: &ChatId,
        chat_type: ChatType,
    ) -> Result<Option<String>, crate::Error> {
        let _guard = self.queues.lock(chat).await;
        let window = match chat_type {
            ChatType::Group => self.config.group_window,
            ChatType::Private => self.config.private_window,
        };
        let history = self.store.recent(chat, window)?;
        if history.is_empty() {
            return Ok(None);
        }
        let (system, user) = summary_prompt(&history);
        let request = LlmRequest {
            model: self.config.model.clone(),
            system: system.clone(),
            user,
            max_tokens: self.config.summary_max_tokens,
            tools: Vec::new(),
            messages: Vec::new(),
        };
        let response = match self.llm.complete(request).await {
            Ok(response) => response,
            Err(LlmError::Disabled) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let screened = safety::screen_reply(&response.text, &system, &self.config);
        let summary = screened.map(|text| truncate(&text, self.config.summary_max_chars));
        if let Some(summary) = &summary {
            self.store.set_summary(chat, summary.clone())?;
        }
        Ok(summary)
    }

    /// The facts `event`'s chat may list and forget, newest first.
    ///
    /// Built under the per-chat lock and through the reader's privacy boundary,
    /// so a `!memory` listing never shows a fact from another chat and cannot
    /// race an extraction pass.
    pub async fn list_memory(
        &self,
        event: &InboundEvent,
    ) -> Result<Vec<MemoryRecord>, crate::Error> {
        let _guard = self.queues.lock(&event.chat).await;
        let reader = self.reader_for(event);
        Ok(retrieval::list(&reader, &self.memory, &self.config)?)
    }

    /// Forgets the facts of `event`'s chat that `target` names.
    ///
    /// `target` is the literal `all`, or a prefix of a fact's id. The eligible
    /// set is exactly what [`list_memory`](Self::list_memory) shows —
    /// reader-visible, this chat's, still true, not already forgotten — so a
    /// `!forget` can never reach another chat's memory or one the listing hid.
    /// An ambiguous prefix forgets nothing and reports how many it matched. Runs
    /// under the per-chat lock, so it cannot race an extraction pass.
    pub async fn forget_memory(
        &self,
        event: &InboundEvent,
        target: &str,
    ) -> Result<ForgetOutcome, crate::Error> {
        let _guard = self.queues.lock(&event.chat).await;
        let reader = self.reader_for(event);
        let eligible: Vec<String> = retrieval::forgettable(&reader, &self.memory)?
            .into_iter()
            .map(|record| record.id)
            .collect();

        let targets: Vec<String> = if target.eq_ignore_ascii_case("all") {
            eligible
        } else {
            let matches: Vec<String> = eligible
                .into_iter()
                .filter(|id| id.starts_with(target))
                .collect();
            match matches.len() {
                0 => return Ok(ForgetOutcome::NoMatch),
                1 => matches,
                count => return Ok(ForgetOutcome::Ambiguous { count }),
            }
        };

        if targets.is_empty() {
            return Ok(ForgetOutcome::NoMatch);
        }
        let ops: Vec<MemoryOp> = targets
            .iter()
            .map(|target| MemoryOp::Forget {
                target: target.clone(),
            })
            .collect();
        let count = self.memory.apply(&event.chat, &ops, None)?;
        Ok(ForgetOutcome::Forgotten { count })
    }

    /// Runs a turn, optionally seeded with a transcript a tool result already
    /// produced.
    ///
    /// An ordinary turn plans before it answers, runs the model loop, then has
    /// the evaluator judge the draft and revise it if asked; the confirmation
    /// path seeds the run with the held call and its result, advertises no
    /// tools, and skips planning and evaluation, so the model just narrates what
    /// happened. One function keeps one copy of the prompt-building, loop,
    /// trace, and guard.
    async fn run_turn(
        &self,
        event: &InboundEvent,
        trigger: Trigger,
        seed_messages: Vec<LlmMessage>,
        seed_calls: Vec<ToolCallTrace>,
        allow_tools: bool,
    ) -> Result<Option<OutboundAction>, crate::Error> {
        let reader = self.reader_for(event);
        let window = match event.chat_type {
            ChatType::Group => self.config.group_window,
            ChatType::Private => self.config.private_window,
        };
        let history = self.store.recent(&event.chat, window)?;
        let summary = self.store.summary(&event.chat)?;

        // The facts this reader may see, filtered and ranked by the retrieval
        // layer's privacy boundary. The context builder re-checks every one.
        let memories = retrieval::search(
            &reader,
            event.trimmed_text(),
            &self.memory,
            &self.config,
            chrono::Utc::now(),
        )?;

        // A seeded turn is a narration or a revision already in flight; it does
        // not plan. `allow_tools` marks the ordinary path — a confirmation turn
        // passes false — so an ordinary turn plans only when the request is
        // non-trivial, and the plan becomes a prompt layer.
        let mut reasoning_usage = Usage::default();
        let mut plan_steps = Vec::new();
        if allow_tools {
            let (planned, usage) = reasoning::plan(
                self.llm.as_ref(),
                &self.config,
                event.chat_type,
                summary.as_deref(),
                event,
            )
            .await;
            reasoning_usage = add_usage(reasoning_usage, usage);
            if let Some(steps) = planned {
                plan_steps = steps;
            }
        }
        let plan_layer = (!plan_steps.is_empty()).then(|| reasoning::render_plan(&plan_steps));

        let prompt = ContextBuilder::new(&self.config).plan(plan_layer).build(
            &reader,
            summary.as_deref(),
            &memories,
            &history,
            event,
        );

        let started = Instant::now();

        // The main loop: the model answers, possibly after calling tools. A
        // seeded turn (confirmation or revision) carries a transcript to start
        // from and advertises no tools of its own.
        let mut outcome = self
            .model_loop(
                event,
                &reader,
                &prompt,
                seed_messages,
                seed_calls,
                allow_tools,
            )
            .await;

        // The evaluator judges the draft and, when it asks for a revision, one
        // more turn rewrites it — bounded by the revision budget. It runs on the
        // ordinary path only (`allow_tools`): a narration turn is not judged, and
        // a turn that will not speak has nothing to judge.
        let mut revisions = 0;
        let mut verdict_label = None;
        if allow_tools && self.config.max_revisions > 0 {
            loop {
                let candidate = match &outcome.reply {
                    Some(text) if !safety::is_silent(text) => text.clone(),
                    _ => break,
                };
                let (verdict, usage) = reasoning::evaluate(
                    self.llm.as_ref(),
                    &self.config,
                    event.trimmed_text(),
                    &candidate,
                )
                .await;
                reasoning_usage = add_usage(reasoning_usage, usage);
                let Some(verdict) = verdict else {
                    break;
                };
                verdict_label = Some(if verdict.accept {
                    "ACCEPT".to_string()
                } else {
                    "REVISE".to_string()
                });
                if verdict.accept || revisions >= self.config.max_revisions {
                    break;
                }
                // Revise: run the same loop again, seeded with the draft and the
                // critique and advertising no tools, so the model just improves
                // the reply. A failed revision leaves the draft in place.
                let seed = reasoning::revision_seed(&candidate, &verdict.reason);
                let revised = self
                    .model_loop(event, &reader, &prompt, seed, Vec::new(), false)
                    .await;
                reasoning_usage = add_usage(
                    reasoning_usage,
                    Usage {
                        input: revised.input_tokens,
                        output: revised.output_tokens,
                    },
                );
                if let Some(text) = revised.reply {
                    outcome.reply = Some(text);
                }
                outcome.calls.extend(revised.calls);
                revisions += 1;
            }
        }
        let latency = started.elapsed();

        // The output guard is the one place a reply becomes sendable: it drops a
        // `NO_REPLY`, an empty reply, one carrying the prompt's own tags, or one
        // reciting the system prompt, and truncates the rest.
        let final_reply = match outcome.reply {
            Some(text) => {
                let screened = safety::screen_reply(&text, &prompt.system, &self.config);
                if screened.is_none() {
                    debug!(chat = %event.chat, "the output guard suppressed the reply");
                }
                screened
            }
            None => None,
        };
        self.traces.record(TurnTrace {
            turn_id: uuid::Uuid::new_v4().to_string(),
            chat: event.chat.clone(),
            trigger: TurnTrace::trigger_label(trigger).to_string(),
            model: self.config.model.clone(),
            context_hash: trace::context_hash(&prompt.system, &prompt.user),
            system_chars: prompt.system.chars().count(),
            user_chars: prompt.user.chars().count(),
            input_tokens: add_tokens(outcome.input_tokens, reasoning_usage.input),
            output_tokens: add_tokens(outcome.output_tokens, reasoning_usage.output),
            latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
            tool_calls: outcome.calls,
            plan: plan_steps,
            revisions,
            verdict: verdict_label,
            reply: final_reply.clone(),
            timestamp: chrono::Utc::now(),
        })?;

        Ok(final_reply.map(|text| OutboundAction::SendText {
            chat: event.chat.clone(),
            text,
        }))
    }

    /// Runs the bounded model loop for one turn.
    ///
    /// Calls the model, runs any tools it asks for, and feeds the results back,
    /// up to [`AgentConfig::max_tool_iterations`]. A state-changing tool call is
    /// held for confirmation rather than run. Returns the reply (if any), the
    /// tool calls it recorded, and the tokens it spent. Extracted so the turn
    /// and its revision share one loop.
    async fn model_loop(
        &self,
        event: &InboundEvent,
        reader: &ReaderContext,
        prompt: &Prompt,
        seed_messages: Vec<LlmMessage>,
        seed_calls: Vec<ToolCallTrace>,
        allow_tools: bool,
    ) -> TurnOutcome {
        // The tools the model may call this turn. A seeded turn advertises none
        // — it only narrates or rewrites what it was handed.
        let tools = if allow_tools {
            self.tools.specs()
        } else {
            Vec::new()
        };
        // The turn's identity, handed to every tool call. The registry is shared
        // across chats, so a tool that acts on this conversation reads the chat
        // and reader from here.
        let context = ToolContext::new(reader.clone());

        let mut messages = vec![LlmMessage::Text {
            assistant: false,
            text: prompt.user.clone(),
        }];
        messages.extend(seed_messages);
        let mut calls = seed_calls;
        // Summed over the loop's calls, staying `None` until a call reports a
        // count — the same answer a single call gave before the loop existed.
        let mut input_tokens: Option<u32> = None;
        let mut output_tokens: Option<u32> = None;

        // The loop is bounded: the model normally answers in one call, and a
        // model that keeps calling tools is cut off rather than looped forever.
        // The last iteration is a final chance to answer, so it advertises no
        // tools — a call there would have no turn left to use its result.
        let mut reply = None;
        let max_iterations = if allow_tools {
            self.config.max_tool_iterations
        } else {
            0
        };
        for iteration in 0..=max_iterations {
            let last = iteration == max_iterations;
            let request = LlmRequest {
                model: self.config.model.clone(),
                system: prompt.system.clone(),
                user: prompt.user.clone(),
                max_tokens: self.config.max_reply_tokens,
                tools: if last { Vec::new() } else { tools.clone() },
                messages: messages.clone(),
            };
            let response = match self.llm.complete(request).await {
                Ok(response) => response,
                Err(LlmError::Disabled) => {
                    debug!(chat = %event.chat, "no model is configured; not replying");
                    break;
                }
                Err(error) => {
                    warn!(chat = %event.chat, %error, "the agent turn failed");
                    break;
                }
            };
            input_tokens = add_tokens(input_tokens, response.input_tokens);
            output_tokens = add_tokens(output_tokens, response.output_tokens);

            // The last iteration answers with what it has; there is no turn left
            // to use a tool result, so any call it still makes is not run.
            if last || response.tool_calls.is_empty() {
                reply = Some(response.text);
                break;
            }

            // A state-changing tool is never run on the model's word. Hold the
            // first such call, ask the chat, and end the turn — the call runs
            // only when the user confirms. No call in this response runs, so a
            // confirmed action is never half-applied alongside an unconfirmed
            // one.
            if let Some((call, question)) = response
                .tool_calls
                .iter()
                .find_map(|call| self.confirmation_for(call).map(|question| (call, question)))
            {
                self.confirmations.put(
                    &event.chat,
                    PendingToolCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        call_id: call.id.clone(),
                        created: chrono::Utc::now(),
                    },
                );
                calls.push(ToolCallTrace {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    result: AWAITING_CONFIRMATION.to_string(),
                });
                reply = Some(format!("{question}\n\n{CONFIRM_HINT}"));
                break;
            }

            // The model asked for tools. Run each, record it, and feed the
            // results back as the next turn's input.
            let results = self
                .run_tool_calls(&context, &response.tool_calls, &mut calls)
                .await;
            messages.push(LlmMessage::ToolCalls(response.tool_calls));
            messages.push(LlmMessage::ToolResults(results));
        }

        TurnOutcome {
            reply,
            calls,
            input_tokens,
            output_tokens,
        }
    }

    /// Runs a held tool call the user has confirmed, then a final turn that
    /// narrates the result.
    ///
    /// The tool runs here, after the "yes", never before: holding the call and
    /// running it in this separate turn is what makes the confirmation real
    /// rather than advisory.
    async fn confirm_turn(
        &self,
        event: &InboundEvent,
        pending: PendingToolCall,
    ) -> Result<Option<OutboundAction>, crate::Error> {
        let arguments: serde_json::Value =
            serde_json::from_str(&pending.arguments).unwrap_or(serde_json::json!({}));
        // The held call runs with the same turn context it was held under, so a
        // tool that acts on the chat still knows which chat that is.
        let context = ToolContext::for_event(event);
        let result = match self.tools.get(&pending.name) {
            Some(tool) => tool.call(&context, &arguments).await,
            None => Err(format!("no tool named `{}`", pending.name)),
        };
        let content = match result {
            Ok(content) => content,
            Err(error) => format!("the tool failed: {error}"),
        };
        let call = LlmToolCall {
            id: pending.call_id.clone(),
            name: pending.name.clone(),
            arguments: pending.arguments.clone(),
        };
        let recorded = ToolCallTrace {
            name: pending.name.clone(),
            arguments: pending.arguments.clone(),
            result: content.clone(),
        };
        let seed = vec![
            LlmMessage::ToolCalls(vec![call]),
            LlmMessage::ToolResults(vec![LlmToolResult {
                id: pending.call_id.clone(),
                content,
            }]),
        ];
        self.run_turn(event, Trigger::Confirmation, seed, vec![recorded], false)
            .await
    }

    /// The confirmation question a tool call needs, or `None` when it may run.
    ///
    /// Every tool decides for itself: a read-only tool leaves
    /// [`Tool::confirmation`](crate::tools::Tool::confirmation) at its `None`
    /// default, a state-changing one overrides it. The registry is asked, so a
    /// call the model invented needs no confirmation because it will not run at
    /// all.
    fn confirmation_for(&self, call: &LlmToolCall) -> Option<String> {
        let tool = self.tools.get(&call.name)?;
        let arguments: serde_json::Value =
            serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
        tool.confirmation(&arguments)
    }

    /// Runs every tool call `calls` asked for, recording each and returning the
    /// results to hand back to the model.
    ///
    /// A tool that fails returns a short error string as its result rather than
    /// failing the turn, so the model can still answer around it.
    async fn run_tool_calls(
        &self,
        context: &ToolContext,
        calls: &[LlmToolCall],
        recorded: &mut Vec<ToolCallTrace>,
    ) -> Vec<LlmToolResult> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let arguments: serde_json::Value =
                serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
            let result = match self.tools.get(&call.name) {
                Some(tool) => tool.call(context, &arguments).await,
                None => Err(format!("no tool named `{}`", call.name)),
            };
            let content = match result {
                Ok(content) => content,
                Err(error) => format!("the tool failed: {error}"),
            };
            recorded.push(ToolCallTrace {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                result: content.clone(),
            });
            results.push(LlmToolResult {
                id: call.id.clone(),
                content,
            });
        }
        results
    }

    /// The message store, for tests and diagnostics.
    pub fn store(&self) -> &MessageStore {
        &self.store
    }

    /// The memory store, for tests and diagnostics.
    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    /// The trace log, for tests and diagnostics.
    pub fn traces(&self) -> &TraceSink {
        &self.traces
    }

    /// The privacy context for a turn triggered by `event`.
    fn reader_for(&self, event: &InboundEvent) -> ReaderContext {
        ReaderContext::for_event(event)
    }
}

/// What a [`Agent::forget_memory`] call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgetOutcome {
    /// `count` facts were marked forgotten.
    Forgotten {
        /// How many facts the call forgot.
        count: usize,
    },
    /// No fact matched the target.
    NoMatch,
    /// The target matched more than one fact, so nothing was forgotten.
    Ambiguous {
        /// How many facts the target matched.
        count: usize,
    },
}

/// Adds a call's token count to the turn's running total.
///
/// A count stays `None` until some call reports one, so a provider that reports
/// no usage yields `None` rather than a misleading `0`.
fn add_tokens(total: Option<u32>, reported: Option<u32>) -> Option<u32> {
    match (total, reported) {
        (Some(total), Some(reported)) => Some(total + reported),
        (Some(total), None) => Some(total),
        (None, reported) => reported,
    }
}

/// Adds one reasoning call's usage to the turn's running reasoning total.
fn add_usage(total: Usage, reported: Usage) -> Usage {
    Usage {
        input: add_tokens(total.input, reported.input),
        output: add_tokens(total.output, reported.output),
    }
}

/// What one run of the model loop produced.
struct TurnOutcome {
    /// The reply the model settled on, or `None` when no call returned one.
    reply: Option<String>,
    /// The tool calls the loop ran, recorded for the trace.
    calls: Vec<ToolCallTrace>,
    /// Input tokens the loop's calls reported, summed.
    input_tokens: Option<u32>,
    /// Output tokens the loop's calls reported, summed.
    output_tokens: Option<u32>,
}

/// The prompt for the `!summary` pass: the messages to summarise.
///
/// Shaped like the memory writer's extraction prompt — a stable instruction and
/// a body of trust-tagged, escaped messages — so the model reads conversation
/// content as data, not instructions.
fn summary_prompt(history: &[StoredMessage]) -> (String, String) {
    let system = "You summarise a chat conversation for a long-term memory note. Reply with a \
                  short, factual summary in the third person — who took part, what was \
                  discussed, and any decisions or plans — and nothing else. Do not add \
                  preamble, headings, or commentary about the task itself."
        .to_string();

    let mut user = String::new();
    for message in history {
        let name = message.sender_name.as_deref().unwrap_or("");
        let text = format!(
            "{}{}",
            attachment_note(&message.attachments),
            escape(message.text.as_deref().unwrap_or("")),
        );
        user.push_str(&format!(
            "<chat_message role=\"{}\" sender=\"{}\" name=\"{}\">{}</chat_message>\n",
            if message.from_self {
                "assistant"
            } else {
                "user"
            },
            escape(message.sender.as_str()),
            escape(name),
            text,
        ));
    }
    (system, user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Visibility;
    use crate::event::SenderId;
    use crate::llm::ScriptedLlm;
    use crate::memory::store::NewFact;
    use chrono::Utc;

    fn agent(replies: impl IntoIterator<Item = &'static str>) -> (Agent, Arc<ScriptedLlm>) {
        let llm = Arc::new(ScriptedLlm::new(replies.into_iter().map(|text| {
            crate::llm::LlmResponse {
                text: text.to_string(),
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            }
        })));
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
        let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
        let agent = Agent::new(
            store,
            memory,
            traces,
            llm.clone(),
            Arc::new(ToolRegistry::new(Vec::new())),
            AgentConfig::for_test(),
        );
        (agent, llm)
    }

    fn group_event(text: &str, mentioned: bool) -> InboundEvent {
        InboundEvent {
            message_id: format!("m-{text}"),
            chat: ChatId::new("gA"),
            chat_type: ChatType::Group,
            sender: SenderId::new("u1"),
            sender_alt: None,
            sender_name: Some("Budi".into()),
            text: Some(text.into()),
            attachments: Vec::new(),
            reply_to: None,
            mentions_self: mentioned,
            is_reply_to_self: false,
            is_command: false,
            from_self: false,
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn an_untriggered_group_message_is_stored_but_not_answered() {
        let (agent, llm) = agent(["should not be used"]);
        let event = group_event("just chatting", false);
        assert!(agent.ingest(&event).unwrap());
        assert!(agent.respond(&event).await.unwrap().is_none());
        assert!(llm.requests().is_empty());
        assert_eq!(agent.store.len(&event.chat).unwrap(), 1);
    }

    #[tokio::test]
    async fn a_mentioned_group_message_is_answered() {
        let (agent, _llm) = agent(["hello there"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        let action = agent.respond(&event).await.unwrap();
        assert_eq!(
            action,
            Some(OutboundAction::SendText {
                chat: ChatId::new("gA"),
                text: "hello there".into(),
            })
        );
    }

    #[tokio::test]
    async fn no_reply_from_the_model_means_silence() {
        let (agent, _llm) = agent(["NO_REPLY"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        assert!(agent.respond(&event).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_turn_is_recorded_in_the_trace_log() {
        let (agent, _llm) = agent(["hello"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        agent.respond(&event).await.unwrap();
        let traces = agent.traces.recent(10).unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].trigger, "mention");
        assert_eq!(traces[0].reply.as_deref(), Some("hello"));
        assert_eq!(traces[0].input_tokens, Some(1));
    }

    #[tokio::test]
    async fn a_private_message_is_answered() {
        let (agent, _llm) = agent(["of course"]);
        let mut event = group_event("what's up?", false);
        event.chat = ChatId::new("dm");
        event.chat_type = ChatType::Private;
        agent.ingest(&event).unwrap();
        let action = agent.respond(&event).await.unwrap();
        assert!(matches!(action, Some(OutboundAction::SendText { .. })));
    }

    #[test]
    fn a_reply_is_detected_from_the_history_window() {
        let (agent, _llm) = agent(["hello"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        let history = agent.store.recent(&event.chat, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].sender_name.as_deref(), Some("Budi"));
    }

    #[tokio::test]
    async fn a_command_forces_a_turn_even_though_the_gate_would_stay_silent() {
        let (agent, _llm) = agent(["42"]);
        let mut event = group_event("!ask what is the answer?", false);
        event.is_command = true;
        agent.ingest(&event).unwrap();

        let action = agent.answer_command(&event).await.unwrap();
        assert_eq!(
            action,
            Some(OutboundAction::SendText {
                chat: ChatId::new("gA"),
                text: "42".into(),
            })
        );
        // The trace labels it a command, not a mention.
        let traces = agent.traces.recent(10).unwrap();
        assert_eq!(traces[0].trigger, "command");
    }

    #[tokio::test]
    async fn summarize_stores_a_screened_summary() {
        let (agent, _llm) = agent(["Budi asked about the venue."]);
        let event = group_event("where is the venue?", true);
        agent.ingest(&event).unwrap();

        let summary = agent.summarize(&event.chat, ChatType::Group).await.unwrap();
        assert_eq!(summary.as_deref(), Some("Budi asked about the venue."));
        assert_eq!(
            agent.store.summary(&event.chat).unwrap().as_deref(),
            Some("Budi asked about the venue.")
        );
    }

    #[tokio::test]
    async fn summarize_drops_a_summary_carrying_the_prompts_tags() {
        let (agent, _llm) = agent(["<chat_message>leaked</chat_message>"]);
        let event = group_event("hi", true);
        agent.ingest(&event).unwrap();

        assert!(
            agent
                .summarize(&event.chat, ChatType::Group)
                .await
                .unwrap()
                .is_none()
        );
        // Nothing was stored either: a screened-out summary is not persisted.
        assert!(agent.store.summary(&event.chat).unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_memory_forgets_only_this_chats_live_facts() {
        let (agent, _llm) = agent(["unused"]);
        let chat = ChatId::new("gA");
        agent
            .memory
            .apply(
                &chat,
                &[MemoryOp::Add(NewFact {
                    content: "the venue is the old hall".into(),
                    visibility: Visibility::Chat,
                    subject: None,
                    confidence: 0.9,
                    importance: 0.5,
                    valid_from: Utc::now(),
                    evidence: vec!["m1".into()],
                })],
                None,
            )
            .unwrap();
        let id = agent.memory.records().unwrap()[0].id.clone();

        let event = group_event("forget it", true);
        // A unique id prefix forgets the one matching fact.
        assert_eq!(
            agent.forget_memory(&event, &id[..4]).await.unwrap(),
            ForgetOutcome::Forgotten { count: 1 }
        );
        // A second forget finds nothing left to forget.
        assert_eq!(
            agent.forget_memory(&event, &id).await.unwrap(),
            ForgetOutcome::NoMatch
        );
        assert!(agent.memory.get(&id).unwrap().unwrap().forgotten);
    }
}
