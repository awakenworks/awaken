#[ignore = "requires DEEPSEEK_API_KEY"]
#[tokio::test]
async fn e2e_deepseek_placeholder_guarded() {
    assert!(std::env::var("DEEPSEEK_API_KEY").is_ok());
}
