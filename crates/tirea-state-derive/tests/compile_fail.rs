#![allow(missing_docs)]

#[test]
fn lattice_compile_errors() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/lattice_*.rs");
}

#[test]
fn derive_lattice_compile_errors() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/derive_lattice_*.rs");
}
