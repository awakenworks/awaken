#[ignore = "requires external llm/provider env"]
#[tokio::test]
async fn e2e_http_matrix_placeholder_guarded() {
    assert!(std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("DEEPSEEK_API_KEY").is_ok());
}
