use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Instant;

use tracing::{debug, info, warn};
use whatsapp_rust::types::message::MessageSource;

use crate::context::{Context, FrameworkData};
use crate::cooldown::{CooldownConfig, CooldownTracker};
use crate::error::{CommandResult, FrameworkError};
use crate::framework::{BoxFuture, Check, ErrorHandler};
use crate::permission::{AdminStatus, Permission};

type CommandInvoke<U> =
    Box<dyn Fn(Context<U>) -> Pin<Box<dyn Future<Output = CommandResult> + Send>> + Send + Sync>;

/// The registered commands, keyed by every alias they answer to, plus the
/// descriptions of the [`CommandGroup`](crate::CommandGroup)s they were
/// registered in.
///
/// This is what the dispatcher looks a message up in and what help is rendered
/// from, so both travel behind one [`Arc`] and a `Context` shares it instead of
/// copying it.
pub(crate) struct Registry<U: FrameworkData = crate::context::NoData> {
    /// Command name and aliases (lowercased) to the command they name.
    pub(crate) commands: HashMap<String, Arc<Command<U>>>,
    /// Group name to the description declared on its `CommandGroup`.
    pub(crate) group_descriptions: HashMap<String, String>,
}

/// A registered command and the metadata the dispatcher checks before running it.
///
/// `U` is the user data the command's body, checks, and error handler all see
/// through [`Context::data`](crate::Context::data). It defaults to
/// [`NoData`](crate::NoData), so a command that shares nothing needs no type
/// argument.
pub struct Command<U: FrameworkData = crate::context::NoData> {
    /// The primary name, and the only one help lists.
    pub name: String,
    /// Alternate names the command answers to, all registered alongside `name`.
    pub aliases: Vec<String>,
    /// One-line summary shown next to the command in the help listing and in the
    /// header of `help <command>`.
    pub description: Option<String>,
    /// Longer help shown by `help <command>`, poise's `help_text`.
    pub help_text: Option<String>,
    /// The group this command is listed under in help.
    pub group: Option<String>,
    /// When true, the command refuses to run outside a group.
    pub guild_only: bool,
    /// When true, the command refuses to run inside a group.
    pub dm_only: bool,
    /// When true, the command is left out of the help listing but still runs.
    pub hide_in_help: bool,
    /// The emoji reacted to the invoking message while the command runs, taken
    /// off again when it finishes. See [`Context::react`](crate::Context::react).
    pub react: Option<String>,
    /// Who is allowed to run the command.
    pub permission: Permission,
    /// Nested commands, reached as `!parent child`. This is poise's
    /// `Command::subcommands`.
    pub subcommands: Vec<Arc<Command<U>>>,
    /// When true, invoking the parent without a child is an error.
    pub subcommand_required: bool,
    /// After the first reply, later [`Context::send`] calls edit it instead of
    /// posting another message. Poise's `reuse_response`.
    pub reuse_response: bool,
    /// If any of these returns `false` (or errors), the command does not run.
    pub checks: Vec<Check<U>>,
    /// Per-command override for the framework's `on_error`.
    pub on_error: Option<ErrorHandler<U>>,
    /// How long each cooldown bucket lasts.
    pub cooldown_config: CooldownConfig,
    /// When true, the dispatcher does not start or consult cooldowns.
    pub manual_cooldowns: bool,
    /// The parameters the command's help text lists.
    pub parameters: Vec<CommandParameter>,
    /// Handles command cooldowns. Mainly for framework internal use; public so
    /// a command with `manual_cooldowns` can drive them itself.
    pub cooldowns: Mutex<CooldownTracker>,
    invoke: CommandInvoke<U>,
}

/// A single parameter of a [`Command`], used to render help.
///
/// This is the prefix-command half of poise's `CommandParameter`.
#[derive(Clone, Debug)]
pub struct CommandParameter {
    /// The parameter's name, as written in the function signature.
    pub name: String,
    /// What the parameter is for, from `#[description = "..."]`.
    pub description: Option<String>,
    /// `true` if the user must supply this argument.
    pub required: bool,
    /// `true` if this parameter consumes the rest of the message.
    pub rest: bool,
    /// `true` if this is a `#[flag]` switch.
    pub flag: bool,
    /// The words a choice parameter accepts, as `(name, description)`.
    ///
    /// Empty unless the parameter's type implements
    /// [`ChoiceParameter`](crate::ChoiceParameter). `name` is the word the user
    /// types; `description` comes from the variant's `#[description]`.
    pub choices: Vec<(String, Option<String>)>,
}

impl<U: FrameworkData> Command<U> {
    /// Whether `alias` names this command, ignoring case.
    pub fn matches(&self, alias: &str) -> bool {
        self.aliases
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(alias))
    }

    /// The child matching `name`, if this command has one.
    pub fn find_subcommand(&self, name: &str) -> Option<&Arc<Command<U>>> {
        self.subcommands
            .iter()
            .find(|command| command.matches(name))
    }

    /// Checks the chat type, permission, checks, and cooldown, runs the command,
    /// and traces the outcome.
    ///
    /// Nothing is sent to the chat here: a rejection or a command failure is
    /// returned as a [`FrameworkError`] for the framework's error handler, the
    /// way poise leaves replying to `on_error`. A command body that panics is
    /// caught and returned as [`FrameworkError::CommandPanic`] rather than
    /// unwinding the dispatcher.
    #[tracing::instrument(
        name = "command",
        skip_all,
        fields(
            command = %self.name,
            chat = %ctx.message.info.source.chat.observe(),
            sender = %ctx.message.info.source.sender.observe(),
            group = ctx.message.info.source.is_group,
        )
    )]
    pub async fn invoke(&self, ctx: Context<U>) -> CommandResult {
        if let Err(error) = self.check_permissions_and_cooldown(&ctx).await {
            match &error {
                FrameworkError::GuildOnly => debug!("rejected: command is guild_only"),
                FrameworkError::DmOnly => debug!("rejected: command is dm_only"),
                FrameworkError::NotAnOwner => debug!("rejected: author is not the owner"),
                FrameworkError::MissingPermissions => {
                    debug!("rejected: author is not a group admin")
                }
                FrameworkError::PermissionFetchFailed(reason) => {
                    warn!(%reason, "could not read group admins")
                }
                FrameworkError::CommandCheckFailed { .. } => debug!("rejected: check failed"),
                FrameworkError::CooldownHit { remaining } => {
                    debug!(?remaining, "rejected: cooldown")
                }
                _ => {}
            }
            return Err(error);
        }

        // The reaction the command declared, shown for as long as its body runs
        // and taken off again below.
        if let Some(emoji) = &self.react {
            let _ = ctx.react(emoji.as_str()).await;
        }

        // The reaction is taken off even when a hook panics, so a progress
        // emoji never sticks. A panic in `pre_command` skips the body and
        // `post_command`; a panic in `post_command` still reports the body's
        // success, then becomes the error the dispatcher answers.
        let result = async {
            catch_unwind_maybe(async {
                (ctx.pre_command)(ctx.clone()).await;
                Ok(())
            })
            .await?;

            let started = Instant::now();
            let result = catch_unwind_maybe((self.invoke)(ctx.clone())).await;
            match &result {
                Ok(()) => {
                    info!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "command completed"
                    );
                    catch_unwind_maybe(async {
                        (ctx.post_command)(ctx.clone()).await;
                        Ok(())
                    })
                    .await?;
                }
                Err(error) => warn!(
                    elapsed_ms = started.elapsed().as_millis(),
                    %error,
                    "command failed"
                ),
            }
            result
        }
        .await;
        ctx.clear_reaction().await;
        result
    }

    /// The permission, check, and cooldown gates poise runs before the command
    /// body. Cooldowns are only consulted here; they start after arguments
    /// parse, so a mistyped invocation does not burn a use.
    async fn check_permissions_and_cooldown(&self, ctx: &Context<U>) -> CommandResult {
        let source = &ctx.message.info.source;
        if gate(self, source, ctx.skip_checks_for_owners)? {
            return Ok(());
        }

        if self.permission == Permission::GroupAdmin {
            match ctx.group_admin_status().await {
                AdminStatus::Admin => {}
                AdminStatus::NotAdmin => return Err(FrameworkError::MissingPermissions),
                AdminStatus::Unreadable(reason) => {
                    return Err(FrameworkError::PermissionFetchFailed(reason.into()));
                }
            }
        }

        if let Some(check) = ctx.command_check {
            match check(ctx.clone()).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(FrameworkError::CommandCheckFailed { error: None });
                }
                Err(error) => {
                    return Err(FrameworkError::CommandCheckFailed { error: Some(error) });
                }
            }
        }
        for check in &self.checks {
            match check(ctx.clone()).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(FrameworkError::CommandCheckFailed { error: None });
                }
                Err(error) => {
                    return Err(FrameworkError::CommandCheckFailed { error: Some(error) });
                }
            }
        }

        if !ctx.manual_cooldowns && !self.manual_cooldowns && !self.cooldown_config.is_empty() {
            let remaining = self
                .cooldowns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remaining_cooldown(ctx.cooldown_context(), &self.cooldown_config);
            if let Some(remaining) = remaining {
                return Err(FrameworkError::CooldownHit { remaining });
            }
        }

        Ok(())
    }

    /// Starts every cooldown bucket this command configured.
    ///
    /// Called by the code the `#[command]` macro generates, after the arguments
    /// parsed, so a mistyped invocation does not burn a use.
    pub fn start_cooldown(&self, ctx: &Context<U>) {
        if ctx.manual_cooldowns || self.manual_cooldowns || self.cooldown_config.is_empty() {
            return;
        }
        self.cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .start_cooldown(ctx.cooldown_context(), &self.cooldown_config);
    }
}

/// The gates that need no network access: the owner skip, the chat-type gate,
/// and [`Permission::Owner`].
///
/// Returns `Ok(true)` when the caller should skip every remaining gate — the
/// message is the bot's own and the framework's `skip_checks_for_owners` is set,
/// poise's behaviour of letting the owner bypass even the chat-type check.
/// Returns `Ok(false)` when the gates pass and dispatch continues, or the
/// rejection that stops it. Split out of
/// [`Command::check_permissions_and_cooldown`] so it can be tested without a
/// live WhatsApp session.
fn gate<U: FrameworkData>(
    command: &Command<U>,
    source: &MessageSource,
    skip_checks_for_owners: bool,
) -> Result<bool, FrameworkError> {
    if skip_checks_for_owners && source.is_from_me {
        return Ok(true);
    }
    if command.guild_only && !source.is_group {
        return Err(FrameworkError::GuildOnly);
    }
    if command.dm_only && source.is_group {
        return Err(FrameworkError::DmOnly);
    }
    if command.permission == Permission::Owner && !source.is_from_me {
        return Err(FrameworkError::NotAnOwner);
    }
    Ok(false)
}

/// Polls `future` to completion, turning a panic into
/// [`FrameworkError::CommandPanic`] instead of unwinding the dispatcher task.
///
/// Used for the command body and for `pre_command` / `post_command`, so a
/// panicking hook is reported the same way and cannot skip the reaction
/// cleanup. This is poise's `catch_unwind_maybe`, inlined because the
/// framework crate has no `futures` dependency.
async fn catch_unwind_maybe<F: Future<Output = CommandResult>>(future: F) -> CommandResult {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(result)) => Poll::Ready(result),
            Err(payload) => Poll::Ready(Err(FrameworkError::CommandPanic {
                payload: panic_payload(&payload),
            })),
        }
    })
    .await
}

/// Renders a panic payload as text, whether the panic carried a `&str` or a
/// `String`.
fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        String::from("the command panicked")
    }
}

/// Anything that can be turned into a [`Command`].
pub trait IntoCommand<U: FrameworkData = crate::context::NoData> {
    /// Converts `self` into a [`Command`].
    fn into_command(self) -> Command<U>;
}

/// Anything that can be turned into a list of [`Command`]s: a single command,
/// a `Vec`, or an array.
pub trait IntoCommands<U: FrameworkData = crate::context::NoData> {
    /// Converts `self` into the commands it holds.
    fn into_commands(self) -> Vec<Command<U>>;
}

pub mod __private {
    // Macro plumbing: the `#[command]` expansion names these fields, and no
    // user of the crate does. The public-facing equivalent is `Command`, whose
    // fields are documented.
    #![allow(missing_docs)]

    use std::sync::{Arc, Mutex};

    use crate::context::FrameworkData;
    use crate::cooldown::{CooldownConfig, CooldownTracker};
    use crate::framework::{Check, ErrorHandler};
    use crate::permission::Permission;

    use super::{CommandInvoke, CommandParameter};

    /// What the `#[command]` macro expands into, before it becomes a
    /// [`Command`](crate::Command).
    ///
    /// `U` is projected from the command function's context parameter, so the
    /// generated function stays monomorphic: it returns
    /// `RegisteredCommand<<Context as _GetGenerics>::U>`.
    pub struct RegisteredCommand<U: FrameworkData = crate::context::NoData> {
        pub name: String,
        pub aliases: Vec<String>,
        pub description: Option<String>,
        pub help_text: Option<String>,
        pub group: Option<String>,
        pub guild_only: bool,
        pub dm_only: bool,
        pub hide_in_help: bool,
        pub react: Option<String>,
        pub permission: Permission,
        pub subcommands: Vec<RegisteredCommand<U>>,
        pub subcommand_required: bool,
        pub reuse_response: bool,
        pub checks: Vec<Check<U>>,
        pub on_error: Option<ErrorHandler<U>>,
        pub cooldown_config: CooldownConfig,
        pub manual_cooldowns: bool,
        pub parameters: Vec<CommandParameter>,
        pub invoke: CommandInvoke<U>,
    }

    /// What a parent command passes down to its children, poise's command-tree
    /// walk: a parent's checks, group, chat-type gate, and permission all apply
    /// to the commands nested under it.
    struct Inherited<U: FrameworkData> {
        checks: Vec<Check<U>>,
        group: Option<String>,
        guild_only: bool,
        dm_only: bool,
        permission: Permission,
    }

    impl<U: FrameworkData> Clone for Inherited<U> {
        fn clone(&self) -> Self {
            Self {
                checks: self.checks.clone(),
                group: self.group.clone(),
                guild_only: self.guild_only,
                dm_only: self.dm_only,
                permission: self.permission,
            }
        }
    }

    impl<U: FrameworkData> Inherited<U> {
        fn root() -> Self {
            Self {
                checks: Vec::new(),
                group: None,
                guild_only: false,
                dm_only: false,
                permission: Permission::Everyone,
            }
        }

        /// The values a child of a command with this state inherits.
        fn for_child(&self) -> Self {
            Self {
                checks: self.checks.clone(),
                group: self.group.clone(),
                guild_only: self.guild_only,
                dm_only: self.dm_only,
                permission: self.permission,
            }
        }
    }

    impl<U: FrameworkData> RegisteredCommand<U> {
        /// Converts this command into a [`Command`](crate::Command) with no parent.
        pub fn into_command(self) -> super::Command<U> {
            self.into_command_with_parent(Inherited::root())
        }

        /// Converts this command, inheriting what its parent declares.
        ///
        /// A child inherits its parent's checks, group, chat-type gate, and
        /// permission, matching poise: the parent's `guild_only`, `dm_only`, and
        /// permission apply to the commands nested under it, so the bot does not
        /// have to repeat them on every child.
        fn into_command_with_parent(self, parent: Inherited<U>) -> super::Command<U> {
            let mut checks = parent.checks;
            checks.extend(self.checks);
            let group = self.group.or(parent.group);
            let guild_only = self.guild_only || parent.guild_only;
            let dm_only = self.dm_only || parent.dm_only;
            let permission = if self.permission == Permission::Everyone {
                parent.permission
            } else {
                self.permission
            };

            let inherited = Inherited {
                checks,
                group,
                guild_only,
                dm_only,
                permission,
            };
            let children = inherited.for_child();

            super::Command {
                name: self.name,
                aliases: self.aliases,
                description: self.description,
                help_text: self.help_text,
                group: inherited.group,
                guild_only: inherited.guild_only,
                dm_only: inherited.dm_only,
                hide_in_help: self.hide_in_help,
                react: self.react,
                permission: inherited.permission,
                subcommands: self
                    .subcommands
                    .into_iter()
                    .map(|command| Arc::new(command.into_command_with_parent(children.clone())))
                    .collect(),
                subcommand_required: self.subcommand_required,
                reuse_response: self.reuse_response,
                checks: inherited.checks,
                on_error: self.on_error,
                cooldown_config: self.cooldown_config,
                manual_cooldowns: self.manual_cooldowns,
                parameters: self.parameters,
                cooldowns: Mutex::new(CooldownTracker::new()),
                invoke: self.invoke,
            }
        }
    }
}

impl<U: FrameworkData> IntoCommand<U> for __private::RegisteredCommand<U> {
    fn into_command(self) -> Command<U> {
        self.into_command()
    }
}

impl<U: FrameworkData> IntoCommands<U> for __private::RegisteredCommand<U> {
    fn into_commands(self) -> Vec<Command<U>> {
        vec![self.into_command()]
    }
}

impl<U: FrameworkData> IntoCommand<U> for Command<U> {
    fn into_command(self) -> Command<U> {
        self
    }
}

impl<T, U> IntoCommands<U> for Vec<T>
where
    U: FrameworkData,
    T: IntoCommand<U>,
{
    fn into_commands(self) -> Vec<Command<U>> {
        self.into_iter().map(IntoCommand::into_command).collect()
    }
}

impl<T, U, const N: usize> IntoCommands<U> for [T; N]
where
    U: FrameworkData,
    T: IntoCommand<U>,
{
    fn into_commands(self) -> Vec<Command<U>> {
        self.into_iter().map(IntoCommand::into_command).collect()
    }
}

/// A no-op hook, used as the default `pre_command` / `post_command`.
pub fn noop_hook<U: FrameworkData>(_ctx: Context<U>) -> BoxFuture<()> {
    Box::pin(async {})
}

/// A bare command with no body, for the framework's own tests.
#[cfg(test)]
pub(crate) fn test_command(name: &str) -> Command<crate::context::NoData> {
    use crate::context::NoData;

    Command {
        name: name.to_string(),
        aliases: vec![name.to_string()],
        description: None,
        help_text: None,
        group: None,
        guild_only: false,
        dm_only: false,
        hide_in_help: false,
        react: None,
        permission: Permission::Everyone,
        subcommands: Vec::new(),
        subcommand_required: false,
        reuse_response: false,
        checks: Vec::new(),
        on_error: None,
        cooldown_config: CooldownConfig::default(),
        manual_cooldowns: false,
        parameters: Vec::new(),
        cooldowns: Mutex::new(CooldownTracker::new()),
        invoke: Box::new(|_ctx: Context<NoData>| Box::pin(async { Ok(()) })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(is_group: bool, is_from_me: bool) -> MessageSource {
        MessageSource {
            is_group,
            is_from_me,
            ..Default::default()
        }
    }

    #[test]
    fn guild_only_refuses_a_direct_message() {
        let mut command = test_command("grouped");
        command.guild_only = true;
        assert!(matches!(
            gate(&command, &source(false, false), false),
            Err(FrameworkError::GuildOnly)
        ));
        assert!(matches!(
            gate(&command, &source(true, false), false),
            Ok(false)
        ));
    }

    #[test]
    fn dm_only_refuses_a_group_message() {
        let mut command = test_command("direct");
        command.dm_only = true;
        assert!(matches!(
            gate(&command, &source(true, false), false),
            Err(FrameworkError::DmOnly)
        ));
        assert!(matches!(
            gate(&command, &source(false, false), false),
            Ok(false)
        ));
    }

    #[test]
    fn owner_permission_refuses_another_member() {
        let mut command = test_command("console");
        command.permission = Permission::Owner;
        assert!(matches!(
            gate(&command, &source(true, false), false),
            Err(FrameworkError::NotAnOwner)
        ));
        assert!(matches!(
            gate(&command, &source(true, true), false),
            Ok(false)
        ));
    }

    #[test]
    fn skip_checks_for_owners_bypasses_even_the_chat_type_gate() {
        let mut command = test_command("grouped");
        command.guild_only = true;
        // The bot's own message in a DM would be refused, unless the framework
        // was told to skip the owner's checks.
        assert!(gate(&command, &source(false, true), false).is_err());
        assert!(matches!(
            gate(&command, &source(false, true), true),
            Ok(true)
        ));
    }

    #[test]
    fn group_admin_permission_is_left_to_the_network_check() {
        // `gate` is pure, so it cannot decide `GroupAdmin`; it must pass the
        // message through to `check_permissions_and_cooldown`.
        let mut command = test_command("kick");
        command.permission = Permission::GroupAdmin;
        assert!(matches!(
            gate(&command, &source(true, false), false),
            Ok(false)
        ));
    }
}
