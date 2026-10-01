//! A small command framework on top of `whatsapp-rust`.
//!
//! Commands are `async fn`s registered with the [`command`] attribute and
//! dispatched from any message that starts with the configured prefix. The
//! repository README documents the attribute list and the wiring example.
//!
//! [`Context`], [`Command`], and [`Framework`] are generic over user data `U`
//! (Poise's `U`), defaulting to [`NoData`]. Call
//! [`FrameworkBuilder::setup`](crate::FrameworkBuilder::setup) to supply a
//! shared value every command reaches through [`Context::data`].
//!
//! Module map:
//!
//! - `args` - the positional arguments a command body receives
//! - `command` - the [`Command`] struct, its traits, and the macro plumbing
//! - `context` - the [`Context`] a command body runs with
//! - `cooldown` - per-command cooldowns
//! - `error` - [`Error`], [`FrameworkError`], and [`CommandResult`]
//! - `framework` - the [`Framework`] registry, its builder, and [`install`]
//! - `group` - the optional [`CommandGroup`] container
//! - `help` - rendered help text
//! - `media` - [`Attachment`] a command reads, [`CreateAttachment`] a reply sends
//! - `parse` - reading a command name and its arguments out of a message
//! - `permission` - [`Permission`] and the group-admin checks
//! - `prefix_argument` - parsing typed parameters out of a message, including [`ChoiceParameter`] enums
//! - `reply` - the [`CreateReply`] a command sends

#![warn(missing_docs)]

mod args;
mod command;
mod context;
mod cooldown;
mod error;
mod framework;
mod group;
mod help;
mod media;
mod parse;
mod permission;
mod prefix_argument;
mod reply;

pub use crate::args::Args;
pub use crate::command::{Command, CommandParameter, IntoCommand, IntoCommands};
pub use crate::context::{_GetGenerics, Context, FrameworkData, NoData};
pub use crate::cooldown::{
    CooldownConfig, CooldownContext, CooldownTracker, CooldownType, Cooldowns,
};
pub use crate::error::{CommandResult, Error, FrameworkError};
pub use crate::framework::{
    BoxFuture, Check, ErrorHandler, Framework, FrameworkBuilder, Hook, ReplyCallback,
    default_on_error, install,
};
pub use crate::group::CommandGroup;
pub use crate::media::{
    Attachment, CreateAttachment, MediaKind, MediaSpec, Uploaded, image_mimetype,
};
pub use crate::parse::{command_text, parse_args, parse_command_text};
pub use crate::permission::{Permission, author_jids, is_admin_participant, participant_matches};
pub use crate::prefix_argument::{
    ChoiceParameter, CodeBlock, CodeBlockError, InvalidBool, InvalidChoice, KeyValueArgs,
    PopArgument, PopArgumentResult, Rest, TooFewArguments, TooManyArguments, pop_from_str,
    pop_string,
};
pub use crate::reply::{CreateReply, LinkCard, LinkPreview};

pub use megumi_framework_macros::{ChoiceParameter, command, group};
pub use whatsapp_rust::bot::MessageContext;
pub use whatsapp_rust::prelude::MessageExt;
pub use whatsapp_rust::wacore_binary::JidExt;
pub use whatsapp_rust::{GroupParticipant, Jid, ParticipantType};

/// The prefix a [`FrameworkBuilder`] answers to until told otherwise.
pub const DEFAULT_PREFIX: &str = "!";

/// Implementation details the [`command`] and [`group`] macros expand against.
pub mod __private {
    pub use crate::command::__private::RegisteredCommand;

    /// Carries a parameter's type so the `#[command]` macro can ask what choices
    /// it advertises, without the parameter having a value.
    pub struct ChoicesOf<T>(pub std::marker::PhantomData<T>);

    impl<T> ChoicesOf<T> {
        /// No choices. This is the fallback for a type that is not a
        /// [`ChoiceParameter`](crate::ChoiceParameter).
        ///
        /// A choice type also implements [`ChoiceChoices::choices`], which takes
        /// `self` by value. Method resolution prefers a by-value candidate over
        /// a by-reference one, so a choice type never reaches this method and
        /// every other type can only reach it.
        pub fn choices(&self) -> Vec<(String, Option<String>)> {
            Vec::new()
        }
    }

    /// The choices of a [`ChoiceParameter`](crate::ChoiceParameter).
    pub trait ChoiceChoices {
        /// The words this type accepts, as `(name, description)`.
        fn choices(self) -> Vec<(String, Option<String>)>;
    }

    impl<T: crate::ChoiceParameter> ChoiceChoices for ChoicesOf<T> {
        fn choices(self) -> Vec<(String, Option<String>)> {
            T::list()
                .into_iter()
                .map(|(name, description)| (name.to_string(), description.map(str::to_string)))
                .collect()
        }
    }
}
