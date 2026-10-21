//! Verbose source statistics must accept imported constructor shims.

#[cfg(kani)]
#[kani::proof]
fn imported_constructor_preserves_its_value() {
    // Taking the imported constructor as FnOnce requires its real shim.
    // Keep the entire u64 domain symbolic and assert the exact returned value.
    let input: u64 = kani::any();
    let value = Some(input).map(constructor_model::Value::Exact);
    assert!(matches!(value, Some(constructor_model::Value::Exact(found)) if found == input));
}
