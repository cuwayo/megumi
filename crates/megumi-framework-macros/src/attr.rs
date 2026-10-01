//! Reading the literal values handed to a macro attribute, and reporting the
//! duplicate ones.

use syn::{Expr, Lit};

pub(crate) fn expr_str(expr: &Expr, span: proc_macro2::Span, name: &str) -> syn::Result<String> {
    let Expr::Lit(expr_lit) = expr else {
        return Err(syn::Error::new(
            span,
            format!("`{name}` expects a string literal"),
        ));
    };
    let Lit::Str(value) = &expr_lit.lit else {
        return Err(syn::Error::new(
            span,
            format!("`{name}` expects a string literal"),
        ));
    };
    Ok(value.value())
}

pub(crate) fn set_once<T>(slot: &mut Option<T>, name: &str, value: T) -> syn::Result<()> {
    if slot.is_some() {
        return Err(duplicate(name));
    }
    *slot = Some(value);
    Ok(())
}

pub(crate) fn duplicate(name: &str) -> syn::Error {
    syn::Error::new(
        proc_macro2::Span::call_site(),
        format!("duplicate command attribute `{name}`"),
    )
}
