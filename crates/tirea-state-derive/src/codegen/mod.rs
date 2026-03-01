//! Code generation for State derive macro.

mod state_ref;
mod utils;

use crate::field_kind::FieldKind;
use crate::parse::ViewModelInput;
use darling::FromDeriveInput;
use proc_macro2::TokenStream;
use syn::DeriveInput;

/// Main entry point for code generation.
pub fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let parsed = ViewModelInput::from_derive_input(input)
        .map_err(|e| syn::Error::new_spanned(input, e.to_string()))?;

    // Validate field attributes
    for field in parsed.fields() {
        // Check for flatten + rename conflict
        if field.flatten && field.rename.is_some() {
            return Err(syn::Error::new_spanned(
                field.ident(),
                "#[tirea(flatten)] and #[tirea(rename)] cannot be used together. \
                 Flattened fields are merged at the parent level, so rename has no effect.",
            ));
        }

        // Validate flatten fields
        if field.flatten {
            let kind = FieldKind::from_type(&field.ty, /* is_nested_attr = */ true, false);
            match kind {
                FieldKind::Nested => {
                    // Valid: flatten on a struct field
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        &field.ty,
                        "#[tirea(flatten)] currently only supports struct fields (non-Option/Vec/Map). \
                         The field must be a type that implements State.",
                    ));
                }
            }
        }

        // Validate lattice fields
        if field.lattice {
            if field.nested {
                return Err(syn::Error::new_spanned(
                    field.ident(),
                    "#[tirea(lattice)] and #[tirea(nested)] cannot be used together.",
                ));
            }
            if field.flatten {
                return Err(syn::Error::new_spanned(
                    field.ident(),
                    "#[tirea(lattice)] and #[tirea(flatten)] cannot be used together.",
                ));
            }
            let kind = FieldKind::from_type(&field.ty, false, true);
            if matches!(
                kind,
                FieldKind::Option(_) | FieldKind::Vec(_) | FieldKind::Map { .. }
            ) {
                return Err(syn::Error::new_spanned(
                    &field.ty,
                    "#[tirea(lattice)] is not supported on Option, Vec, or Map fields.",
                ));
            }
        }
    }

    // Generate only the StateRef struct and State trait impl
    state_ref::generate(&parsed)
}
