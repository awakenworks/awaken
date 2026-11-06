//! Canonical runtime lookup for the package binary exercised by integration tests.

pub fn awaken_sandbox() -> &'static str {
    // The runner states the path; the test never guesses it. Each runner uses
    // its own name for the same fact -- `cargo test` exports
    // CARGO_BIN_EXE_<bin> into the test process, nextest exports
    // NEXTEST_BIN_EXE_<bin> and no CARGO_ one (measured, not assumed:
    // `option_env!` of either is None, so it is not a compile-time value
    // here). Reading only one name made the whole package fail with
    // `NotPresent` under the other runner.
    // B1 cargo check has no runtime binary path -> compile without resolving;
    // B2 either runner supplies the path -> every integration suite uses
    // exactly that executable, including Cargo's separate global intermediate
    // directory. No target/debug guess or fallback binary may hide a missing
    // runtime input.
    static BINARY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        std::env::var("CARGO_BIN_EXE_awaken-sandbox")
            .or_else(|_| std::env::var("NEXTEST_BIN_EXE_awaken-sandbox"))
            .expect("the test runner must provide the awaken-sandbox path")
    });
    BINARY.as_str()
}
