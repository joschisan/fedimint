#![deny(clippy::pedantic)]
#![cfg_attr(feature = "diagnostics", feature(proc_macro_diagnostic))]

use proc_macro::TokenStream;
use proc_macro2::{Ident, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Fields, Index, Lit, Token, Variant,
    parse_macro_input,
};

#[proc_macro_derive(Encodable, attributes(encodable))]
pub fn derive_encodable(input: TokenStream) -> TokenStream {
    let DeriveInput {
        ident,
        data,
        generics,
        ..
    } = parse_macro_input!(input);

    let encode_body = match data {
        Data::Struct(DataStruct { fields, .. }) => derive_struct_encode(&fields),
        Data::Enum(DataEnum { variants, .. }) => derive_enum_encode(&ident, &variants),
        Data::Union(_) => error(&ident, "Encodable can't be derived for unions"),
    };
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    quote! {
        impl #impl_generics ::picomint_core::encoding::Encodable for #ident #ty_generics #where_clause {
            fn consensus_encode<W: ::std::io::Write>(&self, writer: &mut W) -> ::std::io::Result<()> {
                #encode_body
            }
        }
    }
    .into()
}

#[proc_macro_derive(Decodable)]
pub fn derive_decodable(input: TokenStream) -> TokenStream {
    let DeriveInput {
        ident,
        data,
        generics,
        ..
    } = parse_macro_input!(input);

    let decode_body = match data {
        Data::Struct(DataStruct { fields, .. }) => derive_struct_decode(&ident, &fields),
        Data::Enum(DataEnum { variants, .. }) => derive_enum_decode(&ident, &variants),
        Data::Union(_) => error(&ident, "Decodable can't be derived for unions"),
    };
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    quote! {
        impl #impl_generics ::picomint_core::encoding::Decodable for #ident #ty_generics #where_clause {
            fn consensus_decode<R: ::std::io::Read>(reader: &mut R) -> ::std::io::Result<Self> {
                #decode_body
            }
        }
    }
    .into()
}

// ─── Encode ─────────────────────────────────────────────────────────────

fn derive_struct_encode(fields: &Fields) -> TokenStream2 {
    if is_tuple_struct(fields) {
        let idxs = fields
            .iter()
            .enumerate()
            .map(|(i, _)| Index::from(i))
            .collect::<Vec<_>>();
        quote! {
            #(::picomint_core::encoding::Encodable::consensus_encode(&self.#idxs, writer)?;)*
            Ok(())
        }
    } else {
        let names = fields
            .iter()
            .map(|f| f.ident.clone().unwrap())
            .collect::<Vec<_>>();
        quote! {
            #(::picomint_core::encoding::Encodable::consensus_encode(&self.#names, writer)?;)*
            Ok(())
        }
    }
}

fn derive_enum_encode(ident: &Ident, variants: &Punctuated<Variant, Comma>) -> TokenStream2 {
    if variants.is_empty() {
        return quote! { match *self {} };
    }

    let arms = variant_indices(variants).into_iter().map(|(idx, variant)| {
        let vname = variant.ident.clone();
        let idx_lit = idx;

        if is_tuple_variant(&variant.fields) {
            let binds = variant
                .fields
                .iter()
                .enumerate()
                .map(|(i, _)| format_ident!("f{i}"))
                .collect::<Vec<_>>();
            quote! {
                #ident::#vname(#(#binds),*) => {
                    ::picomint_core::encoding::Encodable::consensus_encode(&#idx_lit, writer)?;
                    #(::picomint_core::encoding::Encodable::consensus_encode(#binds, writer)?;)*
                }
            }
        } else if variant.fields.is_empty() {
            quote! {
                #ident::#vname => {
                    ::picomint_core::encoding::Encodable::consensus_encode(&#idx_lit, writer)?;
                }
            }
        } else {
            let names = variant
                .fields
                .iter()
                .map(|f| f.ident.clone().unwrap())
                .collect::<Vec<_>>();
            quote! {
                #ident::#vname { #(#names),* } => {
                    ::picomint_core::encoding::Encodable::consensus_encode(&#idx_lit, writer)?;
                    #(::picomint_core::encoding::Encodable::consensus_encode(#names, writer)?;)*
                }
            }
        }
    });

    quote! {
        match self {
            #(#arms)*
        }
        Ok(())
    }
}

// ─── Decode ─────────────────────────────────────────────────────────────

fn derive_struct_decode(ident: &Ident, fields: &Fields) -> TokenStream2 {
    if is_tuple_struct(fields) {
        let binds = fields
            .iter()
            .enumerate()
            .map(|(i, _)| format_ident!("f{i}"))
            .collect::<Vec<_>>();
        quote! {
            #(let #binds = ::picomint_core::encoding::Decodable::consensus_decode(reader)?;)*
            Ok(#ident(#(#binds),*))
        }
    } else if fields.is_empty() {
        quote! { Ok(#ident {}) }
    } else {
        let names = fields
            .iter()
            .map(|f| f.ident.clone().unwrap())
            .collect::<Vec<_>>();
        quote! {
            #(let #names = ::picomint_core::encoding::Decodable::consensus_decode(reader)?;)*
            Ok(#ident { #(#names),* })
        }
    }
}

fn derive_enum_decode(ident: &Ident, variants: &Punctuated<Variant, Comma>) -> TokenStream2 {
    if variants.is_empty() {
        return quote! {
            Err(::std::io::Error::new(
                ::std::io::ErrorKind::InvalidData,
                concat!("Uninhabited enum ", stringify!(#ident), " cannot be decoded"),
            ))
        };
    }

    let arms = variant_indices(variants).into_iter().map(|(idx, variant)| {
        let vname = variant.ident.clone();
        let idx_lit = idx;

        let construct = if is_tuple_variant(&variant.fields) {
            let binds = variant
                .fields
                .iter()
                .enumerate()
                .map(|(i, _)| format_ident!("f{i}"))
                .collect::<Vec<_>>();
            quote! {
                {
                    #(let #binds = ::picomint_core::encoding::Decodable::consensus_decode(reader)?;)*
                    Ok(#ident::#vname(#(#binds),*))
                }
            }
        } else if variant.fields.is_empty() {
            quote! { Ok(#ident::#vname) }
        } else {
            let names = variant
                .fields
                .iter()
                .map(|f| f.ident.clone().unwrap())
                .collect::<Vec<_>>();
            quote! {
                {
                    #(let #names = ::picomint_core::encoding::Decodable::consensus_decode(reader)?;)*
                    Ok(#ident::#vname { #(#names),* })
                }
            }
        };

        quote! { #idx_lit => #construct, }
    });

    quote! {
        let variant = <u64 as ::picomint_core::encoding::Decodable>::consensus_decode(reader)?;
        match variant {
            #(#arms)*
            other => Err(::std::io::Error::new(
                ::std::io::ErrorKind::InvalidData,
                format!("Invalid variant {} for {}", other, stringify!(#ident)),
            )),
        }
    }
}

// ─── Variant indexing ───────────────────────────────────────────────────

/// Extracts the u64 index from `#[encodable(index = N)]` if present.
fn parse_index_attribute(attributes: &[Attribute]) -> Option<u64> {
    attributes.iter().find_map(|attr| {
        if attr.path().is_ident("encodable") {
            attr.parse_args_with(|input: syn::parse::ParseStream| {
                input.parse::<syn::Ident>()?.span();
                input.parse::<Token![=]>()?;
                if let Lit::Int(lit_int) = input.parse::<Lit>()? {
                    lit_int.base10_parse()
                } else {
                    Err(input.error("Expected integer for 'index'"))
                }
            })
            .ok()
        } else {
            None
        }
    })
}

fn variant_indices(variants: &Punctuated<Variant, Comma>) -> Vec<(u64, Variant)> {
    let pairs = variants
        .iter()
        .cloned()
        .map(|v| (parse_index_attribute(&v.attrs), v))
        .collect::<Vec<_>>();

    let all = pairs.iter().all(|(idx, _)| idx.is_some());
    let none = pairs.iter().all(|(idx, _)| idx.is_none());
    assert!(
        all || none,
        "Either all or none of the variants should have an index annotation"
    );

    if all {
        pairs
            .into_iter()
            .map(|(idx, v)| (idx.expect("checked above"), v))
            .collect()
    } else {
        pairs
            .into_iter()
            .enumerate()
            .map(|(i, (_, v))| (i as u64, v))
            .collect()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn is_tuple_struct(fields: &Fields) -> bool {
    fields.iter().any(|f| f.ident.is_none()) && !fields.is_empty()
}

fn is_tuple_variant(fields: &Fields) -> bool {
    !fields.is_empty() && fields.iter().any(|f| f.ident.is_none())
}

#[allow(unused_variables, unreachable_code)]
fn error(ident: &Ident, message: &str) -> TokenStream2 {
    #[cfg(feature = "diagnostics")]
    ident.span().unstable().error(message).emit();
    #[cfg(not(feature = "diagnostics"))]
    panic!("{message}");

    TokenStream2::new()
}
