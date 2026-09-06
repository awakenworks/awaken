//! Canonical runtime lookup for the package binary exercised by integration tests.

pub fn awaken_sandbox() -> String {
    std::env::var("CARGO_BIN_EXE_awaken-sandbox")
        .expect("Cargo must provide the awaken-sandbox path when an integration test runs")
}
