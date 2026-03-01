//! StateRef code generation.
//!
//! Generates typed state reference structs that use PatchSink for automatic
//! operation collection.

use super::utils::{extract_inner_type, extract_map_types, get_type_name};
use crate::field_kind::FieldKind;
use crate::parse::{FieldInput, ViewModelInput};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

/// Generate the StateRef struct and State trait implementation.
pub fn generate(input: &ViewModelInput) -> syn::Result<TokenStream> {
    let struct_name = &input.ident;
    let ref_name = format_ident!("{}Ref", struct_name);
    let vis = &input.vis;

    let fields: Vec<_> = input
        .fields()
        .into_iter()
        .filter(|f| f.is_included())
        .collect();

    let path_value = input.path.as_deref().unwrap_or("");

    let read_methods = generate_read_methods(&fields)?;
    let write_methods = generate_write_methods(&fields)?;
    let delete_methods = generate_delete_methods(&fields)?;
    let increment_methods = generate_increment_methods(&fields)?;

    Ok(quote! {
        /// Typed state reference for reading and writing state.
        ///
        /// All modifications are automatically collected by the associated `PatchSink`.
        #vis struct #ref_name<'a> {
            doc: &'a ::tirea_state::DocCell,
            base: ::tirea_state::Path,
            sink: ::tirea_state::PatchSink<'a>,
        }

        impl<'a> #ref_name<'a> {
            /// Create a new state reference.
            #[doc(hidden)]
            pub fn new(
                doc: &'a ::tirea_state::DocCell,
                base: ::tirea_state::Path,
                sink: ::tirea_state::PatchSink<'a>,
            ) -> Self {
                Self { doc, base, sink }
            }

            #read_methods

            #write_methods

            #delete_methods

            #increment_methods
        }

        impl ::tirea_state::State for #struct_name {
            type Ref<'a> = #ref_name<'a>;

            const PATH: &'static str = #path_value;

            fn state_ref<'a>(
                doc: &'a ::tirea_state::DocCell,
                base: ::tirea_state::Path,
                sink: ::tirea_state::PatchSink<'a>,
            ) -> Self::Ref<'a> {
                #ref_name::new(doc, base, sink)
            }

            fn from_value(value: &::serde_json::Value) -> ::tirea_state::TireaResult<Self> {
                ::serde_json::from_value(value.clone())
                    .map_err(::tirea_state::TireaError::from)
            }

            fn to_value(&self) -> ::tirea_state::TireaResult<::serde_json::Value> {
                ::serde_json::to_value(self)
                    .map_err(::tirea_state::TireaError::from)
            }
        }
    })
}

/// Generate read methods for each field.
fn generate_read_methods(fields: &[&FieldInput]) -> syn::Result<TokenStream> {
    let mut methods = TokenStream::new();

    for field in fields {
        let method = generate_read_method(field)?;
        methods.extend(method);
    }

    Ok(methods)
}

/// Generate a read method for a single field.
fn generate_read_method(field: &FieldInput) -> syn::Result<TokenStream> {
    let field_name = field.ident();
    let field_ty = &field.ty;
    let json_key = field.json_key();
    let kind = FieldKind::from_type(field_ty, field.flatten || field.nested, field.lattice);

    let method = match &kind {
        FieldKind::Nested => {
            if field.flatten {
                quote! {
                    /// Get a state reference for the flattened nested field.
                    pub fn #field_name(&self) -> <#field_ty as ::tirea_state::State>::Ref<'a> {
                        <#field_ty as ::tirea_state::State>::state_ref(
                            self.doc,
                            self.base.clone(),
                            self.sink.child(),
                        )
                    }
                }
            } else {
                quote! {
                    /// Get a state reference for the nested field.
                    pub fn #field_name(&self) -> <#field_ty as ::tirea_state::State>::Ref<'a> {
                        let path = self.base.clone().key(#json_key);
                        <#field_ty as ::tirea_state::State>::state_ref(
                            self.doc,
                            path,
                            self.sink.child(),
                        )
                    }
                }
            }
        }
        FieldKind::Option(inner) if inner.is_nested() => {
            let inner_ty = extract_inner_type(field_ty);
            let get_name = format_ident!("get_{}", field_name);
            quote! {
                /// Get a state reference for the optional nested field.
                pub fn #field_name(&self) -> <#inner_ty as ::tirea_state::State>::Ref<'a> {
                    let path = self.base.clone().key(#json_key);
                    <#inner_ty as ::tirea_state::State>::state_ref(
                        self.doc,
                        path,
                        self.sink.child(),
                    )
                }

                /// Read the optional nested field value.
                pub fn #get_name(&self) -> ::tirea_state::TireaResult<Option<#inner_ty>> {
                    let path = self.base.clone().key(#json_key);
                    match { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() } {
                        None => Ok(None),
                        Some(value) if value.is_null() => Ok(None),
                        Some(value) => {
                            let v: #inner_ty = ::serde_json::from_value(value)
                                .map_err(::tirea_state::TireaError::from)?;
                            Ok(Some(v))
                        }
                    }
                }
            }
        }
        FieldKind::Option(_) => {
            quote! {
                /// Read the optional field value.
                pub fn #field_name(&self) -> ::tirea_state::TireaResult<#field_ty> {
                    let path = self.base.clone().key(#json_key);
                    match { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() } {
                        None => Ok(None),
                        Some(value) if value.is_null() => Ok(None),
                        Some(value) => {
                            ::serde_json::from_value(value)
                                .map_err(::tirea_state::TireaError::from)
                        }
                    }
                }
            }
        }
        FieldKind::Vec(_) | FieldKind::Map { .. } => {
            if let Some(default) = &field.default {
                let expr: syn::Expr = syn::parse_str(default).map_err(|e| {
                    syn::Error::new_spanned(field_ty, format!("invalid default expression: {}", e))
                })?;
                quote! {
                    /// Read the field value.
                    pub fn #field_name(&self) -> ::tirea_state::TireaResult<#field_ty> {
                        let path = self.base.clone().key(#json_key);
                        match { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() } {
                            None => Ok(#expr),
                            Some(value) => {
                                ::serde_json::from_value(value)
                                    .map_err(::tirea_state::TireaError::from)
                            }
                        }
                    }
                }
            } else {
                quote! {
                    /// Read the field value.
                    pub fn #field_name(&self) -> ::tirea_state::TireaResult<#field_ty> {
                        let path = self.base.clone().key(#json_key);
                        let value = { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() }
                            .ok_or_else(|| ::tirea_state::TireaError::path_not_found(path.clone()))?;
                        ::serde_json::from_value(value)
                            .map_err(::tirea_state::TireaError::from)
                    }
                }
            }
        }
        FieldKind::Lattice => {
            if let Some(default) = &field.default {
                let expr: syn::Expr = syn::parse_str(default).map_err(|e| {
                    syn::Error::new_spanned(field_ty, format!("invalid default expression: {}", e))
                })?;
                quote! {
                    /// Read the lattice field value.
                    pub fn #field_name(&self) -> ::tirea_state::TireaResult<#field_ty> {
                        let path = self.base.clone().key(#json_key);
                        match { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() } {
                            None => Ok(#expr),
                            Some(value) => {
                                ::serde_json::from_value(value)
                                    .map_err(::tirea_state::TireaError::from)
                            }
                        }
                    }
                }
            } else {
                quote! {
                    /// Read the lattice field value.
                    pub fn #field_name(&self) -> ::tirea_state::TireaResult<#field_ty> {
                        let path = self.base.clone().key(#json_key);
                        let value = { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() }
                            .ok_or_else(|| ::tirea_state::TireaError::path_not_found(path.clone()))?;
                        ::serde_json::from_value(value)
                            .map_err(::tirea_state::TireaError::from)
                    }
                }
            }
        }
        FieldKind::Primitive => {
            if let Some(default) = &field.default {
                let expr: syn::Expr = syn::parse_str(default).map_err(|e| {
                    syn::Error::new_spanned(field_ty, format!("invalid default expression: {}", e))
                })?;
                quote! {
                    /// Read the field value.
                    pub fn #field_name(&self) -> ::tirea_state::TireaResult<#field_ty> {
                        let path = self.base.clone().key(#json_key);
                        match { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() } {
                            None => Ok(#expr),
                            Some(value) => {
                                ::serde_json::from_value(value)
                                    .map_err(::tirea_state::TireaError::from)
                            }
                        }
                    }
                }
            } else {
                quote! {
                    /// Read the field value.
                    pub fn #field_name(&self) -> ::tirea_state::TireaResult<#field_ty> {
                        let path = self.base.clone().key(#json_key);
                        let value = { let __guard = self.doc.get(); ::tirea_state::get_at_path(&__guard, &path).cloned() }
                            .ok_or_else(|| ::tirea_state::TireaError::path_not_found(path.clone()))?;
                        ::serde_json::from_value(value)
                            .map_err(::tirea_state::TireaError::from)
                    }
                }
            }
        }
    };

    Ok(method)
}

/// Generate write methods for each field.
fn generate_write_methods(fields: &[&FieldInput]) -> syn::Result<TokenStream> {
    let mut methods = TokenStream::new();

    for field in fields {
        let method = generate_write_method(field)?;
        methods.extend(method);
    }

    Ok(methods)
}

/// Generate a write method for a single field.
fn generate_write_method(field: &FieldInput) -> syn::Result<TokenStream> {
    let field_name = field.ident();
    let field_ty = &field.ty;
    let json_key = field.json_key();
    let kind = FieldKind::from_type(field_ty, field.flatten || field.nested, field.lattice);

    let methods = match &kind {
        FieldKind::Nested => {
            // Nested fields get their state ref through the read method
            quote! {}
        }
        FieldKind::Option(inner) if inner.is_nested() => {
            let set_name = format_ident!("set_{}", field_name);
            let none_name = format_ident!("{}_none", field_name);
            quote! {
                /// Set the entire optional nested field value.
                pub fn #set_name(&self, value: #field_ty) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    let value = ::serde_json::to_value(&value)
                        .map_err(::tirea_state::TireaError::from)?;
                    self.sink.collect(::tirea_state::Op::Set {
                        path,
                        value,
                    })
                }

                /// Set the optional field to null (None).
                pub fn #none_name(&self) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    self.sink.collect(::tirea_state::Op::Set {
                        path,
                        value: ::serde_json::Value::Null,
                    })
                }
            }
        }
        FieldKind::Option(_) => {
            let set_name = format_ident!("set_{}", field_name);
            let none_name = format_ident!("{}_none", field_name);
            quote! {
                /// Set the optional field value.
                pub fn #set_name(&self, value: #field_ty) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    let value = ::serde_json::to_value(&value)
                        .map_err(::tirea_state::TireaError::from)?;
                    self.sink.collect(::tirea_state::Op::Set {
                        path,
                        value,
                    })
                }

                /// Set the optional field to null (None).
                pub fn #none_name(&self) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    self.sink.collect(::tirea_state::Op::Set {
                        path,
                        value: ::serde_json::Value::Null,
                    })
                }
            }
        }
        FieldKind::Vec(inner) => {
            let inner_ty = extract_inner_type(field_ty);
            let set_name = format_ident!("set_{}", field_name);
            let push_name = format_ident!("{}_push", field_name);

            if inner.is_nested() {
                quote! {
                    /// Set the entire array.
                    pub fn #set_name(&self, value: #field_ty) -> ::tirea_state::TireaResult<()> {
                        let path = self.base.clone().key(#json_key);
                        let value = ::serde_json::to_value(&value)
                            .map_err(::tirea_state::TireaError::from)?;
                        self.sink.collect(::tirea_state::Op::Set {
                            path,
                            value,
                        })
                    }

                    /// Push an item to the array.
                    pub fn #push_name(&self, value: #inner_ty) -> ::tirea_state::TireaResult<()> {
                        let path = self.base.clone().key(#json_key);
                        let value = ::serde_json::to_value(&value)
                            .map_err(::tirea_state::TireaError::from)?;
                        self.sink.collect(::tirea_state::Op::Append {
                            path,
                            value,
                        })
                    }
                }
            } else {
                quote! {
                    /// Set the entire array.
                    pub fn #set_name(&self, value: #field_ty) -> ::tirea_state::TireaResult<()> {
                        let path = self.base.clone().key(#json_key);
                        let value = ::serde_json::to_value(&value)
                            .map_err(::tirea_state::TireaError::from)?;
                        self.sink.collect(::tirea_state::Op::Set {
                            path,
                            value,
                        })
                    }

                    /// Push an item to the array.
                    pub fn #push_name(&self, value: impl Into<#inner_ty>) -> ::tirea_state::TireaResult<()> {
                        let path = self.base.clone().key(#json_key);
                        let v: #inner_ty = value.into();
                        let value = ::serde_json::to_value(&v)
                            .map_err(::tirea_state::TireaError::from)?;
                        self.sink.collect(::tirea_state::Op::Append {
                            path,
                            value,
                        })
                    }
                }
            }
        }
        FieldKind::Map { .. } => {
            let (key_ty, value_ty) = extract_map_types(field_ty);
            let key_type_name = get_type_name(&key_ty);
            let set_name = format_ident!("set_{}", field_name);

            let insert_method = if key_type_name == "String" {
                let insert_name = format_ident!("{}_insert", field_name);
                quote! {
                    /// Insert a key-value pair into the map.
                    pub fn #insert_name(&self, key: impl Into<String>, value: impl Into<#value_ty>) -> ::tirea_state::TireaResult<()> {
                        let path = self.base.clone().key(#json_key);
                        let k: String = key.into();
                        let path = path.key(k);
                        let v: #value_ty = value.into();
                        let value = ::serde_json::to_value(&v)
                            .map_err(::tirea_state::TireaError::from)?;
                        self.sink.collect(::tirea_state::Op::Set {
                            path,
                            value,
                        })
                    }
                }
            } else {
                quote! {}
            };

            quote! {
                /// Set the entire map.
                pub fn #set_name(&self, value: #field_ty) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    let value = ::serde_json::to_value(&value)
                        .map_err(::tirea_state::TireaError::from)?;
                    self.sink.collect(::tirea_state::Op::Set {
                        path,
                        value,
                    })
                }

                #insert_method
            }
        }
        FieldKind::Lattice => {
            let set_name = format_ident!("set_{}", field_name);
            let merge_name = format_ident!("merge_{}", field_name);
            quote! {
                /// Set the lattice field value.
                pub fn #set_name(&self, value: #field_ty) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    let value = ::serde_json::to_value(&value)
                        .map_err(::tirea_state::TireaError::from)?;
                    self.sink.collect(::tirea_state::Op::Set { path, value })
                }

                /// Merge another lattice value into this field.
                ///
                /// If the field is missing, writes `other` directly.
                /// If the field exists, deserialises, calls `Lattice::merge`, and writes the result.
                pub fn #merge_name(&self, other: &#field_ty) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    let current_val = {
                        let __guard = self.doc.get();
                        ::tirea_state::get_at_path(&__guard, &path).cloned()
                    };
                    let merged = match current_val {
                        None => other.clone(),
                        Some(v) => {
                            let current: #field_ty = ::serde_json::from_value(v)
                                .map_err(::tirea_state::TireaError::from)?;
                            ::tirea_state::Lattice::merge(&current, other)
                        }
                    };
                    let value = ::serde_json::to_value(&merged)
                        .map_err(::tirea_state::TireaError::from)?;
                    self.sink.collect(::tirea_state::Op::Set { path, value })
                }
            }
        }
        FieldKind::Primitive => {
            let set_name = format_ident!("set_{}", field_name);
            let type_name = get_type_name(field_ty);

            if type_name == "String" {
                quote! {
                    /// Set the field value.
                    pub fn #set_name(&self, value: impl Into<String>) -> ::tirea_state::TireaResult<()> {
                        let path = self.base.clone().key(#json_key);
                        let v: String = value.into();
                        self.sink.collect(::tirea_state::Op::Set {
                            path,
                            value: ::serde_json::Value::String(v),
                        })
                    }
                }
            } else {
                quote! {
                    /// Set the field value.
                    pub fn #set_name(&self, value: #field_ty) -> ::tirea_state::TireaResult<()> {
                        let path = self.base.clone().key(#json_key);
                        let value = ::serde_json::to_value(&value)
                            .map_err(::tirea_state::TireaError::from)?;
                        self.sink.collect(::tirea_state::Op::Set {
                            path,
                            value,
                        })
                    }
                }
            }
        }
    };

    Ok(methods)
}

/// Generate delete methods for each field.
fn generate_delete_methods(fields: &[&FieldInput]) -> syn::Result<TokenStream> {
    let mut methods = TokenStream::new();

    for field in fields {
        let field_name = field.ident();
        let json_key = field.json_key();
        let delete_name = format_ident!("delete_{}", field_name);

        // Skip delete for flatten fields
        if field.flatten {
            continue;
        }

        methods.extend(quote! {
            /// Delete this field entirely from the object.
            pub fn #delete_name(&self) -> ::tirea_state::TireaResult<()> {
                let path = self.base.clone().key(#json_key);
                self.sink.collect(::tirea_state::Op::Delete { path })
            }
        });
    }

    Ok(methods)
}

/// Generate increment methods for numeric fields.
fn generate_increment_methods(fields: &[&FieldInput]) -> syn::Result<TokenStream> {
    let mut methods = TokenStream::new();

    for field in fields {
        let field_ty = &field.ty;
        let kind = FieldKind::from_type(field_ty, field.flatten || field.nested, field.lattice);

        if !matches!(kind, FieldKind::Primitive) {
            continue;
        }

        let type_name = get_type_name(field_ty);

        let (is_int, is_float) = match type_name.as_str() {
            "i32" | "i64" => (true, false),
            "f64" => (false, true),
            _ => continue,
        };

        let field_name = field.ident();
        let json_key = field.json_key();
        let increment_name = format_ident!("increment_{}", field_name);
        let decrement_name = format_ident!("decrement_{}", field_name);

        if is_int {
            methods.extend(quote! {
                /// Increment the numeric field by the given amount.
                pub fn #increment_name(&self, amount: i64) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    self.sink.collect(::tirea_state::Op::Increment {
                        path,
                        amount: ::tirea_state::Number::Int(amount),
                    })
                }

                /// Decrement the numeric field by the given amount.
                pub fn #decrement_name(&self, amount: i64) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    self.sink.collect(::tirea_state::Op::Decrement {
                        path,
                        amount: ::tirea_state::Number::Int(amount),
                    })
                }
            });
        } else if is_float {
            methods.extend(quote! {
                /// Increment the numeric field by the given amount.
                pub fn #increment_name(&self, amount: f64) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    self.sink.collect(::tirea_state::Op::Increment {
                        path,
                        amount: ::tirea_state::Number::Float(amount),
                    })
                }

                /// Decrement the numeric field by the given amount.
                pub fn #decrement_name(&self, amount: f64) -> ::tirea_state::TireaResult<()> {
                    let path = self.base.clone().key(#json_key);
                    self.sink.collect(::tirea_state::Op::Decrement {
                        path,
                        amount: ::tirea_state::Number::Float(amount),
                    })
                }
            });
        }
    }

    Ok(methods)
}
