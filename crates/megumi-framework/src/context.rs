use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::debug;
use whatsapp_rust::bot::MessageContext;
use whatsapp_rust::prelude::MessageExt;
use whatsapp_rust::prelude::wa;
use whatsapp_rust::send::SendOptions;

use crate::args::Args;
use crate::command::noop_hook;
use crate::command::{Command, Registry};
use crate::cooldown::CooldownContext;
use crate::error::Error;
use crate::framework::{Check, Hook, ReplyCallback};
use crate::help;
use crate::media::Attachment;
use crate::permission::{AdminStatus, author_jids, is_admin_participant};
use crate::reply::CreateReply;

/// The default user data type, used by a framework that never called
/// [`FrameworkBuilder::setup`](crate::FrameworkBuilder::setup).
///
/// This is poise's `U` with a default: a command written against a bare
/// [`Context`] reads `Context<NoData>` and needs no annotation, while a bot that
/// wants shared state aliases `type Context = megumi::Context<Data>` and
/// supplies that `Data` once at build time.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoData;

/// The bound every user data type has to satisfy.
///
/// Blanket-implemented, so a command declares the data it wants simply by
/// naming it on its [`Context`]. The data is stored behind an [`Arc`], so it
/// does not itself have to be [`Clone`].
pub trait FrameworkData: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> FrameworkData for T {}

/// Projects the user data type out of a [`Context`].
///
/// The `#[command]` macro expands a command function to return
/// `RegisteredCommand<<Context as _GetGenerics>::U>`, so the data type stays
/// implicit in the function signature instead of becoming a type parameter the
/// caller would have to write. This is poise's `_GetGenerics`.
#[doc(hidden)]
pub trait _GetGenerics {
    type U;
}

impl<U: FrameworkData> _GetGenerics for Context<U> {
    type U = U;
}

/// The handle a command body runs with.
///
/// `U` is the bot's user data, poise's `U`. It defaults to [`NoData`], so a
/// command that does not share state writes `ctx: Context` exactly as before.
/// A bot that does share state defines one alias and uses it everywhere:
///
/// ```no_run
/// use std::sync::atomic::AtomicU64;
/// use megumi::{Context as MegumiContext, Error, Framework};
///
/// struct Data {
///     invocations: AtomicU64,
/// }
///
/// type Context = MegumiContext<Data>;
///
/// async fn ping(ctx: Context) -> Result<(), Error> {
///     ctx.data().invocations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
///     ctx.say("pong").await
/// }
///
/// fn framework() -> Framework<Data> {
///     Framework::builder()
///         .setup(|| Data { invocations: AtomicU64::new(0) })
///         .build()
/// }
/// ```
///
/// The context is cloned for each step of the invocation (each check, the
/// `pre_command` hook, the body, `post_command`), so its fields are cheap to
/// clone by construction: the message is shared behind an [`Arc`] and the
/// argument bag is refcounted. The chat and its metadata are read through
/// [`Context::message`], which derefs to the framework's `MessageContext`.
pub struct Context<U: FrameworkData = NoData> {
    /// The message the command was invoked with, and the client it arrived on.
    ///
    /// Shared behind an [`Arc`] so the several clones of a `Context` during one
    /// invocation do not each copy the message's metadata (the JIDs, the
    /// participant list, and so on).
    pub message: Arc<MessageContext>,
    /// The command's positional arguments.
    pub args: Args,
    /// The command that is running, once the dispatcher has resolved it.
    pub(crate) command: Option<Arc<Command<U>>>,
    /// The name the user typed, which may be an alias. For a subcommand this is
    /// the leaf name, matching poise's `invoked_command_name`.
    pub(crate) invoked_command_name: String,
    /// Every registered command, used to render help on demand.
    pub(crate) commands: Option<Arc<Registry<U>>>,
    /// The prefix this invocation used.
    pub(crate) prefix: String,
    /// Whether the command has a reaction on its invoking message right now,
    /// shared with the dispatcher so it can take it off again. See
    /// [`Context::react`].
    pub(crate) reacted: Arc<AtomicBool>,
    /// The id of the last message this invocation sent, when the command
    /// reuses its response.
    pub(crate) last_reply_id: Arc<Mutex<Option<String>>>,
    /// Per-invocation scratch space, poise's `invocation_data`.
    pub(crate) invocation_data: Arc<Mutex<Option<Box<dyn std::any::Any + Send + Sync>>>>,
    pub(crate) pre_command: Hook<U>,
    pub(crate) post_command: Hook<U>,
    pub(crate) command_check: Option<Check<U>>,
    /// Sees every reply before it is sent; `None` unless the framework set one.
    pub(crate) reply_callback: Option<ReplyCallback<U>>,
    pub(crate) skip_checks_for_owners: bool,
    pub(crate) manual_cooldowns: bool,
    /// The bot's user data, shared with every context the framework builds.
    /// `None` only for a context built outside the dispatcher, which has no
    /// data to offer.
    pub(crate) data: Option<Arc<U>>,
}

impl<U: FrameworkData> Clone for Context<U> {
    fn clone(&self) -> Self {
        Self {
            message: Arc::clone(&self.message),
            args: self.args.clone(),
            command: self.command.clone(),
            invoked_command_name: self.invoked_command_name.clone(),
            commands: self.commands.clone(),
            prefix: self.prefix.clone(),
            reacted: Arc::clone(&self.reacted),
            last_reply_id: Arc::clone(&self.last_reply_id),
            invocation_data: Arc::clone(&self.invocation_data),
            pre_command: self.pre_command,
            post_command: self.post_command,
            command_check: self.command_check,
            reply_callback: self.reply_callback,
            skip_checks_for_owners: self.skip_checks_for_owners,
            manual_cooldowns: self.manual_cooldowns,
            data: self.data.clone(),
        }
    }
}

impl From<MessageContext> for Context<NoData> {
    fn from(message: MessageContext) -> Self {
        Self {
            message: Arc::new(message),
            args: Args::default(),
            command: None,
            invoked_command_name: String::new(),
            commands: None,
            prefix: crate::DEFAULT_PREFIX.to_string(),
            reacted: Arc::new(AtomicBool::new(false)),
            last_reply_id: Arc::new(Mutex::new(None)),
            invocation_data: Arc::new(Mutex::new(None)),
            pre_command: noop_hook,
            post_command: noop_hook,
            command_check: None,
            reply_callback: None,
            skip_checks_for_owners: false,
            manual_cooldowns: false,
            data: None,
        }
    }
}

impl<U: FrameworkData> Context<U> {
    /// Sends `text` to this chat.
    ///
    /// Returns the framework-wide [`Error`], so a command can write
    /// `ctx.say("...").await?` or use the call as its tail expression.
    pub async fn say(&self, text: impl Into<String>) -> Result<(), Error> {
        self.send(CreateReply::new().content(text)).await
    }

    /// Like [`say`](Self::say), quoting the message the command replies to.
    pub async fn reply_quoting(&self, text: impl Into<String>) -> Result<(), Error> {
        self.send(CreateReply::new().content(text).reply(true))
            .await
    }

    /// Sends `reply`: the text it carries and the media it attaches.
    ///
    /// When the command set `reuse_response`, a later call edits the first
    /// reply instead of posting another message, matching poise. The
    /// `reply_callback` set on the framework sees the reply before it is sent,
    /// but not before it is edited.
    ///
    /// ```no_run
    /// use megumi::{Context, CreateAttachment, CreateReply, Error};
    ///
    /// async fn send_a_picture(ctx: Context, bytes: Vec<u8>) -> Result<(), Error> {
    ///     ctx.send(
    ///         CreateReply::new()
    ///             .content("here it is")
    ///             .attachment(CreateAttachment::image(bytes)),
    ///     )
    ///     .await
    /// }
    /// ```
    pub async fn send(&self, mut reply: CreateReply) -> Result<(), Error> {
        let existing_id = if self.reuses_response() {
            self.last_reply_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        } else {
            None
        };
        if let Some(id) = existing_id {
            return reply.edit(self, &id).await;
        }

        if let Some(callback) = self.reply_callback {
            reply = callback(self, reply);
        }
        let result = reply.send(self).await?;
        if self.reuses_response() {
            *self
                .last_reply_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result.message_id);
        }
        Ok(())
    }

    /// Sends a prepared stanza to this chat, carrying the same ephemeral timer
    /// [`say`](Self::say) applies.
    ///
    /// This is the escape hatch for a message shape
    /// [`CreateReply`] does not model, such as a sticker pack a command built
    /// itself. It bypasses the `reply_callback`, which only sees
    /// [`CreateReply`] values.
    pub async fn send_raw(&self, message: wa::Message) -> Result<(), Error> {
        self.send_raw_result(message).await.map(drop)
    }

    pub(crate) async fn send_raw_result(
        &self,
        message: wa::Message,
    ) -> Result<whatsapp_rust::send::SendResult, Error> {
        self.message
            .client
            .send_message_with_options(&self.message.info.source.chat, message, self.send_options())
            .await
            .map_err(Into::into)
    }

    fn reuses_response(&self) -> bool {
        self.command
            .as_ref()
            .is_some_and(|command| command.reuse_response)
    }

    /// Reacts to the message the command was invoked with.
    ///
    /// The framework takes this reaction off again when the command finishes,
    /// whether it succeeded or failed, so a progress indicator never sticks
    /// after an early return. Call it again as the command moves along and the
    /// last emoji is the one removed; `""` removes it early and leaves nothing
    /// for the framework to remove.
    ///
    /// For a reaction that is meant to stay, react through
    /// [`Context::message`] and its `MessageContext::react` instead.
    pub async fn react(&self, emoji: impl Into<String>) -> Result<(), Error> {
        let emoji = emoji.into();
        self.message.react(&emoji).await?;
        self.reacted.store(!emoji.is_empty(), Ordering::Relaxed);
        Ok(())
    }

    /// Takes off the reaction [`react`](Self::react) last put on, if any.
    pub(crate) async fn clear_reaction(&self) {
        if self.reacted.swap(false, Ordering::Relaxed)
            && let Err(error) = self.message.react("").await
        {
            debug!(%error, "could not remove the command's reaction");
        }
    }

    /// The media this message carries, or the media of the message it replies
    /// to when the message itself carries none.
    pub fn attachment(&self) -> Option<Attachment<'_>> {
        Attachment::from_message(self.message.message.get_base_message())
    }

    /// Downloads the bytes of `attachment` from WhatsApp's CDN.
    pub async fn download(&self, attachment: &Attachment<'_>) -> Result<Vec<u8>, Error> {
        self.message
            .client
            .download(attachment.downloadable())
            .await
            .map_err(Into::into)
    }

    /// The user data the framework was built with.
    ///
    /// This is poise's `Context::data`. It is a shared reference: mutate through
    /// interior mutability (`Mutex`, atomics) rather than expecting `&mut`. A
    /// context not built by the dispatcher — one converted straight from a
    /// [`MessageContext`] — carries no data, so this panics there. Every context
    /// a command, check, or hook receives does.
    ///
    /// # Panics
    ///
    /// Panics when the context was not produced by a framework, which is the
    /// only way a context ends up without data.
    pub fn data(&self) -> &U {
        self.data
            .as_deref()
            .expect("user data is only available on a context the framework dispatched")
    }

    /// The prefix this command was invoked with.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The command that is running.
    pub fn command(&self) -> Option<&Command<U>> {
        self.command.as_deref()
    }

    /// The name the user typed, which may be an alias of [`command`](Self::command).
    ///
    /// For a subcommand this is the leaf name (`info` in `!group info`), not the
    /// parent's, matching poise.
    pub fn invoked_command_name(&self) -> &str {
        &self.invoked_command_name
    }

    /// The rendered listing of every command, or `None` when the context was not
    /// produced by a framework.
    ///
    /// This is built on demand and is not cached, because only a handful of
    /// commands ever read it; building it for every dispatched message would be
    /// wasted work.
    pub fn help_text(&self) -> Option<String> {
        self.commands
            .as_ref()
            .map(|registry| help::help_text(registry, &self.prefix))
    }

    /// The help text for a single command, addressed by name, alias, or
    /// `parent child` path.
    pub fn command_help(&self, name: &str) -> Option<String> {
        self.commands
            .as_ref()
            .and_then(|registry| help::command_help(registry, name, &self.prefix))
    }

    /// Stores `data` for the rest of this invocation.
    ///
    /// The value is carried across the `pre_command` hook, checks, the command
    /// body, and `post_command`. This is poise's `set_invocation_data`.
    pub async fn set_invocation_data<T: Send + Sync + 'static>(&self, data: T) {
        *self
            .invocation_data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(data));
    }

    /// Reads the value [`set_invocation_data`](Self::set_invocation_data) stored,
    /// if it is of type `T`.
    pub async fn invocation_data<T>(&self) -> Option<T>
    where
        T: Send + Sync + Clone + 'static,
    {
        self.invocation_data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|data| data.downcast_ref::<T>())
            .cloned()
    }

    /// The addressing a cooldown decision is made with.
    pub fn cooldown_context(&self) -> CooldownContext {
        let source = &self.message.info.source;
        CooldownContext {
            user: source.sender.clone(),
            channel: source.chat.clone(),
            guild: source.is_group.then(|| source.chat.clone()),
        }
    }

    fn send_options(&self) -> SendOptions {
        let mut options = SendOptions::default();
        if let Some(expiration) = self.message.ephemeral_expiration {
            options = options.with_ephemeral_expiration(expiration);
        }
        options
    }

    /// Whether the author of the invoking message is an admin of the group it
    /// came from. `false` in a DM, and `false` when the admin list could not be
    /// read.
    pub async fn author_is_group_admin(&self) -> bool {
        matches!(self.group_admin_status().await, AdminStatus::Admin)
    }

    pub(crate) async fn group_admin_status(&self) -> AdminStatus {
        let source = &self.message.info.source;
        if !source.is_group {
            return AdminStatus::NotAdmin;
        }

        let metadata = match self
            .message
            .client
            .groups()
            .fetch_metadata(&source.chat)
            .await
        {
            Ok(metadata) => metadata,
            Err(error) => return AdminStatus::Unreadable(error.to_string()),
        };

        let authors = author_jids(source, self.message.client.pn(), self.message.client.lid());
        if is_admin_participant(&metadata.participants, &authors) {
            AdminStatus::Admin
        } else {
            AdminStatus::NotAdmin
        }
    }
}
