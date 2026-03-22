#[ignore = "requires tensorzero + DEEPSEEK_API_KEY"]
#[tokio::test]
async fn e2e_tensorzero_placeholder_guarded() {
    assert!(std::env::var("DEEPSEEK_API_KEY").is_ok());
}
