//! The `#[derive(ChoiceParameter)]` macro.
//!
//! A fieldless enum becomes a [`megumi::ChoiceParameter`]: each variant is one
//! choice, `#[name = "..."]` renames it (and further `#[name]`s are aliases),
//! and `#[description = "..."]` is the line help shows next to it.

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Attribute, Data, DeriveInput, Fields, LitStr};

pub(crate) fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new(
            input.ident.span(),
            "only enums can be choice parameters",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new(
            input.ident.span(),
            "a choice parameter needs at least one variant",
        ));
    }

    let mut variants = Vec::new();
    let mut names = Vec::new();
    let mut aliases = Vec::new();
    let mut descriptions = Vec::new();

    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new(
                variant.fields.span(),
                "choice parameter variants cannot have fields",
            ));
        }

        let mut variant_names = Vec::new();
        let mut description = None;
        for attr in &variant.attrs {
            match attr
                .path()
                .get_ident()
                .map(|ident| ident.to_string())
                .as_deref()
            {
                Some("name") => variant_names.push(attr_string(attr, "name")?),
                Some("description") => {
                    if description.is_some() {
                        return Err(syn::Error::new_spanned(
                            attr,
                            "duplicate `#[description]` on a choice variant",
                        ));
                    }
                    description = Some(attr_string(attr, "description")?);
                }
                _ => {}
            }
        }

        let (name, aliases_of) = match variant_names.split_first() {
            Some((name, rest)) => (name.clone(), rest.to_vec()),
            None => (variant.ident.to_string(), Vec::new()),
        };

        variants.push(variant.ident.clone());
        names.push(name);
        aliases.push(aliases_of);
        descriptions.push(description);
    }

    let enum_ident = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    let list_entries = names.iter().zip(&descriptions).map(|(name, description)| {
        let description = match description {
            Some(description) => quote! { ::std::option::Option::Some(#description) },
            None => quote! { ::std::option::Option::None },
        };
        quote! { (#name, #description) }
    });

    let from_name_arms =
        variants
            .iter()
            .zip(&names)
            .zip(&aliases)
            .map(|((variant, name), aliases)| {
                quote! {
                    if name.eq_ignore_ascii_case(#name)
                        #( || name.eq_ignore_ascii_case(#aliases) )*
                    {
                        return ::std::option::Option::Some(Self::#variant);
                    }
                }
            });

    let name_arms = variants.iter().zip(&names).map(|(variant, name)| {
        quote! { Self::#variant => #name, }
    });

    Ok(quote! {
        impl #impl_generics ::megumi::ChoiceParameter for #enum_ident #type_generics #where_clause {
            fn list() -> ::std::vec::Vec<(&'static str, ::std::option::Option<&'static str>)> {
                ::std::vec![#(#list_entries),*]
            }

            fn from_name(name: &str) -> ::std::option::Option<Self> {
                #(#from_name_arms)*
                ::std::option::Option::None
            }

            fn name(&self) -> &'static str {
                match self {
                    #(#name_arms)*
                }
            }
        }
    })
}

/// The string a `#[name]` or `#[description]` attribute carries.
fn attr_string(attr: &Attribute, name: &str) -> syn::Result<String> {
    let text = match &attr.meta {
        syn::Meta::NameValue(value) => match &value.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(text),
                ..
            }) => text.value(),
            _ => {
                return Err(syn::Error::new(
                    value.span(),
                    format!("`#[{name}]` expects a string literal"),
                ));
            }
        },
        syn::Meta::List(list) => syn::parse2::<LitStr>(list.tokens.clone())
            .map_err(|_| {
                syn::Error::new(list.span(), format!("`#[{name}]` expects a string literal"))
            })?
            .value(),
        syn::Meta::Path(_) => {
            return Err(syn::Error::new_spanned(
                attr,
                format!("`#[{name}]` expects a string literal"),
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
