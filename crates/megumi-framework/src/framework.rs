use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tracing::{error, warn};
use whatsapp_rust::Client;
use whatsapp_rust::bot::{BotBuilder, MessageContext};
use whatsapp_rust::types::events::{Event, EventKind};

use crate::DEFAULT_PREFIX;
use crate::args::Args;
use crate::command::{Command, IntoCommands, Registry};
use crate::context::{Context, FrameworkData, NoData};
use crate::error::{Error, FrameworkError};
use crate::group::CommandGroup;
use crate::help;
use crate::parse::{command_text, parse_command_text};
use crate::reply::CreateReply;

/// A future that outlives the call that produced it, the shape poise uses for
/// its `on_error` callback.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The one-shot closure that produces the framework's user data, poise's
/// `setup`, run by the first event to reach [`Framework::resolve_user_data`].
///
/// A framework that never called `setup` has no closure: [`UserDataCell::ready`]
/// starts it already resolved, so it skips the lazy step entirely.
type SetupFn<U> = Box<dyn FnOnce(Arc<Client>) -> BoxFuture<Result<U, Error>> + Send + Sync>;

/// The framework's user data: produced once by `setup`, then read by every event
/// without touching a lock.
///
/// The read is on the per-event path, and whatsapp-rust dispatches each event on
/// its own task, so the steady-state read must not contend. `resolved` is a
/// [`std::sync::OnceLock`], read with a plain atomic load; `setup` is a
/// `tokio::sync::Mutex` locked only by the first event, and held across the
/// setup future so events arriving together wait for that one run rather than
/// racing. Once `resolved` is set the fast path returns before the lock is
/// touched.
struct UserDataCell<U> {
    /// `Some(data)` once setup succeeded, `Some(None)` once it failed; `None`
    /// until the first event resolves it.
    resolved: std::sync::OnceLock<Option<Arc<U>>>,
    /// The setup closure, taken by the first event. Empty once it has run.
    setup: tokio::sync::Mutex<Option<SetupFn<U>>>,
}

impl<U: FrameworkData> UserDataCell<U> {
    /// A cell whose value is already there, for a framework with no `setup`.
    ///
    /// The fast path works from the first event, so `NoData` costs no lazy step.
    fn ready(data: Arc<U>) -> Self {
        let resolved = std::sync::OnceLock::new();
        let _ = resolved.set(Some(data));
        Self {
            resolved,
            setup: tokio::sync::Mutex::new(None),
        }
    }

    /// A cell that runs `setup` on the first event to arrive.
    fn pending(setup: SetupFn<U>) -> Self {
        Self {
            resolved: std::sync::OnceLock::new(),
            setup: tokio::sync::Mutex::new(Some(setup)),
        }
    }
}

/// A hook that sees every event the client dispatches, poise's
/// `FrameworkOptions::event_handler`.
///
/// It runs after the framework has handled the event's messages, and receives a
/// [`FrameworkContext`] so it can reach the client and the user data. Returning
/// an error is logged; it is not routed through the command `on_error`, which
/// expects a command context this hook has none of.
pub type EventHook<U = NoData> =
    fn(FrameworkContext<U>, Arc<Event>) -> BoxFuture<Result<(), Error>>;

/// The view an [`EventHook`] runs with: the connected client and the user data.
///
/// This is poise's `FrameworkContext`, reduced to what WhatsApp offers. The
/// fields are owned `Arc`s rather than borrows because the framework's
/// [`BoxFuture`] is `'static` and whatsapp-rust hands each event over as an
/// `Arc`, so there is nothing to borrow from.
pub struct FrameworkContext<U: FrameworkData = NoData> {
    /// The client that dispatched the event.
    pub client: Arc<Client>,
    /// The framework's user data, already initialized.
    pub data: Arc<U>,
}

/// Called with the error the dispatcher produced and the context it happened in.
///
/// This is poise's `FrameworkOptions::on_error`, adapted: the failing
/// [`Context`] travels next to the error so the handler can answer in the chat.
/// `U` is the framework's user data, so the handler can read [`Context::data`]
/// exactly as a command body can.
pub type ErrorHandler<U = NoData> = fn(FrameworkError, Context<U>) -> BoxFuture<()>;

/// A check that decides whether a command may run, poise's `command_check`.
pub type Check<U = NoData> = fn(Context<U>) -> BoxFuture<Result<bool, Error>>;

/// A hook that runs before or after a command, poise's `pre_command` and
/// `post_command`.
pub type Hook<U = NoData> = fn(Context<U>) -> BoxFuture<()>;

/// Sees every [`CreateReply`] on its way out, so a framework can stamp a
/// footer or sanitise content without touching each command.
///
/// This is poise's `reply_callback`, simplified to the synchronous shape a
/// WhatsApp reply allows. It is called for every reply a command sends through
/// [`Context::send`](crate::Context::send) (and therefore
/// [`Context::say`](crate::Context::say)); it is **not** called for the edit
/// path `reuse_response` takes, nor for [`Context::send_raw`](crate::Context::send_raw),
/// which hands over a prebuilt stanza.
pub type ReplyCallback<U = NoData> = fn(&Context<U>, CreateReply) -> CreateReply;

/// The handler a framework uses unless [`FrameworkBuilder::on_error`] replaces it.
///
/// It answers the chat with what went wrong, the way poise's builtin `on_error`
/// does, quoting the message that caused it so the report lands under the
/// mistake, and only logs when the reply itself could not be sent.
pub fn default_on_error<U: FrameworkData>(error: FrameworkError, ctx: Context<U>) -> BoxFuture<()> {
    Box::pin(async move {
        let text = match &error {
            FrameworkError::UnknownCommand { command } => {
                format!("Unknown command `{command}`. Try `{}help`.", ctx.prefix())
            }
            error => error.to_string(),
        };

        if let Err(send_error) = ctx.reply_quoting(text).await {
            warn!(%send_error, %error, "could not report the error to the chat");
        }
    })
}

/// A registered command registry, and the policy the dispatcher applies to it.
///
/// Build one with [`Framework::builder`], and inject it into the WhatsApp client
/// with [`FrameworkExt::framework`], which is how poise hands its framework to
/// Serenity's client builder. The client then runs [`Framework::dispatch_event`]
/// for every event.
pub struct Framework<U: FrameworkData = NoData> {
    prefixes: Arc<Vec<String>>,
    /// The commands and group descriptions, shared with every `Context` so help
    /// can be rendered on demand without a second `Arc` per field.
    registry: Arc<Registry<U>>,
    on_error: ErrorHandler<U>,
    pre_command: Hook<U>,
    post_command: Hook<U>,
    command_check: Option<Check<U>>,
    reply_callback: Option<ReplyCallback<U>>,
    event_handler: Option<EventHook<U>>,
    skip_checks_for_owners: bool,
    manual_cooldowns: bool,
    /// When true, a message that carries the prefix but no known command is
    /// left alone instead of being answered. Poise's `non_command_message`
    /// fills this role by simply not being set.
    report_unknown_commands: bool,
    /// The bot's user data. Poise fills its equivalent from an async `setup`
    /// callback on Discord's Ready event; WhatsApp has no guaranteed ordering
    /// between its events, so the value is produced lazily on the first event
    /// and shared with every context the dispatcher builds.
    user_data: Arc<UserDataCell<U>>,
}

impl<U: FrameworkData> Clone for Framework<U> {
    fn clone(&self) -> Self {
        Self {
            prefixes: Arc::clone(&self.prefixes),
            registry: Arc::clone(&self.registry),
            on_error: self.on_error,
            pre_command: self.pre_command,
            post_command: self.post_command,
            command_check: self.command_check,
            reply_callback: self.reply_callback,
            event_handler: self.event_handler,
            skip_checks_for_owners: self.skip_checks_for_owners,
            manual_cooldowns: self.manual_cooldowns,
            report_unknown_commands: self.report_unknown_commands,
            user_data: Arc::clone(&self.user_data),
        }
    }
}

impl Framework<NoData> {
    /// A builder for a framework that carries no user data.
    ///
    /// Call [`FrameworkBuilder::setup`] to switch the builder to a framework
    /// whose commands can reach shared state through [`Context::data`].
    pub fn builder() -> FrameworkBuilder<NoData> {
        FrameworkBuilder::new()
    }
}

impl<U: FrameworkData> Framework<U> {
    /// Handles one event from the client: dispatches the messages it carries,
    /// then runs the [`EventHook`] if the framework set one.
    ///
    /// This is what [`FrameworkExt::framework`] wires the client to, so it is
    /// the framework's whole entry point, poise's `dispatch`. The user data is
    /// resolved here — built by `setup` on the first event, reused after — and a
    /// framework whose setup failed drops the event rather than running a
    /// command that would find no data.
    pub async fn dispatch_event(&self, client: Arc<Client>, event: Arc<Event>) {
        let Some(data) = self.resolve_user_data(&client).await else {
            return;
        };

        // A message event carries one or more messages; every other kind
        // carries none, so this loop is a no-op for them.
        if let Some(batch) = event.as_messages() {
            for inbound in batch.iter() {
                let message = MessageContext::from_inbound(inbound, Arc::clone(&client));
                self.dispatch_message(message, Arc::clone(&data)).await;
            }
        }

        if let Some(handler) = self.event_handler {
            let ctx = FrameworkContext {
                client,
                data: Arc::clone(&data),
            };
            if let Err(error) = handler(ctx, event).await {
                error!(%error, "the framework event handler failed");
            }
        }
    }

    /// Resolves the user data, running `setup` on the first call.
    ///
    /// The steady state is a lock-free atomic load. Only when the value is not
    /// yet there does this take the setup lock, and it holds it across the setup
    /// future: an event arriving while the first is still setting up waits on
    /// the lock and then finds the value in the re-check, so `setup` runs once
    /// and no event observes a half-built cell. A failure is stored as `None`,
    /// so every later event returns `None` without retrying or logging again.
    async fn resolve_user_data(&self, client: &Arc<Client>) -> Option<Arc<U>> {
        if let Some(data) = self.user_data.resolved.get() {
            return data.clone();
        }

        let mut setup = self.user_data.setup.lock().await;
        // Re-check under the lock: the first event may have finished setup while
        // this one was waiting for it.
        if let Some(data) = self.user_data.resolved.get() {
            return data.clone();
        }

        let data = match setup.take() {
            Some(setup) => match setup(Arc::clone(client)).await {
                Ok(data) => Some(Arc::new(data)),
                Err(error) => {
                    error!(%error, "the framework setup failed");
                    None
                }
            },
            // A previous event already took the closure and its setup panicked
            // before storing anything. Fail closed and store `None` below, so
            // this does not take the lock on every event from here on.
            None => {
                error!(
                    "the framework setup was interrupted before completing; dropping this and \
                     every later event"
                );
                None
            }
        };
        let _ = self.user_data.resolved.set(data.clone());
        data
    }

    /// Dispatches one message: resolves the command it names, runs the gates,
    /// and invokes it, reporting any failure through the command's `on_error` or
    /// the framework's.
    ///
    /// This is the hot path, so it is written to allocate as little as
    /// possible: a message that is not a command for this bot returns after one
    /// prefix check. An unknown command that will be answered owns the prefix
    /// and the name it did not recognise; one that will be ignored is still
    /// classified, then dropped.
    async fn dispatch_message(&self, message: MessageContext, data: Arc<U>) {
        // The route is owned, so the borrow of `message` ends here and the
        // message can move into the context below.
        let route = {
            let Some(text) = command_text(&message.message) else {
                return;
            };
            classify(text, &self.prefixes, &self.registry)
        };

        match route {
            Route::NotACommand => {}
            Route::Unknown { prefix, command } => {
                if !self.report_unknown_commands {
                    return;
                }
                let mut ctx = self.context(message, data);
                ctx.prefix = prefix;
                ctx.commands = Some(Arc::clone(&self.registry));
                ctx.reply_callback = self.reply_callback;
                (self.on_error)(FrameworkError::UnknownCommand { command }, ctx).await;
            }
            Route::Command {
                prefix,
                command,
                params,
                invoked_name,
            } => {
                let mut ctx = self.context(message, data);
                ctx.args = Args::from_owned(params);
                ctx.commands = Some(Arc::clone(&self.registry));
                ctx.prefix = prefix;
                ctx.invoked_command_name = invoked_name;
                ctx.pre_command = self.pre_command;
                ctx.post_command = self.post_command;
                ctx.command_check = self.command_check;
                ctx.reply_callback = self.reply_callback;
                ctx.skip_checks_for_owners = self.skip_checks_for_owners;
                ctx.manual_cooldowns = self.manual_cooldowns;
                ctx.command = Some(Arc::clone(&command));

                if command.subcommand_required {
                    let error = FrameworkError::SubcommandRequired {
                        command: command.name.clone(),
                        subcommands: command
                            .subcommands
                            .iter()
                            .map(|child| child.name.clone())
                            .collect(),
                    };
                    let handler = command.on_error.unwrap_or(self.on_error);
                    handler(error, ctx).await;
                    return;
                }

                // `Command::invoke` traces the outcome; the error handler answers the chat.
                if let Err(error) = command.invoke(ctx.clone()).await {
                    let handler = command.on_error.unwrap_or(self.on_error);
                    handler(error, ctx).await;
                }
            }
        }
    }

    /// The rendered listing of every command, grouped by their `group` label.
    pub fn help_text(&self) -> String {
        help::help_text(&self.registry, self.primary_prefix())
    }

    /// The help text for a single command, addressed by name, alias, or
    /// `parent child` path.
    pub fn command_help(&self, name: &str) -> Option<String> {
        help::command_help(&self.registry, name, self.primary_prefix())
    }

    /// The first prefix, which help text is rendered with.
    pub fn primary_prefix(&self) -> &str {
        self.prefixes.first().map(String::as_str).unwrap_or("")
    }
}

/// What a message resolves to before any gate runs.
///
/// The dispatcher's whole decision — prefix, name, and subcommand walk — is
/// computed here, in one pure function with no client access, so it can be
/// tested without a live WhatsApp session.
enum Route<U: FrameworkData> {
    /// The text does not start with any configured prefix.
    NotACommand,
    /// A prefix matched, but the name is not registered.
    Unknown {
        /// The prefix that matched.
        prefix: String,
        /// The name the user typed.
        command: String,
    },
    /// A registered command, resolved to the deepest matching subcommand.
    Command {
        /// The prefix that matched.
        prefix: String,
        /// The resolved command.
        command: Arc<Command<U>>,
        /// The arguments left after the command path.
        params: String,
        /// The leaf name the user typed, which may be an alias.
        invoked_name: String,
    },
}

impl<U: FrameworkData> std::fmt::Debug for Route<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotACommand => f.write_str("NotACommand"),
            Self::Unknown { prefix, command } => f
                .debug_struct("Unknown")
                .field("prefix", prefix)
                .field("command", command)
                .finish(),
            Self::Command {
                prefix,
                command,
                params,
                invoked_name,
            } => f
                .debug_struct("Command")
                .field("prefix", prefix)
                .field("name", &command.name)
                .field("params", params)
                .field("invoked_name", invoked_name)
                .finish(),
        }
    }
}

/// Resolves `text` against the prefixes and the command registry.
fn classify<U: FrameworkData>(text: &str, prefixes: &[String], registry: &Registry<U>) -> Route<U> {
    let Some((prefix, after_prefix)) = strip_prefix(text, prefixes) else {
        return Route::NotACommand;
    };
    let Some((command_name, raw_params)) = parse_command_text(after_prefix, "") else {
        return Route::NotACommand;
    };

    // Aliases are stored lowercased, so the common already-lowercase name needs
    // no allocation to look up.
    let key = if command_name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(command_name.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(command_name)
    };
    let Some(command) = registry.commands.get(key.as_ref()).cloned() else {
        return Route::Unknown {
            prefix: prefix.to_string(),
            command: command_name.to_string(),
        };
    };

    let (command, invoked_name, params) = resolve_subcommand(command, command_name, raw_params);
    Route::Command {
        prefix: prefix.to_string(),
        command,
        params: params.to_string(),
        invoked_name,
    }
}

/// The first prefix `text` starts with, and the text after it.
fn strip_prefix<'a>(text: &'a str, prefixes: &[String]) -> Option<(&'a str, &'a str)> {
    let text = text.trim_start();
    prefixes.iter().find_map(|prefix| {
        text.strip_prefix(prefix.as_str())
            .map(|rest| (&text[..prefix.len()], rest.trim_start()))
    })
}

/// Descends into `command`'s subcommands for as long as the next word names one.
///
/// Returns the resolved command, the leaf name the user typed (`typed_name`
/// when no subcommand matched), and the arguments left over.
fn resolve_subcommand<'a, U: FrameworkData>(
    mut command: Arc<Command<U>>,
    typed_name: &str,
    mut remaining: &'a str,
) -> (Arc<Command<U>>, String, &'a str) {
    let mut invoked = typed_name.to_string();
    loop {
        let (word, rest) = remaining
            .split_once(char::is_whitespace)
            .unwrap_or((remaining, ""));
        if word.is_empty() {
            break;
        }
        let Some(child) = command.find_subcommand(word) else {
            break;
        };
        command = Arc::clone(child);
        invoked = word.to_string();
        remaining = rest.trim_start();
    }
    (command, invoked, remaining)
}

/// Builds a [`Framework`] from the commands and policy it is given.
///
/// The builder is fluent and consumes itself, the way poise's is: each setter
/// returns the builder, and [`build`](FrameworkBuilder::build) produces the
/// framework.
pub struct FrameworkBuilder<U: FrameworkData = NoData> {
    prefixes: Vec<String>,
    commands: Vec<Command<U>>,
    group_descriptions: HashMap<String, String>,
    on_error: ErrorHandler<U>,
    pre_command: Hook<U>,
    post_command: Hook<U>,
    command_check: Option<Check<U>>,
    reply_callback: Option<ReplyCallback<U>>,
    event_handler: Option<EventHook<U>>,
    skip_checks_for_owners: bool,
    manual_cooldowns: bool,
    report_unknown_commands: bool,
    user_data: UserDataCell<U>,
}

impl FrameworkBuilder<NoData> {
    /// Switches this builder to one whose commands share the data `setup`
    /// returns, poise's `FrameworkBuilder::setup`.
    ///
    /// The closure is async and is handed the connected [`Client`], so the data
    /// can depend on the account that only exists after pairing. It runs once,
    /// lazily, on the first event the framework sees — not at build time, and
    /// not only on `Connected`, because WhatsApp may deliver a message before it
    /// announces the connection. A failed setup is logged and drops every event.
    ///
    /// ```no_run
    /// use std::sync::atomic::AtomicU64;
    /// use megumi::{Error, Framework};
    ///
    /// struct Data {
    ///     invocations: AtomicU64,
    /// }
    ///
    /// let framework = Framework::builder()
    ///     .setup(|_client| async move {
    ///         Ok(Data { invocations: AtomicU64::new(0) })
    ///     })
    ///     .prefix("!")
    ///     .build();
    /// ```
    ///
    /// Prefixes and the boolean policy flags already set on this builder are
    /// kept. Hooks (`on_error`, `pre_command`, `post_command`, `command_check`,
    /// `reply_callback`, `event_handler`) cannot follow: they are `fn` pointers
    /// of `U`, and this method changes `U` from [`NoData`] to `D`. Attach them
    /// after `setup`.
    ///
    /// # Panics
    ///
    /// Panics when commands were already added. The two builder types are
    /// parameterised by different data types, so a `Command<NoData>` registered
    /// before `setup` cannot become a `Command<D>` and would be dropped
    /// silently; call `setup` first, as poise does.
    pub fn setup<D, F, Fut>(self, setup: F) -> FrameworkBuilder<D>
    where
        D: FrameworkData,
        F: FnOnce(Arc<Client>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<D, Error>> + Send + 'static,
    {
        assert!(
            self.commands.is_empty(),
            "`setup` must be called before any command is added: a command \
             registered as `Command<NoData>` cannot carry the new data type"
        );
        FrameworkBuilder {
            prefixes: self.prefixes,
            commands: Vec::new(),
            group_descriptions: HashMap::new(),
            on_error: default_on_error,
            pre_command: crate::command::noop_hook,
            post_command: crate::command::noop_hook,
            command_check: None,
            reply_callback: None,
            event_handler: None,
            skip_checks_for_owners: self.skip_checks_for_owners,
            manual_cooldowns: self.manual_cooldowns,
            report_unknown_commands: self.report_unknown_commands,
            user_data: UserDataCell::pending(Box::new(move |client| Box::pin(setup(client)))),
        }
    }
}

impl<U: FrameworkData> FrameworkBuilder<U> {
    fn fresh(user_data: UserDataCell<U>) -> Self {
        Self {
            prefixes: vec![DEFAULT_PREFIX.to_string()],
            commands: Vec::new(),
            group_descriptions: HashMap::new(),
            on_error: default_on_error,
            pre_command: crate::command::noop_hook,
            post_command: crate::command::noop_hook,
            command_check: None,
            reply_callback: None,
            event_handler: None,
            skip_checks_for_owners: false,
            manual_cooldowns: false,
            report_unknown_commands: true,
            user_data,
        }
    }

    /// Replaces the prefixes the framework answers to with the single `prefix`.
    ///
    /// Defaults to [`DEFAULT_PREFIX`].
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefixes = vec![prefix.into()];
        self
    }

    /// Adds prefixes recognised in addition to the primary one, poise's
    /// `additional_prefixes`.
    pub fn additional_prefixes(
        mut self,
        prefixes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.prefixes.extend(prefixes.into_iter().map(Into::into));
        self
    }

    /// Replaces the handler that reacts to rejected and failed commands.
    ///
    /// Defaults to [`default_on_error`].
    pub fn on_error(mut self, handler: ErrorHandler<U>) -> Self {
        self.on_error = handler;
        self
    }

    /// Called before every command that passes its checks.
    pub fn pre_command(mut self, hook: Hook<U>) -> Self {
        self.pre_command = hook;
        self
    }

    /// Called after every command that returned `Ok`.
    pub fn post_command(mut self, hook: Hook<U>) -> Self {
        self.post_command = hook;
        self
    }

    /// A check every command must pass, in addition to its own.
    pub fn command_check(mut self, check: Check<U>) -> Self {
        self.command_check = Some(check);
        self
    }

    /// Sees every reply a command sends through [`Context::send`](crate::Context::send)
    /// before it goes out.
    pub fn reply_callback(mut self, callback: ReplyCallback<U>) -> Self {
        self.reply_callback = Some(callback);
        self
    }

    /// Called for every event the client dispatches, poise's `event_handler`.
    ///
    /// The hook runs after the framework has handled the event's messages, so a
    /// handler can react to non-command traffic — a group update, a connect or
    /// logout, a receipt. It is the WhatsApp-shaped counterpart to poise's
    /// single generic event handler.
    pub fn event_handler(mut self, handler: EventHook<U>) -> Self {
        self.event_handler = Some(handler);
        self
    }

    /// When true, a message from the bot's own account skips the chat-type,
    /// permission, check, and cooldown gates, poise's `skip_checks_for_owners`.
    pub fn skip_checks_for_owners(mut self, skip: bool) -> Self {
        self.skip_checks_for_owners = skip;
        self
    }

    /// When true, the dispatcher never starts or consults cooldowns.
    pub fn manual_cooldowns(mut self, manual: bool) -> Self {
        self.manual_cooldowns = manual;
        self
    }

    /// When false, a prefix followed by an unknown name is ignored.
    ///
    /// Defaults to true, so a mistyped command is answered with a hint.
    pub fn report_unknown_commands(mut self, report: bool) -> Self {
        self.report_unknown_commands = report;
        self
    }

    /// Adds an already-built [`Command`].
    pub fn command(mut self, command: Command<U>) -> Self {
        self.commands.push(command);
        self
    }

    /// Adds every command the value converts into: a `RegisteredCommand`, a
    /// `Command`, a `Vec` of either, or an array.
    pub fn commands(mut self, commands: impl IntoCommands<U>) -> Self {
        self.commands.extend(commands.into_commands());
        self
    }

    /// Adds a [`CommandGroup`], stamping its name onto any member that declares
    /// no group of its own, and keeping its description for help.
    pub fn add_group(mut self, group: CommandGroup<U>) -> Self {
        let (name, description, commands) = group.into_parts();
        if let Some(description) = description {
            self.group_descriptions.insert(name.clone(), description);
        }
        for mut command in commands {
            stamp_group(&mut command, &name);
            self.commands.push(command);
        }
        self
    }

    /// Adds several [`CommandGroup`]s, as [`add_group`](Self::add_group) does
    /// for one.
    pub fn groups(mut self, groups: impl IntoIterator<Item = CommandGroup<U>>) -> Self {
        for group in groups {
            self = self.add_group(group);
        }
        self
    }

    /// Produces the [`Framework`].
    pub fn build(self) -> Framework<U> {
        let mut commands = HashMap::with_capacity(self.commands.len() * 2);
        for command in self.commands {
            let command = Arc::new(command);
            for alias in &command.aliases {
                commands.insert(alias.to_ascii_lowercase(), Arc::clone(&command));
            }
        }

        Framework {
            prefixes: Arc::new(self.prefixes),
            registry: Arc::new(Registry {
                commands,
                group_descriptions: self.group_descriptions,
            }),
            on_error: self.on_error,
            pre_command: self.pre_command,
            post_command: self.post_command,
            command_check: self.command_check,
            reply_callback: self.reply_callback,
            event_handler: self.event_handler,
            skip_checks_for_owners: self.skip_checks_for_owners,
            manual_cooldowns: self.manual_cooldowns,
            report_unknown_commands: self.report_unknown_commands,
            user_data: Arc::new(self.user_data),
        }
    }
}

impl FrameworkBuilder<NoData> {
    /// A builder with the default prefix and no commands.
    pub fn new() -> Self {
        Self::fresh(UserDataCell::ready(Arc::new(NoData)))
    }
}

impl Default for FrameworkBuilder<NoData> {
    fn default() -> Self {
        Self::new()
    }
}

impl<U: FrameworkData> Framework<U> {
    /// A context carrying `data` for a message that has not yet been recognised
    /// as a command.
    fn context(&self, message: MessageContext, data: Arc<U>) -> Context<U> {
        Context {
            message: Arc::new(message),
            args: Args::default(),
            command: None,
            invoked_command_name: String::new(),
            commands: None,
            prefix: String::new(),
            reacted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_reply_id: Arc::new(std::sync::Mutex::new(None)),
            invocation_data: Arc::new(std::sync::Mutex::new(None)),
            pre_command: crate::command::noop_hook,
            post_command: crate::command::noop_hook,
            command_check: None,
            reply_callback: None,
            skip_checks_for_owners: false,
            manual_cooldowns: false,
            data: Some(data),
        }
    }
}

/// Injects a [`Framework`] into a WhatsApp [`BotBuilder`], the way poise is
/// handed to Serenity's client builder with `ClientBuilder::framework`.
///
/// ```no_run
/// use megumi::{Framework, FrameworkExt};
/// use whatsapp_rust::bot::Bot;
/// # async fn run(store: whatsapp_rust::store::SqliteStore) -> Result<(), Box<dyn std::error::Error>> {
/// let bot = Bot::builder()
///     .with_backend(store)
///     .framework(Framework::builder().build())
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// It registers a callback that hands each event to
/// [`Framework::dispatch_event`], so the client drives the framework rather than
/// the other way round. The callback is scoped to the kinds the framework
/// actually handles: message events always, and every other kind only when an
/// [`event_handler`](FrameworkBuilder::event_handler) was set. This is the
/// WhatsApp-shaped stand-in for Serenity's `ClientBuilder::framework`, which
/// whatsapp-rust's `BotBuilder` has no equivalent of.
pub trait FrameworkExt: Sized {
    /// Registers `framework` to handle the events it subscribes to.
    fn framework<U: FrameworkData + 'static>(self, framework: Framework<U>) -> Self;
}

impl<B, T, H, R> FrameworkExt for BotBuilder<B, T, H, R> {
    fn framework<U: FrameworkData + 'static>(self, framework: Framework<U>) -> Self {
        // Only an event handler wants every kind. A framework without one
        // subscribes to `Messages` alone, so whatsapp-rust never materializes
        // the events the framework would ignore — a large `HistorySync` blob,
        // a device-list update — which the all-kinds interest would force it to
        // build and hand over on every dispatch.
        let wants_every_kind = framework.event_handler.is_some();
        let handler = move |event: Arc<Event>, client: Arc<Client>| {
            let framework = framework.clone();
            async move {
                framework.dispatch_event(client, event).await;
            }
        };
        if wants_every_kind {
            self.on_event(handler)
        } else {
            self.on_event_for(&[EventKind::Messages], handler)
        }
    }
}

/// Stamps `name` onto `command` and every descendant that declares no group of
/// its own, so a group's header in help covers the subcommands reached through
/// it. Children the builder has already shared are left untouched.
fn stamp_group<U: FrameworkData>(command: &mut Command<U>, name: &str) {
    if command.group.is_none() {
        command.group = Some(name.to_string());
    }
    for child in &mut command.subcommands {
        if let Some(child) = Arc::get_mut(child) {
            stamp_group(child, name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::test_command;

    #[test]
    fn strip_prefix_tries_each_prefix() {
        let prefixes = ["!".into(), "?".into(), "megumi ".into()];
        assert_eq!(strip_prefix("!ping", &prefixes), Some(("!", "ping")));
        assert_eq!(strip_prefix("?ping", &prefixes), Some(("?", "ping")));
        assert_eq!(
            strip_prefix("megumi ping", &prefixes),
            Some(("megumi ", "ping"))
        );
        assert_eq!(strip_prefix(" ping", &prefixes), None);
        assert_eq!(strip_prefix("ping", &prefixes), None);
    }

    fn registry(commands: Vec<Command<NoData>>) -> Registry<NoData> {
        let mut by_alias = HashMap::new();
        for command in commands {
            let command = Arc::new(command);
            for alias in &command.aliases {
                by_alias.insert(alias.to_ascii_lowercase(), Arc::clone(&command));
            }
        }
        Registry {
            commands: by_alias,
            group_descriptions: HashMap::new(),
        }
    }

    #[test]
    fn classify_rejects_a_message_without_a_prefix() {
        let commands = registry(vec![test_command("ping")]);
        let prefixes = ["!".to_string()];
        assert!(matches!(
            classify("hello there", &prefixes, &commands),
            Route::NotACommand
        ));
        assert!(matches!(
            classify("!", &prefixes, &commands),
            Route::NotACommand
        ));
    }

    #[test]
    fn classify_reports_an_unknown_name_with_its_prefix() {
        let commands = registry(vec![test_command("ping")]);
        let prefixes = ["!".to_string()];
        match classify("!nope arg", &prefixes, &commands) {
            Route::Unknown { prefix, command } => {
                assert_eq!(prefix, "!");
                assert_eq!(command, "nope");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn classify_matches_aliases_case_insensitively() {
        let mut command = test_command("ping");
        command.aliases.push("p".into());
        let commands = registry(vec![command]);
        let prefixes = ["!".to_string()];
        for text in ["!ping", "!PING", "!p", "!P"] {
            match classify(text, &prefixes, &commands) {
                Route::Command { params, .. } => assert_eq!(params, ""),
                other => panic!("expected a command for {text}, got {other:?}"),
            }
        }
    }

    #[test]
    fn classify_walks_into_a_subcommand_and_reports_the_leaf() {
        let mut parent = test_command("group");
        parent.subcommands = vec![Arc::new(test_command("info"))];
        let commands = registry(vec![parent]);
        let prefixes = ["!".to_string()];

        match classify("!group info extra", &prefixes, &commands) {
            Route::Command {
                command,
                params,
                invoked_name,
                ..
            } => {
                assert_eq!(command.name, "info");
                assert_eq!(invoked_name, "info");
                assert_eq!(params, "extra");
            }
            other => panic!("expected a subcommand, got {other:?}"),
        }
    }

    #[test]
    fn classify_stops_the_walk_at_a_word_that_is_not_a_child() {
        let mut parent = test_command("group");
        parent.subcommands = vec![Arc::new(test_command("info"))];
        let commands = registry(vec![parent]);
        let prefixes = ["!".to_string()];

        match classify("!group subject New name", &prefixes, &commands) {
            Route::Command {
                command,
                params,
                invoked_name,
                ..
            } => {
                assert_eq!(command.name, "group");
                assert_eq!(invoked_name, "group");
                assert_eq!(params, "subject New name");
            }
            other => panic!("expected the parent, got {other:?}"),
        }
    }

    #[test]
    fn classify_reports_the_typed_alias_as_the_invoked_name() {
        let mut command = test_command("ping");
        command.aliases.push("p".into());
        let commands = registry(vec![command]);
        let prefixes = ["!".to_string()];

        match classify("!p", &prefixes, &commands) {
            Route::Command { invoked_name, .. } => assert_eq!(invoked_name, "p"),
            other => panic!("expected a command, got {other:?}"),
        }
    }
}
