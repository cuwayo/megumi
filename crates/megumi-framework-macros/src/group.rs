//! The `#[group]` attribute: turning a module of `#[command]` functions into a
//! [`CommandGroup`](::megumi::CommandGroup) constructor.
//!
//! The attribute runs before the `#[command]` macros inside the module, so it
//! can see each member's original name and emit a constructor that calls the
//! public function each `#[command]` will expand into. The module itself is
//! re-emitted unchanged, so the commands stay reachable at their own paths.

use quote::quote;
use syn::{
    Expr, Ident, Item, ItemMod, Path, Token,
    parse::{Parse, ParseStream},
    spanned::Spanned,
};

use crate::attr::{expr_str, set_once};

pub(crate) fn expand_group(
    input: ItemMod,
    attr: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    let Some((_, items)) = &input.content else {
        return Err(syn::Error::new_spanned(
            &input,
            "`#[group]` needs a module with a body",
        ));
    };

    let args = parse_group_attrs(attr)?;
    let module_ident = input.ident.clone();
    let name = args.name.unwrap_or_else(|| module_ident.to_string());

    // The context type each member takes, so the group can project the bot's
    // user data out of it the way `#[command]` does. A module whose commands
    // disagree on the context type cannot be one group, which is correct.
    let contexts: Vec<(Ident, syn::Type)> = items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(function) if has_command_attribute(function) => {
                let ty = function.sig.inputs.first().and_then(|arg| match arg {
                    syn::FnArg::Typed(pat) => Some((*pat.ty).clone()),
                    _ => None,
                })?;
                Some((function.sig.ident.clone(), ty))
            }
            _ => None,
        })
        .collect();

    // An explicit list names the members; otherwise every `#[command]` in the
    // module is one.
    let members: Vec<Path> = match args.commands {
        Some(commands) => commands,
        None => contexts
            .iter()
            .map(|(ident, _)| syn::parse_quote!(#ident))
            .collect(),
    };

    if members.is_empty() {
        return Err(syn::Error::new_spanned(
            &input,
            "`#[group]` found no commands: add `#[command]` functions to the \
             module, or name them with `commands(...)`",
        ));
    }

    // The context type of the first member, used to project `U`.
    let ctx_ty = match args.context {
        Some(context) => context,
        None => members
            .first()
            .and_then(|member| member.segments.last())
            .and_then(|segment| {
                contexts
                    .iter()
                    .find(|(ident, _)| *ident == segment.ident)
                    .map(|(_, ty)| ty.clone())
            })
            .ok_or_else(|| {
                syn::Error::new_spanned(
                    &input,
                    "`#[group]` could not read the context type of its commands: \
                     give one with `context = ...`, or list local `#[command]` \
                     functions with `commands(...)`",
                )
            })?,
    };

    let description = args.description;
    let mut chain = quote! { ::megumi::CommandGroup::new(#name) };
    if let Some(description) = &description {
        chain = quote! { #chain.description(#description) };
    }
    let members = members.iter().map(|member| {
        // A single-segment path names a function in this module, reached
        // through the module the attribute was applied to; a qualified path
        // already points at the command, so it is called as written.
        if member.segments.len() == 1 {
            quote! { #module_ident::#member() }
        } else {
            quote! { #member() }
        }
    });
    let chain = quote! { #chain.commands([ #(#members),* ]) };

    let expanded = quote! {
        #input

        #[allow(non_snake_case)]
        pub fn #module_ident() -> ::megumi::CommandGroup<
            <#ctx_ty as ::megumi::_GetGenerics>::U,
        > {
            #chain
        }
    };
    Ok(expanded)
}

fn has_command_attribute(function: &syn::ItemFn) -> bool {
    function
        .attrs
        .iter()
        .any(|attr| attr.path().is_ident("command"))
}

#[derive(Default)]
struct GroupArgs {
    name: Option<String>,
    description: Option<String>,
    commands: Option<Vec<Path>>,
    context: Option<syn::Type>,
}

fn parse_group_attrs(attr: proc_macro2::TokenStream) -> syn::Result<GroupArgs> {
    if attr.is_empty() {
        return Ok(GroupArgs::default());
    }

    let args = syn::parse2::<PunctuatedGroupArgs>(attr)?.0;
    let mut out = GroupArgs::default();
    for arg in args {
        match arg {
            GroupArg::Name(value) => set_once(&mut out.name, "name", value)?,
            GroupArg::Description(value) => set_once(&mut out.description, "description", value)?,
            GroupArg::Context(value) => set_once(&mut out.context, "context", *value)?,
            GroupArg::Commands(value) => {
                if out.commands.is_some() {
                    return Err(syn::Error::new(
                        proc_macro2::Span::call_site(),
                        "`commands` may only be given once",
                    ));
                }
                out.commands = Some(value);
            }
        }
    }
    Ok(out)
}

struct PunctuatedGroupArgs(pub syn::punctuated::Punctuated<GroupArg, Token![,]>);

impl Parse for PunctuatedGroupArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Ok(Self(syn::punctuated::Punctuated::parse_terminated(input)?))
    }
}

enum GroupArg {
    Name(String),
    Description(String),
    Commands(Vec<Path>),
    Context(Box<syn::Type>),
}

impl Parse for GroupArg {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let key: Ident = input.parse()?;
        let key_name = key.to_string();

        if key_name == "commands" {
            let content;
            syn::parenthesized!(content in input);
            let paths = content
                .parse_terminated(Path::parse_mod_style, Token![,])?
                .into_iter()
                .collect();
            return Ok(GroupArg::Commands(paths));
        }

        input.parse::<Token![=]>()?;
        if key_name == "context" {
            return Ok(GroupArg::Context(Box::new(input.parse()?)));
        }
        let value: Expr = input.parse()?;
        let span = value.span();
        match key_name.as_str() {
            "name" => Ok(GroupArg::Name(expr_str(&value, span, &key_name)?)),
            "description" => Ok(GroupArg::Description(expr_str(&value, span, &key_name)?)),
            _ => Err(syn::Error::new(
                key.span(),
                format!("unknown group attribute `{key_name}`"),
            )),
        }
    }
}
