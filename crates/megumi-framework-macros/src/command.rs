//! The `#[command]` attribute: parsing the attribute and expanding it into a
//! `::megumi::__private::RegisteredCommand` constructor.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Expr, Ident, ItemFn, LitInt, LitStr, ReturnType, Token,
    parse::{Parse, ParseStream},
    spanned::Spanned,
};

use crate::attr::{duplicate, expr_str, set_once};
use crate::signature::{SignatureParams, build_bindings, build_call, build_parser, parameter_list};

#[derive(Default)]
pub(crate) struct CommandArgs {
    name: Option<String>,
    aliases: Vec<String>,
    description: Option<String>,
    help_text: Option<String>,
    permission: Option<Expr>,
    group: Option<String>,
    react: Option<String>,
    subcommands: Vec<Ident>,
    checks: Vec<Expr>,
    on_error: Option<Expr>,
    global_cooldown: Option<u64>,
    user_cooldown: Option<u64>,
    guild_cooldown: Option<u64>,
    channel_cooldown: Option<u64>,
    member_cooldown: Option<u64>,
    guild_only: bool,
    dm_only: bool,
    hide_in_help: bool,
    subcommand_required: bool,
    reuse_response: bool,
    manual_cooldowns: bool,
    discard_spare_arguments: bool,
}

impl CommandArgs {
    fn push_arg(&mut self, arg: CommandArg) -> syn::Result<()> {
        match arg {
            CommandArg::Name(value) => set_once(&mut self.name, "name", value),
            CommandArg::Aliases(value) => {
                if !self.aliases.is_empty() {
                    return Err(duplicate("aliases"));
                }
                self.aliases = value;
                Ok(())
            }
            CommandArg::Description(value) => set_once(&mut self.description, "description", value),
            CommandArg::HelpText(value) => set_once(&mut self.help_text, "help_text", value),
            CommandArg::Permission(value) => set_once(&mut self.permission, "permission", value),
            CommandArg::Group(value) => set_once(&mut self.group, "group", value),
            CommandArg::React(value) => set_once(&mut self.react, "react", value),
            CommandArg::Subcommands(value) => {
                if !self.subcommands.is_empty() {
                    return Err(duplicate("subcommands"));
                }
                self.subcommands = value;
                Ok(())
            }
            CommandArg::Check(value) => {
                self.checks.push(value);
                Ok(())
            }
            CommandArg::OnError(value) => set_once(&mut self.on_error, "on_error", value),
            CommandArg::GlobalCooldown(value) => {
                set_once(&mut self.global_cooldown, "global_cooldown", value)
            }
            CommandArg::UserCooldown(value) => {
                set_once(&mut self.user_cooldown, "user_cooldown", value)
            }
            CommandArg::GuildCooldown(value) => {
                set_once(&mut self.guild_cooldown, "guild_cooldown", value)
            }
            CommandArg::ChannelCooldown(value) => {
                set_once(&mut self.channel_cooldown, "channel_cooldown", value)
            }
            CommandArg::MemberCooldown(value) => {
                set_once(&mut self.member_cooldown, "member_cooldown", value)
            }
            CommandArg::GuildOnly => set_flag(&mut self.guild_only, "guild_only"),
            CommandArg::DmOnly => set_flag(&mut self.dm_only, "dm_only"),
            CommandArg::HideInHelp => set_flag(&mut self.hide_in_help, "hide_in_help"),
            CommandArg::SubcommandRequired => {
                set_flag(&mut self.subcommand_required, "subcommand_required")
            }
            CommandArg::ReuseResponse => set_flag(&mut self.reuse_response, "reuse_response"),
            CommandArg::ManualCooldowns => set_flag(&mut self.manual_cooldowns, "manual_cooldowns"),
            CommandArg::DiscardSpareArguments => {
                set_flag(&mut self.discard_spare_arguments, "discard_spare_arguments")
            }
        }
    }
}

/// Bare flags, the poise spelling (`guild_only`, `dm_only`, `hide_in_help`).
enum CommandArg {
    Name(String),
    Aliases(Vec<String>),
    Description(String),
    HelpText(String),
    Permission(Expr),
    Group(String),
    React(String),
    Subcommands(Vec<Ident>),
    Check(Expr),
    OnError(Expr),
    GlobalCooldown(u64),
    UserCooldown(u64),
    GuildCooldown(u64),
    ChannelCooldown(u64),
    MemberCooldown(u64),
    GuildOnly,
    DmOnly,
    HideInHelp,
    SubcommandRequired,
    ReuseResponse,
    ManualCooldowns,
    DiscardSpareArguments,
}

struct PunctuatedArgs(pub syn::punctuated::Punctuated<CommandArg, Token![,]>);

impl Parse for PunctuatedArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Ok(Self(syn::punctuated::Punctuated::parse_terminated(input)?))
    }
}

impl Parse for CommandArg {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let key: Ident = input.parse()?;
        let key_name = key.to_string();

        // `aliases("a", "b")` matches poise. `subcommands` takes the child
        // constructors' idents (`subcommands(get, set)`), not string literals.
        if key_name == "aliases" {
            let content;
            syn::parenthesized!(content in input);
            let values = content
                .parse_terminated(|input: ParseStream<'_>| input.parse::<LitStr>(), Token![,])?
                .into_iter()
                .map(|value| value.value())
                .collect();
            return Ok(CommandArg::Aliases(values));
        }
        if key_name == "subcommands" {
            let content;
            syn::parenthesized!(content in input);
            let values = content
                .parse_terminated(|input: ParseStream<'_>| input.parse::<Ident>(), Token![,])?
                .into_iter()
                .collect();
            return Ok(CommandArg::Subcommands(values));
        }
        if key_name == "check" {
            // `check = "fn_name"` or `check = fn_name`, matching poise's string
            // path spelling and a plain ident.
            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                return Ok(CommandArg::Check(parse_path_or_str(input)?));
            }
        }

        if !input.peek(Token![=]) {
            return match key_name.as_str() {
                "guild_only" => Ok(CommandArg::GuildOnly),
                "dm_only" => Ok(CommandArg::DmOnly),
                "hide_in_help" => Ok(CommandArg::HideInHelp),
                "subcommand_required" => Ok(CommandArg::SubcommandRequired),
                "reuse_response" => Ok(CommandArg::ReuseResponse),
                "manual_cooldowns" => Ok(CommandArg::ManualCooldowns),
                "discard_spare_arguments" => Ok(CommandArg::DiscardSpareArguments),
                _ => Err(syn::Error::new(
                    key.span(),
                    format!("`{key_name}` needs a value, or is not a command attribute"),
                )),
            };
        }

        input.parse::<Token![=]>()?;

        match key_name.as_str() {
            "name" | "description" | "group" | "react" | "help_text" => {
                let value: Expr = input.parse()?;
                let span = value.span();
                let text = expr_str(&value, span, &key_name)?;
                Ok(match key_name.as_str() {
                    "name" => CommandArg::Name(text),
                    "description" => CommandArg::Description(text),
                    "react" => CommandArg::React(text),
                    "help_text" => CommandArg::HelpText(text),
                    _ => CommandArg::Group(text),
                })
            }
            "permission" => Ok(CommandArg::Permission(input.parse()?)),
            "on_error" => Ok(CommandArg::OnError(parse_path_or_str(input)?)),
            "check" => Ok(CommandArg::Check(parse_path_or_str(input)?)),
            "global_cooldown" => Ok(CommandArg::GlobalCooldown(parse_seconds(input)?)),
            "user_cooldown" => Ok(CommandArg::UserCooldown(parse_seconds(input)?)),
            "guild_cooldown" => Ok(CommandArg::GuildCooldown(parse_seconds(input)?)),
            "channel_cooldown" => Ok(CommandArg::ChannelCooldown(parse_seconds(input)?)),
            "member_cooldown" => Ok(CommandArg::MemberCooldown(parse_seconds(input)?)),
            _ => Err(syn::Error::new(
                key.span(),
                format!("unknown command attribute `{key_name}`"),
            )),
        }
    }
}

fn parse_path_or_str(input: ParseStream<'_>) -> syn::Result<Expr> {
    if input.peek(LitStr) {
        let value: LitStr = input.parse()?;
        syn::parse_str(&value.value())
    } else {
        input.parse()
    }
}

fn parse_seconds(input: ParseStream<'_>) -> syn::Result<u64> {
    let value: LitInt = input.parse()?;
    value.base10_parse()
}

fn set_flag(slot: &mut bool, name: &str) -> syn::Result<()> {
    if *slot {
        return Err(duplicate(name));
    }
    *slot = true;
    Ok(())
}

pub(crate) fn parse_command_args(attr: TokenStream) -> syn::Result<CommandArgs> {
    if attr.is_empty() {
        return Ok(CommandArgs::default());
    }

    let input: proc_macro2::TokenStream = attr.into();
    let args = syn::parse2::<PunctuatedArgs>(input)?.0;
    let mut out = CommandArgs::default();
    for arg in args {
        out.push_arg(arg)?;
    }
    Ok(out)
}

pub(crate) fn expand_command(
    input: ItemFn,
    mut args: CommandArgs,
) -> syn::Result<proc_macro2::TokenStream> {
    if input.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &input.sig,
            "command functions must be `async fn`",
        ));
    }

    // Every command returns a `Result`, so a failure always reaches the error
    // handler instead of being swallowed. This is poise's contract.
    if matches!(input.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &input.sig,
            "command functions must return `Result<(), ::megumi::Error>`",
        ));
    }

    // A `subcommand_required` parent is a table of contents, not a command, so
    // it must not try to read arguments of its own and must actually have some.
    if args.subcommand_required {
        if input.sig.inputs.len() > 1 {
            return Err(syn::Error::new_spanned(
                &input.sig,
                "`subcommand_required` cannot be combined with command parameters",
            ));
        }
        if args.subcommands.is_empty() {
            return Err(syn::Error::new_spanned(
                &input.sig,
                "`subcommand_required` needs at least one `subcommands(...)` entry",
            ));
        }
    }

    let params = SignatureParams::parse(&input)?;

    // `#[rest]` and `discard_spare_arguments` mean the same thing twice over:
    // one already keeps every leftover word, the other says not to mind them.
    if args.discard_spare_arguments
        && params
            .params
            .iter()
            .any(|param| param.kind == crate::signature::ParamKind::Rest)
    {
        return Err(syn::Error::new_spanned(
            &input.sig,
            "`discard_spare_arguments` cannot be combined with a `#[rest]` parameter",
        ));
    }

    // A doc comment describes the command unless the attribute said otherwise,
    // and an explicit attribute always wins. This is poise's precedence.
    let (doc_description, doc_help) = extract_help_from_doc_comments(&input.attrs);
    if args.description.is_none() {
        args.description = doc_description;
    }
    if args.help_text.is_none() {
        args.help_text = doc_help;
    }

    let fn_name = input.sig.ident.clone();
    let hidden_name = Ident::new(&format!("__megumi_command_{fn_name}"), fn_name.span());
    // The user data type is projected out of the declared context parameter, so
    // the generated function stays monomorphic. This is poise's `_GetGenerics`.
    let ctx_ty = context_type(&input)?;
    let name = args.name.take().unwrap_or_else(|| fn_name.to_string());
    let mut aliases = args.aliases.clone();
    if !aliases.iter().any(|alias| alias == &name) {
        aliases.push(name.clone());
    }

    let bindings = build_bindings(&params, &ctx_ty, args.discard_spare_arguments);
    let (_, parser_item, parser_output) =
        build_parser(&params, &ctx_ty, args.discard_spare_arguments);
    let call = build_call(&hidden_name, &params);
    let parameters = parameter_list(&params);
    let call_expression = quote! {
        match #call.await {
            Ok(()) => Ok(()),
            Err(error) => Err(::megumi::FrameworkError::command(error)),
        }
    };

    if args.guild_only && args.dm_only {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "`guild_only` and `dm_only` cannot be combined",
        ));
    }
    let guild_only = args.guild_only;
    let dm_only = args.dm_only;
    let hide_in_help = args.hide_in_help;
    let subcommand_required = args.subcommand_required;
    let reuse_response = args.reuse_response;
    let manual_cooldowns = args.manual_cooldowns;
    let cooldown_config = cooldown_expr(&args);
    let permission = args
        .permission
        .unwrap_or_else(|| syn::parse_quote!(::megumi::Permission::Everyone));
    let description_expr = option_string(args.description.as_ref());
    let help_text_expr = option_string(args.help_text.as_ref());
    let group_expr = option_string(args.group.as_ref());
    let react_expr = option_string(args.react.as_ref());
    let subcommands = &args.subcommands;
    let checks_expr = wrap_checks(&args.checks, &ctx_ty);
    let on_error_expr = match &args.on_error {
        Some(handler) => quote! {
            ::std::option::Option::Some({
                fn __megumi_on_error(
                    error: ::megumi::FrameworkError,
                    ctx: #ctx_ty,
                ) -> ::megumi::BoxFuture<()> {
                    ::std::boxed::Box::pin(#handler(error, ctx))
                }
                __megumi_on_error
            })
        },
        None => quote! { ::std::option::Option::None },
    };

    let registered = quote! {
        ::megumi::__private::RegisteredCommand {
            name: #name.to_string(),
            aliases: vec![#(#aliases.to_string()),*],
            description: #description_expr,
            help_text: #help_text_expr,
            group: #group_expr,
            guild_only: #guild_only,
            dm_only: #dm_only,
            hide_in_help: #hide_in_help,
            react: #react_expr,
            permission: #permission,
            subcommands: vec![#(#subcommands()),*],
            subcommand_required: #subcommand_required,
            reuse_response: #reuse_response,
            checks: #checks_expr,
            on_error: #on_error_expr,
            cooldown_config: #cooldown_config,
            manual_cooldowns: #manual_cooldowns,
            parameters: #parameters,
            invoke: Box::new(|__megumi_ctx| {
                Box::pin(async move {
                    #![allow(unused_variables, unused_mut, unused_assignments)]
                    #bindings
                    #call_expression
                })
            }),
        }
    };

    let mut hidden_fn = input;
    hidden_fn.sig.ident = hidden_name.clone();
    hidden_fn.vis = syn::Visibility::Inherited;
    hidden_fn.attrs.retain(|attr| {
        !attr.path().is_ident("command")
            && !attr.path().is_ident("group")
            && !attr.path().is_ident("allow")
    });
    for input in &mut hidden_fn.sig.inputs {
        if let syn::FnArg::Typed(pat_type) = input {
            pat_type.attrs.retain(|attr| {
                !attr.path().is_ident("rest")
                    && !attr.path().is_ident("flag")
                    && !attr.path().is_ident("lazy")
                    && !attr.path().is_ident("description")
                    && !attr.path().is_ident("rename")
            });
        }
    }

    let parser_name = Ident::new(&format!("__megumi_parse_{fn_name}"), fn_name.span());

    let expanded = quote! {
        #[doc(hidden)]
        #[allow(non_snake_case)]
        #hidden_fn

        /// The argument parser this command's body runs, exposed so a test can
        /// feed it raw text without a live WhatsApp session.
        #[doc(hidden)]
        pub fn #parser_name<'__megumi_a>(
            __megumi_args: &'__megumi_a str,
        ) -> ::std::result::Result<#parser_output, ::megumi::FrameworkError> {
            #parser_item
            __megumi_parse_args(::std::option::Option::None, __megumi_args)
        }

        #[allow(non_snake_case)]
        pub fn #fn_name() -> ::megumi::__private::RegisteredCommand<
            <#ctx_ty as ::megumi::_GetGenerics>::U,
        > {
            #registered
        }
    };

    Ok(expanded)
}

fn wrap_checks(checks: &[Expr], ctx_ty: &syn::Type) -> proc_macro2::TokenStream {
    let wrappers = checks.iter().enumerate().map(|(index, check)| {
        let ident = Ident::new(&format!("__megumi_check_{index}"), check.span());
        quote! {{
            fn #ident(
                ctx: #ctx_ty,
            ) -> ::megumi::BoxFuture<::std::result::Result<bool, ::megumi::Error>> {
                ::std::boxed::Box::pin(#check(ctx))
            }
            #ident
        }}
    });
    quote! { ::std::vec![#(#wrappers),*] }
}

/// The first parameter's type, which is the context the command declared.
fn context_type(input: &ItemFn) -> syn::Result<syn::Type> {
    match input.sig.inputs.first() {
        Some(syn::FnArg::Typed(pat_type)) => Ok((*pat_type.ty).clone()),
        _ => Err(syn::Error::new_spanned(
            &input.sig,
            "commands need a first `::megumi::Context` argument",
        )),
    }
}

/// Splits a function's `///` doc comment into `(description, help_text)`.
///
/// The first paragraph becomes the one-line description and the rest the help
/// text, the way rustdoc and poise both read it. A line-continuation backslash
/// joins lines, and blank lines separate paragraphs.
fn extract_help_from_doc_comments(attrs: &[syn::Attribute]) -> (Option<String>, Option<String>) {
    let mut doc_lines = String::new();
    for attr in attrs {
        if let syn::Meta::NameValue(doc_attr) = &attr.meta
            && doc_attr.path.is_ident("doc")
            && let syn::Expr::Lit(lit) = &doc_attr.value
            && let syn::Lit::Str(literal) = &lit.lit
        {
            // Trim each line the way rustdoc does, then rejoin.
            doc_lines.push_str(literal.value().trim());
            doc_lines.push('\n');
        }
    }

    let doc_lines = doc_lines.trim().replace("\\\n", "");
    let mut paragraphs = doc_lines.splitn(2, "\n\n").filter(|part| !part.is_empty());

    let description = paragraphs.next().map(|part| part.replace('\n', " "));
    let help_text = paragraphs.next().map(str::to_owned);
    (description, help_text)
}

fn option_string(value: Option<&String>) -> proc_macro2::TokenStream {
    if let Some(value) = value {
        quote! { ::std::option::Option::Some(#value.to_string()) }
    } else {
        quote! { ::std::option::Option::None }
    }
}

fn cooldown_expr(args: &CommandArgs) -> proc_macro2::TokenStream {
    let field = |seconds: Option<u64>| match seconds {
        Some(seconds) => quote! {
            ::std::option::Option::Some(::std::time::Duration::from_secs(#seconds))
        },
        None => quote! { ::std::option::Option::None },
    };
    let global = field(args.global_cooldown);
    let user = field(args.user_cooldown);
    let guild = field(args.guild_cooldown);
    let channel = field(args.channel_cooldown);
    let member = field(args.member_cooldown);
    quote! {
        ::megumi::CooldownConfig {
            global: #global,
            user: #user,
            guild: #guild,
            channel: #channel,
            member: #member,
        }
    }
}
