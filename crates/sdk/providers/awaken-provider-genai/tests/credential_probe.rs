//! Live credential-validation probe (ADR-0043 `CredentialValidation`). Ignored by
//! default; needs network + a live Anthropic-compatible key. Run with:
//!
//! ```sh
//! KIMI_API_KEY=sk-... KIMI_BASE_URL=https://api.kimi.com/coding/v1/ \
//! KIMI_MODEL=kimi-for-coding \
//! cargo test -p awaken-provider-genai --test credential_probe -- --ignored --nocapture
//! ```
//!
//! Proves both arms against the real endpoint: the real key probes `Valid`, and a
//! bogus key probes `Invalid` (never a false `Valid`).

use awaken_provider_genai::{CredentialProbe, probe_credential};

fn env() -> (String, String, String) {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| {
            if base.contains("api.kimi.com/coding") {
                "kimi-for-coding".to_string()
            } else {
                "claude-3-5-haiku-latest".to_string()
            }
        });
    (key, base, model)
}

#[tokio::test]
#[ignore = "requires network and a live key"]
async fn real_key_probes_valid() {
    let (key, base, model) = env();
    let outcome = probe_credential(base, key, &model).await;
    assert_eq!(
        outcome,
        CredentialProbe::Valid,
        "a real key must probe Valid"
    );
}

#[tokio::test]
#[ignore = "requires network and a live key"]
async fn bogus_key_probes_invalid() {
    let (_key, base, model) = env();
    let outcome = probe_credential(base, "sk-obviously-not-a-real-key", &model).await;
    assert_eq!(
        outcome,
        CredentialProbe::Invalid,
        "a bogus key must probe Invalid, never a false Valid"
    );
}
