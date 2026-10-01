//! Procedural macros for `megumi-framework`: [`command`] and [`group`].
//!
//! Module map:
//!
//! - `attr` - reading the literal values given to an attribute
//! - `command` - the `#[command]` expansion (parsing, bindings, registration)
//! - `group` - the `#[group]` expansion
//! - `signature` - mapping a command's parameters onto the dispatcher's call
//!
//! The entry points below stay in the crate root because `proc-macro` crates
//! cannot export them from anywhere else.

use proc_macro::TokenStream;
use syn::{DeriveInput, Item, parse_macro_input};

mod attr;
mod choice_parameter;
mod command;
mod group;
mod signature;

use crate::command::{expand_command, parse_command_args};
use crate::group::expand_group;

/// Turns a fieldless enum into a `megumi::ChoiceParameter`.
///
/// Each variant is one choice the user can type. `#[name = "..."]` replaces the
/// variant's name as the word that selects it, and a second `#[name]` is an
/// alias that selects it without being listed. `#[description = "..."]` is the
/// line `help <command>` shows next to the choice. A variant with no `#[name]`
/// is selected by its own name.
///
/// ```ignore
/// #[derive(megumi::ChoiceParameter)]
/// enum Audience {
///     #[name = "admins"]
///     #[description = "group admins only"]
///     Admins,
///     #[name = "all"]
///     #[name = "everyone"]
///     Everyone,
/// }
/// ```
#[proc_macro_derive(ChoiceParameter, attributes(name, description))]
pub fn choice_parameter(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match choice_parameter::expand(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Turns an `async fn` into a registered command.
///
/// The function keeps its name and becomes a zero-argument constructor the
/// framework builder registers, returning
/// `RegisteredCommand<U>` with `U` projected from the declared context
/// parameter. Its parameters are the arguments the dispatcher parses out of the
/// message; see the crate README for the full attribute list.
///
/// ```ignore
/// /// Replies with pong.
/// #[command(name = "ping", aliases("p"), react = "⌛")]
/// async fn ping(ctx: megumi::Context) -> Result<(), megumi::Error> {
///     ctx.say("pong").await
/// }
/// ```
#[proc_macro_attribute]
pub fn command(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::ItemFn);
    let args = match parse_command_args(attr) {
        Ok(args) => args,
        Err(error) => return error.into_compile_error().into(),
    };

    match expand_command(input, args) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Turns a module of `#[command]` functions into a `CommandGroup` constructor.
///
/// The attribute emits a function named after the module, returning a
/// `CommandGroup` holding the module's commands. The
/// module's name is the group name unless `name = "..."` overrides it, and
/// `description = "..."` is rendered next to the group's header in help.
///
/// Members are every `#[command]` in the module, or the constructors named in
/// `commands(...)`. When the members live elsewhere, give the context type with
/// `context = ...` so the group's user data can still be projected:
///
/// ```ignore
/// #[group(description = "Media conversion", context = crate::Context,
///         commands(crate::commands::sticker, crate::commands::shazam))]
/// pub mod media {}
/// ```
#[proc_macro_attribute]
pub fn group(attr: TokenStream, item: TokenStream) -> TokenStream {
    match parse_macro_input!(item as Item) {
        Item::Mod(module) => match expand_group(module, attr.into()) {
            Ok(tokens) => tokens.into(),
            Err(error) => error.into_compile_error().into(),
        },
        other => syn::Error::new_spanned(
            other,
            "`#[group]` applies to a module of `#[command]` functions",
        )
        .into_compile_error()
        .into(),
    }
}
