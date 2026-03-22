#[ignore = "requires phoenix OTLP endpoint"]
#[tokio::test]
async fn phoenix_observability_e2e_placeholder_guarded() {
    assert!(
        std::env::var("PHOENIX_BASE_URL").is_ok()
            || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
    );
}
