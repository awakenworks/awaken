//! Field type analysis for code generation.

use syn::{GenericArgument, PathArguments, Type, TypePath};

/// The kind of a field, determining how to generate reader/writer methods.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// A primitive type (String, i32, bool, etc.)
    Primitive,

    /// An Option<T> type
    Option(Box<FieldKind>),

    /// A Vec<T> type
    Vec(Box<FieldKind>),

    /// A BTreeMap<K, V> or HashMap<K, V> type
    Map {
        key: Box<FieldKind>,
        value: Box<FieldKind>,
    },

    /// A nested State type
    Nested,

    /// A lattice CRDT type
    Lattice,
}

impl FieldKind {
    /// Analyze a type and determine its kind.
    ///
    /// The `is_nested_attr` flag marks the **leaf type** as nested,
    /// while container structure (Option/Vec/Map) is preserved.
    ///
    /// Examples:
    /// - `Option<Profile>` with `nested=true` → `Option(Nested)`
    /// - `Vec<Profile>` with `nested=true` → `Vec(Nested)`
    /// - `Profile` with `nested=true` → `Nested`
    pub fn from_type(ty: &Type, is_nested_attr: bool, is_lattice_attr: bool) -> Self {
        match ty {
            Type::Path(type_path) => {
                Self::from_type_path(type_path, is_nested_attr, is_lattice_attr)
            }
            _ => FieldKind::Primitive,
        }
    }

    fn from_type_path(type_path: &TypePath, is_nested_attr: bool, is_lattice_attr: bool) -> Self {
        let path = &type_path.path;

        // Get the last segment (the actual type name)
        if let Some(segment) = path.segments.last() {
            let type_name = segment.ident.to_string();

            match type_name.as_str() {
                "Option" => {
                    if let Some(inner) = extract_single_generic_arg(&segment.arguments) {
                        // Propagate nested attr to inner type (lattice not propagated into containers)
                        FieldKind::Option(Box::new(Self::from_type(inner, is_nested_attr, false)))
                    } else {
                        FieldKind::Primitive
                    }
                }
                "Vec" => {
                    if let Some(inner) = extract_single_generic_arg(&segment.arguments) {
                        // Propagate nested attr to inner type (lattice not propagated into containers)
                        FieldKind::Vec(Box::new(Self::from_type(inner, is_nested_attr, false)))
                    } else {
                        FieldKind::Primitive
                    }
                }
                "BTreeMap" | "HashMap" => {
                    if let Some((key, value)) = extract_two_generic_args(&segment.arguments) {
                        FieldKind::Map {
                            // JSON keys are always strings, never nested
                            key: Box::new(Self::from_type(key, false, false)),
                            // Propagate nested attr to value type (lattice not propagated into containers)
                            value: Box::new(Self::from_type(value, is_nested_attr, false)),
                        }
                    } else {
                        FieldKind::Primitive
                    }
                }
                // Common primitive types
                "String" | "str" | "bool" | "char" | "i8" | "i16" | "i32" | "i64" | "i128"
                | "isize" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "f32" | "f64" => {
                    FieldKind::Primitive
                }
                // Unknown types - check nested/lattice attr
                _ => {
                    if is_nested_attr {
                        FieldKind::Nested
                    } else if is_lattice_attr {
                        FieldKind::Lattice
                    } else {
                        // Treat as primitive (will use serde for serialization)
                        FieldKind::Primitive
                    }
                }
            }
        } else {
            FieldKind::Primitive
        }
    }

    /// Check if this is a primitive type.
    #[allow(dead_code)]
    pub fn is_primitive(&self) -> bool {
        matches!(self, FieldKind::Primitive)
    }

    /// Check if this is an Option type.
    #[allow(dead_code)]
    pub fn is_option(&self) -> bool {
        matches!(self, FieldKind::Option(_))
    }

    /// Check if this is a Vec type.
    #[allow(dead_code)]
    pub fn is_vec(&self) -> bool {
        matches!(self, FieldKind::Vec(_))
    }

    /// Check if this is a Map type.
    #[allow(dead_code)]
    pub fn is_map(&self) -> bool {
        matches!(self, FieldKind::Map { .. })
    }

    /// Check if this is a nested type.
    pub fn is_nested(&self) -> bool {
        matches!(self, FieldKind::Nested)
    }

    /// Check if this is a lattice CRDT type.
    pub fn is_lattice(&self) -> bool {
        matches!(self, FieldKind::Lattice)
    }
}

/// Extract a single generic type argument from path arguments.
fn extract_single_generic_arg(args: &PathArguments) -> Option<&Type> {
    match args {
        PathArguments::AngleBracketed(ab) => {
            if ab.args.len() == 1 {
                if let GenericArgument::Type(ty) = ab.args.first()? {
                    return Some(ty);
                }
            }
            None
        }
        _ => None,
    }
}

/// Extract two generic type arguments from path arguments (for Map types).
fn extract_two_generic_args(args: &PathArguments) -> Option<(&Type, &Type)> {
    match args {
        PathArguments::AngleBracketed(ab) => {
            if ab.args.len() == 2 {
                let mut iter = ab.args.iter();
                if let (Some(GenericArgument::Type(key)), Some(GenericArgument::Type(value))) =
                    (iter.next(), iter.next())
                {
                    return Some((key, value));
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn test_primitive_types() {
        let ty: Type = parse_quote!(String);
        assert!(FieldKind::from_type(&ty, false, false).is_primitive());

        let ty: Type = parse_quote!(i32);
        assert!(FieldKind::from_type(&ty, false, false).is_primitive());

        let ty: Type = parse_quote!(bool);
        assert!(FieldKind::from_type(&ty, false, false).is_primitive());
    }

    #[test]
    fn test_option_type() {
        let ty: Type = parse_quote!(Option<String>);
        let kind = FieldKind::from_type(&ty, false, false);
        assert!(kind.is_option());

        if let FieldKind::Option(inner) = kind {
            assert!(inner.is_primitive());
        }
    }

    #[test]
    fn test_vec_type() {
        let ty: Type = parse_quote!(Vec<i32>);
        let kind = FieldKind::from_type(&ty, false, false);
        assert!(kind.is_vec());

        if let FieldKind::Vec(inner) = kind {
            assert!(inner.is_primitive());
        }
    }

    #[test]
    fn test_map_type() {
        let ty: Type = parse_quote!(BTreeMap<String, i32>);
        let kind = FieldKind::from_type(&ty, false, false);
        assert!(kind.is_map());
    }

    #[test]
    fn test_nested_attr() {
        let ty: Type = parse_quote!(Profile);
        let kind = FieldKind::from_type(&ty, true, false);
        assert!(kind.is_nested());
    }

    #[test]
    fn test_lattice_attr() {
        let ty: Type = parse_quote!(GCounter);
        let kind = FieldKind::from_type(&ty, false, true);
        assert!(kind.is_lattice());
    }

    #[test]
    fn test_lattice_on_container_stays_container() {
        // Lattice attr is NOT propagated into containers
        let ty: Type = parse_quote!(Option<GCounter>);
        let kind = FieldKind::from_type(&ty, false, true);
        assert!(kind.is_option());

        let ty: Type = parse_quote!(Vec<GCounter>);
        let kind = FieldKind::from_type(&ty, false, true);
        assert!(kind.is_vec());

        let ty: Type = parse_quote!(BTreeMap<String, GCounter>);
        let kind = FieldKind::from_type(&ty, false, true);
        assert!(kind.is_map());
    }
}
