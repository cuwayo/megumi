//! Turning a command function's signature into argument bindings and the call
//! the generated closure makes.
//!
//! The bindings are a chain of nested parser functions, one per parameter. Each
//! parses its own parameter and delegates the remainder to the next, so a
//! parameter that has to look ahead — a `#[lazy]` `Option` or `Vec` — can retry
//! the tail with a shorter prefix. The chain is generated rather than
//! interpreted so every type stays static; the parser for parameter `i` returns
//! `(T_i, T_{i+1}, …, &str)`, the trailing `&str` being what is left over.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, Ident, ItemFn, Type};

/// How a parameter is read out of the argument string.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamKind {
    /// The command's `Context`.
    Context,
    /// The raw [`Args`] bag, for commands that parse by hand.
    Args,
    /// A typed parameter implementing `PopArgument`.
    Typed,
    /// A `#[rest]` parameter, consuming everything left.
    Rest,
    /// A `#[flag]` switch, true when its name was typed.
    Flag,
}

pub(crate) struct Param {
    pub ident: Ident,
    pub ty: Type,
    pub kind: ParamKind,
    /// `#[lazy]`: retry the following parameters with a shorter prefix. Only
    /// valid on `Option<T>` and `Vec<T>`.
    pub lazy: bool,
    pub name: String,
    pub description: Option<String>,
}

pub(crate) struct SignatureParams {
    pub params: Vec<Param>,
}

impl SignatureParams {
    pub(crate) fn parse(input: &ItemFn) -> syn::Result<Self> {
        if input.sig.inputs.is_empty() {
            return Err(syn::Error::new_spanned(
                &input.sig,
                "commands need a first `::megumi::Context` argument",
            ));
        }

        let mut params: Vec<Param> = Vec::new();
        for (index, arg) in input.sig.inputs.iter().enumerate() {
            let FnArg::Typed(pat_type) = arg else {
                return Err(syn::Error::new_spanned(arg, "unexpected `self` parameter"));
            };
            let syn::Pat::Ident(ident) = pat_type.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &pat_type.pat,
                    "command parameters must be named",
                ));
            };

            let mut kind = if index == 0 {
                ParamKind::Context
            } else {
                ParamKind::Typed
            };
            let mut description = None;
            let mut renamed = None;
            let mut rest = false;
            let mut flag = false;
            let mut lazy = false;

            for attr in &pat_type.attrs {
                let name = attr.path().get_ident().map(|ident| ident.to_string());
                match name.as_deref() {
                    Some("rest") => rest = true,
                    Some("flag") => flag = true,
                    Some("lazy") => lazy = true,
                    Some("description") => {
                        description = Some(attr_string(attr, "description")?);
                    }
                    Some("rename") => renamed = Some(attr_string(attr, "rename")?),
                    _ => {}
                }
            }

            // A bare `&str` straight after `Args` is the raw remainder, the
            // spelling the existing commands use. A typed parameter that wants
            // the same thing says `#[rest]`.
            let bare_rest = is_str_ref(&pat_type.ty)
                && params
                    .last()
                    .is_some_and(|previous| previous.kind == ParamKind::Args);
            if rest || bare_rest {
                kind = ParamKind::Rest;
            } else if flag {
                kind = ParamKind::Flag;
            } else if is_args(&pat_type.ty) {
                kind = ParamKind::Args;
            }

            // Modifiers are mutually exclusive, the way poise treats them; a
            // combination is nearly always a mistake rather than a request.
            let modifiers = [rest, flag, lazy];
            if modifiers.iter().filter(|set| **set).count() > 1 {
                return Err(syn::Error::new_spanned(
                    &pat_type.ty,
                    "`#[rest]`, `#[flag]` and `#[lazy]` cannot be combined",
                ));
            }
            if lazy
                && unwrap_generic(&pat_type.ty, "Option").is_none()
                && unwrap_generic(&pat_type.ty, "Vec").is_none()
            {
                return Err(syn::Error::new_spanned(
                    &pat_type.ty,
                    "`#[lazy]` only applies to `Option<T>` or `Vec<T>`",
                ));
            }
            if kind == ParamKind::Flag && !is_bool(&pat_type.ty) {
                return Err(syn::Error::new_spanned(
                    &pat_type.ty,
                    "a `#[flag]` parameter must be a `bool`",
                ));
            }
            if lazy && kind != ParamKind::Typed {
                return Err(syn::Error::new_spanned(
                    &pat_type.ty,
                    "`#[lazy]` cannot be combined with `#[rest]` or `#[flag]`",
                ));
            }

            params.push(Param {
                ident: ident.ident.clone(),
                ty: (*pat_type.ty).clone(),
                kind,
                lazy,
                name: renamed.unwrap_or_else(|| ident.ident.to_string()),
                description,
            });
        }

        if params[0].kind != ParamKind::Context {
            return Err(syn::Error::new_spanned(
                &input.sig.inputs[0],
                "the first command argument must be the context",
            ));
        }

        if let Some(rest_at) = params
            .iter()
            .rposition(|param| param.kind == ParamKind::Rest)
            && rest_at + 1 < params.len()
        {
            return Err(syn::Error::new_spanned(
                &input.sig,
                "a `#[rest]` parameter consumes the rest of the message, so it must be last",
            ));
        }

        Ok(Self { params })
    }

    /// Whether the signature has a parameter that takes a bounded number of
    /// words, so a leftover is worth reporting.
    ///
    /// A `#[flag]` is bounded the same way a typed parameter is: it takes at
    /// most one word. `Args` and `#[rest]` are unbounded, and a context-only
    /// command has no argument surface at all.
    fn has_bounded_parameter(&self) -> bool {
        self.params
            .iter()
            .any(|param| matches!(param.kind, ParamKind::Typed | ParamKind::Flag))
    }

    /// Whether a parameter consumes the whole remainder, so a word left over is
    /// its concern rather than `TooManyArguments`.
    ///
    /// `Args` hands the remainder to the body, and `#[rest]` consumes it. A
    /// `#[flag]` does neither: it takes one word only when that word is its
    /// name, so anything else is a genuine leftover and still an error.
    fn leaves_leftovers(&self) -> bool {
        self.params
            .iter()
            .any(|param| matches!(param.kind, ParamKind::Args | ParamKind::Rest))
    }
}

fn attr_string(attr: &syn::Attribute, name: &str) -> syn::Result<String> {
    let text = match &attr.meta {
        syn::Meta::NameValue(value) => match &value.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(text),
                ..
            }) => text.value(),
            _ => {
                return Err(syn::Error::new_spanned(
                    attr,
                    format!("`#[{name}]` expects a string"),
                ));
            }
        },
        syn::Meta::List(_) => attr.parse_args::<syn::LitStr>()?.value(),
        syn::Meta::Path(_) => {
            return Err(syn::Error::new_spanned(
                attr,
                format!("`#[{name}]` expects a string"),
            ));
        }
    };
    if text.is_empty() {
        return Err(syn::Error::new_spanned(
            attr,
            format!("`#[{name}]` needs a non-empty string"),
        ));
    }
    Ok(text)
}

fn is_args(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Args")
}

fn is_str_ref(ty: &Type) -> bool {
    let Type::Reference(reference) = ty else {
        return false;
    };
    matches!(&*reference.elem, Type::Path(path) if path.path.is_ident("str"))
}

fn is_bool(ty: &Type) -> bool {
    matches!(ty, Type::Path(path) if path.path.is_ident("bool"))
}

/// The `T` of an `Option<T>` / `Vec<T>` written as the last path segment.
fn unwrap_generic<'a>(ty: &'a Type, name: &str) -> Option<&'a Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != name {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

/// The identifier a parser binds its parameter's value to.
///
/// A raw ident keeps the parameter's span, which is what error messages point
/// at, but a leading underscore would make the generated name trip the snake
/// case lint (`_args` becomes `__megumi_value__args`). The span is kept; the
/// underscores are not.
fn value_ident(param: &Param) -> Ident {
    let stem = param.ident.to_string().trim_start_matches('_').to_string();
    format_ident!("__megumi_value_{stem}", span = param.ident.span())
}

/// The Rust type a parser reports for a parameter.
fn parser_value_type(param: &Param) -> Type {
    match param.kind {
        ParamKind::Context => param.ty.clone(),
        ParamKind::Args => syn::parse_quote!(::megumi::Args),
        ParamKind::Flag => syn::parse_quote!(bool),
        // A borrowed rest borrows the parser's argument, so it names the
        // lifetime the generated parser declares.
        ParamKind::Rest if is_str_ref(&param.ty) => syn::parse_quote!(&'__megumi_a str),
        _ => param.ty.clone(),
    }
}

/// The parser the command's body uses, returned as a function item so the same
/// expansion can be called on its own.
///
/// The returned function takes the context and the raw argument text and yields
/// the parameter tuple, or the [`FrameworkError`](megumi::FrameworkError) a
/// failed parse becomes. `discard_spare_arguments` drops words left after the
/// last parameter instead of reporting them.
pub(crate) fn build_parser(
    params: &SignatureParams,
    ctx_ty: &Type,
    discard_spare_arguments: bool,
) -> (Ident, TokenStream, TokenStream) {
    // The leftover check lives inside the terminal parser, not after the chain:
    // a `#[lazy]` parameter treats any `Ok` from its tail as "the tail is
    // happy", so a word left over has to fail the tail itself or the lazy
    // parameter never consumes anything.
    let check_leftovers =
        !discard_spare_arguments && params.has_bounded_parameter() && !params.leaves_leftovers();
    // The context is not parsed from the text, so it is left out of the chain
    // and bound from the live context at the call site. Keeping it out of the
    // result also means a failed parse's error can be formatted without a
    // context, which is what a test does.
    let parsed: Vec<&Param> = params
        .params
        .iter()
        .filter(|param| param.kind != ParamKind::Context)
        .collect();
    let (defs, types, first_parser) = parser_chain(&parsed, ctx_ty, check_leftovers);

    let parser = format_ident!("__megumi_parse_args");
    // A command with nothing to parse still returns the one-element tuple the
    // terminal parser produces, so the result type matches it.
    let output = if types.is_empty() {
        quote! { (&'__megumi_a str,) }
    } else {
        quote! { ( #(#types,)* &'__megumi_a str ) }
    };

    // Only the context parameter reads the context, and it merely clones it.
    // Every other parameter parses text, so the chain can run against a context
    // that was never built — which is what a test wants, since building one
    // needs a live WhatsApp client.
    let ctx_param = if params
        .params
        .first()
        .is_some_and(|param| param.kind == ParamKind::Context)
    {
        quote! { __megumi_ctx: ::std::option::Option<&'__megumi_b #ctx_ty>, }
    } else {
        quote! {}
    };

    let item = quote! {
        fn #parser<'__megumi_a, '__megumi_b>(
            #ctx_param
            __megumi_args: &'__megumi_a str,
        ) -> ::std::result::Result<
            #output,
            ::megumi::FrameworkError,
        > {
            type __MegumiParseError = (
                ::std::boxed::Box<dyn ::std::error::Error + ::std::marker::Send + ::std::marker::Sync>,
                ::std::option::Option<::std::string::String>,
            );

            #defs

            #first_parser(__megumi_ctx, __megumi_args).map_err(|(__megumi_error, __megumi_input)| {
                ::megumi::FrameworkError::argument_parse(__megumi_error, __megumi_input)
            })
        }
    };

    (parser, item, output)
}

/// The statements that bind each parameter, run before the command is called.
///
/// `ctx_ty` is the context type the command declared, needed to type the parser
/// functions. When `discard_spare_arguments` is set, words left after the last
/// parameter are dropped instead of reported.
pub(crate) fn build_bindings(
    params: &SignatureParams,
    ctx_ty: &Type,
    discard_spare_arguments: bool,
) -> TokenStream {
    // The leftover check lives inside the terminal parser, not after the chain:
    // a `#[lazy]` parameter treats any `Ok` from its tail as "the tail is
    // happy", so a word left over has to fail the tail itself or the lazy
    // parameter never consumes anything.
    let param_idents: Vec<&Ident> = params
        .params
        .iter()
        .filter(|param| param.kind != ParamKind::Context)
        .map(|param| &param.ident)
        .collect();
    let (parser, parser_item, _) = build_parser(params, ctx_ty, discard_spare_arguments);

    let context = params
        .params
        .iter()
        .find(|param| param.kind == ParamKind::Context)
        .map(|param| {
            let ident = &param.ident;
            quote! { let #ident = __megumi_ctx.clone(); }
        });

    let mut bindings = quote! {
        #parser_item

        let __megumi_args: &str = __megumi_ctx.args.raw();
        let ( #(#param_idents,)* _ ) = #parser(::std::option::Option::Some(&__megumi_ctx), __megumi_args)?;
        #context
    };

    // Cooldowns start once the arguments parsed, so a mistyped invocation does
    // not consume one. This is where poise starts them too.
    bindings.extend(quote! {
        if let ::std::option::Option::Some(__megumi_command) = __megumi_ctx.command() {
            __megumi_command.start_cooldown(&__megumi_ctx);
        }
    });

    bindings
}

/// Builds the nested parser functions for `params`, returning the definitions,
/// the value types they produce in order, and the name of the first parser.
fn parser_chain(
    params: &[&Param],
    ctx_ty: &Type,
    check_leftovers: bool,
) -> (TokenStream, Vec<Type>, Ident) {
    let Some((first, rest)) = params.split_first() else {
        let name = format_ident!("__megumi_parser_terminal");
        let leftover_check = check_leftovers.then(|| {
            quote! {
                if !__megumi_args.trim().is_empty() {
                    return ::std::result::Result::Err((
                        ::megumi::TooManyArguments::default().into(),
                        ::std::option::Option::Some(__megumi_args.trim().to_string()),
                    ));
                }
            }
        });
        let def = quote! {
            fn #name<'__megumi_a, '__megumi_b>(
                __megumi_ctx: ::std::option::Option<&'__megumi_b #ctx_ty>,
                __megumi_args: &'__megumi_a str,
            ) -> ::std::result::Result<(&'__megumi_a str,), __MegumiParseError> {
                let _ = __megumi_ctx;
                #leftover_check
                ::std::result::Result::Ok((__megumi_args,))
            }
        };
        return (def, Vec::new(), name);
    };

    let (rest_defs, rest_types, rest_parser) = parser_chain(rest, ctx_ty, check_leftovers);
    let stem = first.ident.to_string().trim_start_matches('_').to_string();
    let parser_name = format_ident!("__megumi_parser_{stem}", span = first.ident.span());
    let value = value_ident(first);
    let value_ty = parser_value_type(first);

    // The tail of every tuple: the following parameters' values, then whatever
    // text is left. `destructure` is a pattern for the tail parser's result;
    // `rebuild` is the same values as flat tuple elements for this parser's own
    // result, so the tuple never nests.
    let suffix_idents: Vec<Ident> = rest.iter().copied().map(value_ident).collect();
    let destructure = quote! { ( #(#suffix_idents,)* __megumi_leftover, ) };
    let rebuild = quote! { #(#suffix_idents,)* __megumi_leftover };

    let body = parser_body(first, &value, &destructure, &rebuild, &rest_parser);

    let def = quote! {
        fn #parser_name<'__megumi_a, '__megumi_b>(
            __megumi_ctx: ::std::option::Option<&'__megumi_b #ctx_ty>,
            __megumi_args: &'__megumi_a str,
        ) -> ::std::result::Result<
            ( #value_ty, #(#rest_types,)* &'__megumi_a str ),
            __MegumiParseError,
        > {
            #rest_defs
            #body
        }
    };

    let mut types = vec![value_ty];
    types.extend(rest_types);
    (def, types, parser_name)
}

/// One parser function's body: parse this parameter, then hand the remainder to
/// the next parser and assemble the tuple.
fn parser_body(
    param: &Param,
    value: &Ident,
    destructure: &TokenStream,
    rebuild: &TokenStream,
    rest_parser: &Ident,
) -> TokenStream {
    let name = &param.name;

    match param.kind {
        // The context is never part of the parser chain; it is bound at the call
        // site from the live context.
        ParamKind::Context => unreachable!("the context parameter is not parsed"),
        // The `Args` bag does not consume anything: it is the same remainder,
        // offered a second way.
        ParamKind::Args => quote! {
            let #value = ::megumi::Args::from_rest(__megumi_args);
            let #destructure = #rest_parser(__megumi_ctx, __megumi_args)?;
            ::std::result::Result::Ok((#value, #rebuild))
        },
        ParamKind::Flag => quote! {
            let __megumi_trimmed = __megumi_args.trim_start();
            let __megumi_word = __megumi_trimmed.split_whitespace().next().unwrap_or("");
            let #value = __megumi_word.eq_ignore_ascii_case(#name);
            let __megumi_args = if #value {
                __megumi_trimmed[__megumi_word.len()..].trim_start()
            } else {
                __megumi_trimmed
            };
            let #destructure = #rest_parser(__megumi_ctx, __megumi_args)?;
            ::std::result::Result::Ok((#value, #rebuild))
        },
        // `#[rest]` means "the remainder as one value". `String` and `&str`
        // take it verbatim, including when it is empty; anything else is parsed
        // from the whole remainder, then the remainder is spent, so a leftover
        // word cannot slip past the spare-argument check that `#[rest]` skips.
        ParamKind::Rest if is_str_ref(&param.ty) => quote! {
            let #value: &str = __megumi_args.trim_start();
            let #destructure = #rest_parser(__megumi_ctx, "")?;
            ::std::result::Result::Ok((#value, #rebuild))
        },
        ParamKind::Rest if is_string(&param.ty) => quote! {
            let #value = __megumi_args.trim_start().to_string();
            let #destructure = #rest_parser(__megumi_ctx, "")?;
            ::std::result::Result::Ok((#value, #rebuild))
        },
        ParamKind::Rest => {
            let ty = &param.ty;
            quote! {
                let (#value, __megumi_unused) =
                    match <#ty as ::megumi::PopArgument>::pop_from(__megumi_args.trim_start()) {
                        ::std::result::Result::Ok((__megumi_rest, __megumi_value)) => (__megumi_value, __megumi_rest),
                        ::std::result::Result::Err((__megumi_error, __megumi_input)) =>
                            return ::std::result::Result::Err((__megumi_error, __megumi_input)),
                    };
                if !__megumi_unused.trim().is_empty() {
                    return ::std::result::Result::Err((
                        ::megumi::TooManyArguments::default().into(),
                        ::std::option::Option::Some(__megumi_unused.trim().to_string()),
                    ));
                }
                let #destructure = #rest_parser(__megumi_ctx, "")?;
                ::std::result::Result::Ok((#value, #rebuild))
            }
        }
        ParamKind::Typed if param.lazy => lazy_body(param, destructure, rebuild, rest_parser),
        ParamKind::Typed => {
            let ty = &param.ty;
            quote! {
                let (#value, __megumi_args) = match <#ty as ::megumi::PopArgument>::pop_from(__megumi_args.trim_start()) {
                    ::std::result::Result::Ok((__megumi_rest, __megumi_value)) => (__megumi_value, __megumi_rest),
                    ::std::result::Result::Err((__megumi_error, __megumi_input)) =>
                        return ::std::result::Result::Err((__megumi_error, __megumi_input)),
                };
                let #destructure = #rest_parser(__megumi_ctx, __megumi_args)?;
                ::std::result::Result::Ok((#value, #rebuild))
            }
        }
    }
}

/// A `#[lazy]` parameter: try the tail first, and only take a word once the
/// tail cannot do without it. This is poise's laziness, adapted to a
/// synchronous `PopArgument`.
fn lazy_body(
    param: &Param,
    destructure: &TokenStream,
    rebuild: &TokenStream,
    rest_parser: &Ident,
) -> TokenStream {
    if let Some(inner) = unwrap_generic(&param.ty, "Option") {
        // `#[lazy] Option<T>`: `None` if the tail parses without a word, else
        // `Some` of one word.
        quote! {
            ::std::result::Result::Ok(match #rest_parser(__megumi_ctx, __megumi_args) {
                ::std::result::Result::Ok(#destructure) => (::std::option::Option::None, #rebuild),
                ::std::result::Result::Err(_) => {
                    let (__megumi_parsed, __megumi_args) =
                        match <#inner as ::megumi::PopArgument>::pop_from(__megumi_args.trim_start()) {
                            ::std::result::Result::Ok((__megumi_rest, __megumi_value)) => (__megumi_value, __megumi_rest),
                            ::std::result::Result::Err((__megumi_error, __megumi_input)) =>
                                return ::std::result::Result::Err((__megumi_error, __megumi_input)),
                        };
                    let #destructure = #rest_parser(__megumi_ctx, __megumi_args)?;
                    (::std::option::Option::Some(__megumi_parsed), #rebuild)
                }
            })
        }
    } else {
        // `#[lazy] Vec<T>`: the shortest list the tail accepts, starting with
        // the empty one.
        let inner =
            unwrap_generic(&param.ty, "Vec").expect("validated in `SignatureParams::parse`");
        quote! {
            {
                let mut __megumi_values = ::std::vec::Vec::new();
                let mut __megumi_args = __megumi_args;
                let mut __megumi_error: ::std::option::Option<__MegumiParseError> =
                    ::std::option::Option::None;
                loop {
                    match #rest_parser(__megumi_ctx, __megumi_args) {
                        ::std::result::Result::Ok(#destructure) => {
                            return ::std::result::Result::Ok((__megumi_values, #rebuild));
                        }
                        ::std::result::Result::Err(__megumi_tail_error) => {
                            __megumi_error = ::std::option::Option::Some(__megumi_tail_error);
                        }
                    }
                    match <#inner as ::megumi::PopArgument>::pop_from(__megumi_args.trim_start()) {
                        // A success that consumes nothing would loop forever, so
                        // it counts as the element not being there.
                        ::std::result::Result::Ok((__megumi_rest, __megumi_value))
                            if __megumi_rest.len() < __megumi_args.trim_start().len() =>
                        {
                            __megumi_values.push(__megumi_value);
                            __megumi_args = __megumi_rest;
                        }
                        _ => {
                            return ::std::result::Result::Err(__megumi_error.unwrap_or_else(|| (
                                ::megumi::TooManyArguments::default().into(),
                                ::std::option::Option::None,
                            )));
                        }
                    }
                }
            }
        }
    }
}

fn is_string(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "String")
}

pub(crate) fn build_call(fn_name: &Ident, params: &SignatureParams) -> TokenStream {
    let args: Vec<&Ident> = params.params.iter().map(|param| &param.ident).collect();
    quote! { #fn_name(#(#args),*) }
}

/// The `CommandParameter` entries the command advertises in its help.
pub(crate) fn parameter_list(params: &SignatureParams) -> TokenStream {
    let entries = params.params.iter().filter_map(|param| {
        if !matches!(
            param.kind,
            ParamKind::Typed | ParamKind::Rest | ParamKind::Flag
        ) {
            return None;
        }
        let name = &param.name;
        let description = match &param.description {
            Some(description) => quote! { ::std::option::Option::Some(#description.to_string()) },
            None => quote! { ::std::option::Option::None },
        };
        let required = param.kind != ParamKind::Flag && !is_option(&param.ty);
        let rest = param.kind == ParamKind::Rest;
        let flag = param.kind == ParamKind::Flag;
        // A choice parameter resolves `choices` to the by-value method that
        // lists its variants. Every other type has only the `&self` fallback,
        // which lists nothing. The trait has to be in scope for the by-value
        // candidate to be visible.
        let ty = &param.ty;
        let choices = quote! {{
            use ::megumi::__private::ChoiceChoices as _;
            ::megumi::__private::ChoicesOf::<#ty>(::std::marker::PhantomData).choices()
        }};
        Some(quote! {
            ::megumi::CommandParameter {
                name: #name.to_string(),
                description: #description,
                required: #required,
                rest: #rest,
                flag: #flag,
                choices: #choices,
            }
        })
    });
    quote! { ::std::vec![#(#entries),*] }
}

fn is_option(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Option" || segment.ident == "Vec")
}
