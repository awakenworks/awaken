//! Canonical runtime lookup for the package binary exercised by integration tests.

pub fn awaken_sandbox() -> &'static str {
    // B1 cargo check has no runtime binary path -> compile without resolving;
    // B2 cargo test supplies the path -> every integration suite uses exactly
    // that executable, including Cargo's separate global intermediate directory.
    // No target/debug guess or fallback binary may hide a missing runtime input.
    static BINARY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        std::env::var("CARGO_BIN_EXE_awaken-sandbox")
            .expect("Cargo must provide the awaken-sandbox path when an integration test runs")
    });
    BINARY.as_str()
}
