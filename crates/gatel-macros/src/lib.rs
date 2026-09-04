//! `#[derive(FromNode)]`: generate a strict configuration decoder from a Rust
//! struct.
//!
//! The generated code only ever touches `gatel_model::NodeReader`, which is
//! format-neutral. That is what lets one struct definition serve the KDL file
//! frontend, the Admin API and Kubernetes without a decoder per frontend.
//!
//! ```ignore
//! #[derive(FromNode)]
//! struct Respond {
//!     #[gatel(arg, default)]
//!     body: String,
//!     #[gatel(prop, default = 200)]
//!     status: u16,
//!     #[gatel(prop)]
//!     content_type: Option<String>, // optional, reads `content-type`
//! }
//! ```

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{
    Data, DeriveInput, Error, Expr, Field, Fields, GenericArgument, PathArguments, Type, parse_macro_input,
};

#[proc_macro_derive(FromNode, attributes(gatel))]
pub fn derive_from_node(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Where a field's value comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// One positional argument.
    Arg,
    /// Every remaining positional argument.
    Args,
    /// A named property.
    Prop,
    /// One child node.
    Child,
    /// Every child node with this name.
    Children,
    /// Every child node not claimed by another field, left undecoded.
    RestChildren,
}

struct FieldSpec {
    ident: syn::Ident,
    source: Source,
    key: String,
    /// `None` for a required field, `Some(None)` for `Default::default()`,
    /// `Some(Some(expr))` for an explicit default.
    default: Option<Option<Expr>>,
    optional: bool,
    inner: Type,
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    let Data::Struct(data) = &input.data else {
        return Err(Error::new(
            input.span(),
            "FromNode can only be derived for structs",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(Error::new(input.span(), "FromNode requires named fields"));
    };

    let specs: Vec<FieldSpec> = fields.named.iter().map(parse_field).collect::<syn::Result<_>>()?;

    let mut seen_rest_args = false;
    for spec in &specs {
        if seen_rest_args && matches!(spec.source, Source::Arg | Source::Args) {
            return Err(Error::new(
                spec.ident.span(),
                "no positional field may follow a `#[gatel(args)]` field",
            ));
        }
        if spec.source == Source::Args {
            seen_rest_args = true;
        }
    }

    let mut reads = Vec::new();
    let mut builds = Vec::new();

    for spec in &specs {
        let ident = &spec.ident;
        let binding = format_ident!("__field_{}", ident);
        let inner = &spec.inner;
        let key = &spec.key;
        let required = spec.default.is_none() && !spec.optional;

        let read = match (spec.source, required) {
            (Source::Arg, true) => quote!(__reader.arg::<#inner>(#key)),
            (Source::Arg, false) => quote!(__reader.opt_arg::<#inner>(#key)),
            (Source::Args, _) => quote!(__reader.rest_args::<#inner>(#key)),
            (Source::Prop, true) => quote!(__reader.prop::<#inner>(#key)),
            (Source::Prop, false) => quote!(__reader.opt_prop::<#inner>(#key)),
            (Source::Child, true) => quote!(__reader.child::<#inner>(#key)),
            (Source::Child, false) => quote!(__reader.opt_child::<#inner>(#key)),
            (Source::Children, _) => quote!(__reader.children::<#inner>(#key)),
            (Source::RestChildren, _) => {
                quote!({
                    __reader.takes_any_children();
                    __reader.node().children.clone()
                })
            }
        };
        reads.push(quote!(let #binding = #read;));

        let build = match spec.source {
            Source::Args | Source::Children | Source::RestChildren => quote!(#binding),
            _ if spec.optional => quote!(#binding),
            _ => match &spec.default {
                None => quote!(#binding?),
                Some(None) => quote!(#binding.unwrap_or_default()),
                Some(Some(expr)) => quote!(#binding.unwrap_or_else(|| #expr)),
            },
        };
        builds.push(quote!(#ident: #build));
    }

    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics ::gatel_model::FromNode for #name #ty_generics #where_clause {
            fn from_node(
                __node: &::gatel_model::Node,
                __diagnostics: &mut ::gatel_model::Diagnostics,
            ) -> ::core::option::Option<Self> {
                let __errors_before = __diagnostics.error_count();
                let mut __reader = ::gatel_model::NodeReader::new(__node, __diagnostics);
                #(#reads)*
                __reader.finish();
                if __diagnostics.error_count() > __errors_before {
                    return ::core::option::Option::None;
                }
                ::core::option::Option::Some(Self { #(#builds),* })
            }
        }
    })
}

fn parse_field(field: &Field) -> syn::Result<FieldSpec> {
    let ident = field.ident.clone().expect("named field");
    let mut source = None;
    let mut rename = None;
    let mut default = None;

    for attr in &field.attrs {
        if !attr.path().is_ident("gatel") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let set_source = |slot: &mut Option<Source>, value: Source| -> syn::Result<()> {
                if slot.is_some() {
                    return Err(meta.error("a field may only have one source"));
                }
                *slot = Some(value);
                Ok(())
            };
            if meta.path.is_ident("arg") {
                return set_source(&mut source, Source::Arg);
            }
            if meta.path.is_ident("args") {
                return set_source(&mut source, Source::Args);
            }
            if meta.path.is_ident("prop") {
                return set_source(&mut source, Source::Prop);
            }
            if meta.path.is_ident("child") {
                return set_source(&mut source, Source::Child);
            }
            if meta.path.is_ident("children") {
                return set_source(&mut source, Source::Children);
            }
            if meta.path.is_ident("rest_children") {
                return set_source(&mut source, Source::RestChildren);
            }
            if meta.path.is_ident("rename") {
                rename = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                return Ok(());
            }
            if meta.path.is_ident("default") {
                default = Some(if meta.input.peek(syn::Token![=]) {
                    Some(meta.value()?.parse::<Expr>()?)
                } else {
                    None
                });
                return Ok(());
            }
            Err(meta
                .error("expected one of: arg, args, prop, child, children, rest_children, rename, default"))
        })?;
    }

    let source = source.ok_or_else(|| {
        Error::new(
            field.span(),
            "every field needs a `#[gatel(..)]` source: arg, args, prop, child, children or rest_children",
        )
    })?;

    let key = rename.unwrap_or_else(|| kebab_case(&ident.to_string()));

    let (optional, inner) = match source {
        Source::Args | Source::Children => {
            let inner = vec_inner(&field.ty).ok_or_else(|| {
                Error::new(field.ty.span(), "`args` and `children` fields must be `Vec<T>`")
            })?;
            (false, inner)
        }
        Source::RestChildren => (false, field.ty.clone()),
        _ => match option_inner(&field.ty) {
            Some(inner) => (true, inner),
            None => (false, field.ty.clone()),
        },
    };

    if optional && default.is_some() {
        return Err(Error::new(
            field.span(),
            "`Option<T>` is already optional, remove `default`",
        ));
    }

    Ok(FieldSpec {
        ident,
        source,
        key,
        default,
        optional,
        inner,
    })
}

fn kebab_case(name: &str) -> String {
    name.trim_start_matches("r#").replace('_', "-")
}

fn generic_inner<'a>(ty: &'a Type, wrapper: &str) -> Option<Type> {
    let Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    if segment.ident != wrapper {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        GenericArgument::Type(inner) => Some(inner.clone()),
        _ => None,
    })
}

fn option_inner(ty: &Type) -> Option<Type> {
    generic_inner(ty, "Option")
}

fn vec_inner(ty: &Type) -> Option<Type> {
    generic_inner(ty, "Vec")
}
