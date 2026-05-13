//! `#[platform_adapter]` — declarative boundary marker for modules that
//! are the legitimate entry point between an upper subsystem and a
//! platform crate (`tx-substrate`, `tx-reactor`).
//!
//! ```ignore
//! #[platform_adapter(
//!     platform = "substrate",
//!     domain   = "vfs",
//!     reason   = "translate substrate index publication into VFS \
//!                 parent/name binding semantics",
//! )]
//! pub mod binding_adapter {
//!     use crate::platform::substrate;
//!     // …wrapper fns that re-shape substrate calls into role-typed VFS
//!     //   surface…
//! }
//! ```
//!
//! What the attribute does:
//!
//! * Validates `platform ∈ {"substrate", "reactor"}`, `domain` non-empty,
//!   `reason` ≥ 12 chars (forcing a real justification, not "todo").
//! * Injects a single `pub const __PLATFORM_ADAPTER: &str = "…manifest…"`
//!   at the top of the module so the declaration is real code and shows
//!   up in `cargo doc`.
//! * Otherwise re-emits the module unchanged.
//!
//! The compiler can't *seal* an extern crate — any file in a consumer
//! crate can still write `use tx_substrate::…`. The enforcement lives in
//! `cargo xtask boundary-report` (and, eventually, `lint boundary`): raw
//! `tx_substrate::` calls inside a `#[platform_adapter(platform =
//! "substrate", …)]` module count as `inside_adapter` (legal);
//! everywhere else counts as `outside_adapter` and must burn down to
//! zero.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parse_macro_input, ItemMod, LitStr, Token};

const MIN_REASON_LEN: usize = 12;
const KNOWN_PLATFORMS: &[&str] = &["substrate", "reactor"];

struct AdapterArgs {
    platform: LitStr,
    domain: LitStr,
    reason: LitStr,
    apis: Vec<LitStr>,
}

impl Parse for AdapterArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut platform: Option<LitStr> = None;
        let mut domain: Option<LitStr> = None;
        let mut reason: Option<LitStr> = None;
        let mut apis: Vec<LitStr> = Vec::new();

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "platform" => platform = Some(parse_dup(&key, "platform", platform, input)?),
                "domain" => domain = Some(parse_dup(&key, "domain", domain, input)?),
                "reason" => reason = Some(parse_dup(&key, "reason", reason, input)?),
                "apis" => {
                    let content;
                    syn::bracketed!(content in input);
                    let list = Punctuated::<LitStr, Token![,]>::parse_terminated(&content)?;
                    apis = list.into_iter().collect();
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown key `{other}`; expected `platform`, `domain`, `reason`, or `apis`"
                        ),
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        let platform = platform.ok_or_else(|| {
            syn::Error::new(Span::call_site(), "missing required `platform = \"...\"`")
        })?;
        let domain = domain.ok_or_else(|| {
            syn::Error::new(Span::call_site(), "missing required `domain = \"...\"`")
        })?;
        let reason = reason.ok_or_else(|| {
            syn::Error::new(Span::call_site(), "missing required `reason = \"...\"`")
        })?;

        validate_platform(&platform)?;
        validate_domain(&domain)?;
        validate_reason(&reason)?;
        for api in &apis {
            validate_api(api)?;
        }

        Ok(AdapterArgs {
            platform,
            domain,
            reason,
            apis,
        })
    }
}

fn parse_dup(
    key_ident: &syn::Ident,
    key_name: &str,
    existing: Option<LitStr>,
    input: ParseStream,
) -> syn::Result<LitStr> {
    if existing.is_some() {
        return Err(syn::Error::new(
            key_ident.span(),
            format!("duplicate key `{key_name}`"),
        ));
    }
    input.parse::<LitStr>()
}

fn validate_platform(p: &LitStr) -> syn::Result<()> {
    let value = p.value();
    if KNOWN_PLATFORMS.contains(&value.as_str()) {
        return Ok(());
    }
    Err(syn::Error::new(
        p.span(),
        format!(
            "unknown platform `{value}`; expected one of {}",
            KNOWN_PLATFORMS.join(", ")
        ),
    ))
}

fn validate_domain(d: &LitStr) -> syn::Result<()> {
    let value = d.value();
    if value.is_empty() {
        return Err(syn::Error::new(d.span(), "`domain` must be non-empty"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(syn::Error::new(
            d.span(),
            format!("`domain = \"{value}\"` must be snake_case (a-z, 0-9, underscore)"),
        ));
    }
    Ok(())
}

fn validate_reason(r: &LitStr) -> syn::Result<()> {
    let value = r.value();
    if value.trim().len() < MIN_REASON_LEN {
        return Err(syn::Error::new(
            r.span(),
            format!(
                "`reason` must be at least {MIN_REASON_LEN} characters and describe *why* this adapter is the legitimate boundary"
            ),
        ));
    }
    Ok(())
}

fn validate_api(a: &LitStr) -> syn::Result<()> {
    let value = a.value();
    if value.is_empty() {
        return Err(syn::Error::new(
            a.span(),
            "`apis` entries must be non-empty (e.g. `\"index\"`, `\"epoch\"`)",
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(syn::Error::new(
            a.span(),
            format!(
                "`apis` entry `{value}` must be snake_case identifier (substrate sub-module name)"
            ),
        ));
    }
    Ok(())
}

fn manifest_string(args: &AdapterArgs) -> String {
    let mut parts = vec![
        format!("platform={}", args.platform.value()),
        format!("domain={}", args.domain.value()),
    ];
    if !args.apis.is_empty() {
        let joined = args
            .apis
            .iter()
            .map(LitStr::value)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("apis={joined}"));
    }
    parts.push(format!("reason={}", args.reason.value()));
    parts.join(";")
}

/// Mark a module as the legitimate adapter between an upper subsystem
/// and a platform crate. See crate-level docs for the contract.
#[proc_macro_attribute]
pub fn platform_adapter(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as AdapterArgs);
    let mut module = parse_macro_input!(item as ItemMod);

    let Some((brace, items)) = module.content.take() else {
        return syn::Error::new_spanned(
            &module.ident,
            "#[platform_adapter] requires an inline module `mod x { ... }`, not a declaration `mod x;`",
        )
        .to_compile_error()
        .into();
    };

    let manifest = manifest_string(&args);
    // Namespace the manifest constant by platform so the attribute can
    // be stacked on the same module when an adapter sits between a
    // subsystem and multiple platforms (e.g. wait-routing that wraps
    // both `tx-substrate::wake` and `tx-reactor::wait`). Two adapter
    // attributes for the same platform on the same module would still
    // collide, which is the intent — that's a duplicate declaration.
    let const_ident = syn::Ident::new(
        &format!(
            "__PLATFORM_ADAPTER_{}",
            args.platform.value().to_uppercase()
        ),
        Span::call_site(),
    );
    let manifest_const: syn::Item = syn::parse_quote! {
        #[doc(hidden)]
        pub const #const_ident: &str = #manifest;
    };

    let mut injected = Vec::with_capacity(items.len() + 1);
    injected.push(manifest_const);
    injected.extend(items);
    module.content = Some((brace, injected));

    quote!(#module).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_str;

    fn ok(input: &str) -> AdapterArgs {
        match parse_str::<AdapterArgs>(input) {
            Ok(args) => args,
            Err(err) => panic!("expected parse to succeed, got error: {err}"),
        }
    }

    fn err(input: &str) -> String {
        match parse_str::<AdapterArgs>(input) {
            Ok(_) => panic!("expected parse to fail for input: {input}"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn accepts_full_attribute() {
        let args = ok(
            "platform = \"substrate\", domain = \"vfs\", reason = \"translate index into VFS binding\"",
        );
        assert_eq!(args.platform.value(), "substrate");
        assert_eq!(args.domain.value(), "vfs");
        assert_eq!(args.reason.value(), "translate index into VFS binding");
        assert!(args.apis.is_empty());
    }

    #[test]
    fn accepts_apis_list() {
        let args = ok(
            "platform = \"substrate\", domain = \"vfs\", apis = [\"index\", \"epoch\"], reason = \"translate index into VFS binding\"",
        );
        assert_eq!(args.apis.len(), 2);
        assert_eq!(args.apis[0].value(), "index");
        assert_eq!(args.apis[1].value(), "epoch");
    }

    #[test]
    fn rejects_unknown_platform() {
        let msg =
            err("platform = \"hal\", domain = \"vfs\", reason = \"some sufficiently long reason\"");
        assert!(msg.contains("unknown platform"), "got: {msg}");
    }

    #[test]
    fn rejects_missing_platform() {
        let msg = err("domain = \"vfs\", reason = \"some sufficiently long reason\"");
        assert!(
            msg.contains("missing required") && msg.contains("platform"),
            "got: {msg}"
        );
    }

    #[test]
    fn rejects_short_reason() {
        let msg = err("platform = \"substrate\", domain = \"vfs\", reason = \"todo\"");
        assert!(msg.contains("at least"), "got: {msg}");
    }

    #[test]
    fn rejects_non_snake_case_domain() {
        let msg = err(
            "platform = \"substrate\", domain = \"VFS-Binding\", reason = \"some sufficiently long reason\"",
        );
        assert!(msg.contains("snake_case"), "got: {msg}");
    }

    #[test]
    fn rejects_duplicate_key() {
        let msg = err(
            "platform = \"substrate\", platform = \"reactor\", domain = \"vfs\", reason = \"some sufficiently long reason\"",
        );
        assert!(msg.contains("duplicate"), "got: {msg}");
    }

    #[test]
    fn rejects_unknown_key() {
        let msg = err(
            "platform = \"substrate\", domain = \"vfs\", reason = \"some sufficiently long reason\", foo = \"bar\"",
        );
        assert!(msg.contains("unknown key"), "got: {msg}");
    }

    #[test]
    fn manifest_includes_apis_when_present() {
        let args = ok(
            "platform = \"substrate\", domain = \"vfs\", apis = [\"index\"], reason = \"translate index into VFS binding\"",
        );
        let m = manifest_string(&args);
        assert!(m.contains("platform=substrate"));
        assert!(m.contains("domain=vfs"));
        assert!(m.contains("apis=index"));
        assert!(m.contains("reason=translate index into VFS binding"));
    }

    #[test]
    fn manifest_omits_apis_when_absent() {
        let args = ok(
            "platform = \"reactor\", domain = \"wait\", reason = \"bridge reactor wait sources to VFS layer\"",
        );
        let m = manifest_string(&args);
        assert!(!m.contains("apis="));
    }

    #[test]
    fn validates_apis_snake_case() {
        let msg = err(
            "platform = \"substrate\", domain = \"vfs\", apis = [\"IndexAPI\"], reason = \"translate index into VFS binding\"",
        );
        assert!(msg.contains("snake_case"), "got: {msg}");
    }
}
