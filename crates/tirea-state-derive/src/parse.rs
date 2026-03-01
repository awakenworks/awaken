//! Parsing logic for State derive macro.

use darling::{ast, FromDeriveInput, FromField};
use syn::{Generics, Ident, Type, Visibility};

/// Parsed struct-level options.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(tirea), supports(struct_named))]
pub struct ViewModelInput {
    /// The struct identifier.
    pub ident: Ident,

    /// The struct visibility.
    pub vis: Visibility,

    /// Generic parameters.
    #[allow(dead_code)]
    pub generics: Generics,

    /// Struct data (fields).
    pub data: ast::Data<(), FieldInput>,

    /// Canonical JSON path for this state type (e.g., `#[tirea(path = "reminders")]`).
    #[darling(default)]
    pub path: Option<String>,
}

impl ViewModelInput {
    /// Get the fields as a vector.
    pub fn fields(&self) -> Vec<&FieldInput> {
        self.data
            .as_ref()
            .take_struct()
            .map(|s| s.fields.to_vec())
            .unwrap_or_default()
    }
}

/// Parsed field-level options.
#[derive(Debug, FromField)]
#[darling(attributes(tirea))]
pub struct FieldInput {
    /// Field identifier.
    pub ident: Option<Ident>,

    /// Field visibility.
    #[allow(dead_code)]
    pub vis: Visibility,

    /// Field type.
    pub ty: Type,

    /// Rename the field in JSON.
    #[darling(default)]
    pub rename: Option<String>,

    /// Default value expression if field is missing.
    #[darling(default)]
    pub default: Option<String>,

    /// Skip this field in reader/writer.
    #[darling(default)]
    pub skip: bool,

    /// Treat as nested State type.
    #[darling(default)]
    pub nested: bool,

    /// Flatten nested struct fields into parent.
    #[darling(default)]
    pub flatten: bool,

    /// Treat as a lattice CRDT type.
    #[darling(default)]
    pub lattice: bool,
}

impl FieldInput {
    /// Get the field identifier (panics if None).
    pub fn ident(&self) -> &Ident {
        self.ident.as_ref().expect("named field required")
    }

    /// Get the JSON key name for this field.
    pub fn json_key(&self) -> String {
        self.rename
            .clone()
            .unwrap_or_else(|| self.ident().to_string())
    }

    /// Check if this field should be included in reader/writer.
    pub fn is_included(&self) -> bool {
        !self.skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darling::FromDeriveInput;
    use syn::parse_quote;

    #[test]
    fn test_parse_basic_struct() {
        let input: syn::DeriveInput = parse_quote! {
            struct User {
                name: String,
                age: u32,
            }
        };

        let parsed = ViewModelInput::from_derive_input(&input).unwrap();
        assert_eq!(parsed.ident.to_string(), "User");

        let fields = parsed.fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].ident().to_string(), "name");
        assert_eq!(fields[1].ident().to_string(), "age");
    }

    #[test]
    fn test_parse_with_attributes() {
        let input: syn::DeriveInput = parse_quote! {
            struct User {
                #[tirea(rename = "user_name")]
                name: String,
                #[tirea(skip)]
                internal: String,
                #[tirea(default = "0")]
                count: u32,
            }
        };

        let parsed = ViewModelInput::from_derive_input(&input).unwrap();
        let fields = parsed.fields();

        assert_eq!(fields[0].json_key(), "user_name");
        assert!(fields[1].skip);
        assert_eq!(fields[2].default, Some("0".to_string()));
    }
}
