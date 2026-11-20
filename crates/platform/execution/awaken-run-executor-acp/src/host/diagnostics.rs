// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Limits untrusted CLI diagnostics to bounded text and removes credential-bearing lines and opaque values before reporting.
//! Bounded summaries for untrusted CLI output; never persist credential output.

pub fn diagnostic_summary(input: &str) -> String {
    let bounded: String = input.chars().take(4096).collect();
    bounded
        .lines()
        .take(8)
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if [
                "token",
                "secret",
                "password",
                "authorization",
                "api_key",
                "api-key",
                "cookie",
                "sk-",
            ]
            .iter()
            .any(|key| lower.contains(key))
                || (line.contains("://") && line.contains('@'))
            {
                return "[credential-bearing diagnostic redacted]".to_string();
            }
            line.split_whitespace()
                .map(|word| {
                    if word.len() >= 32
                        && word
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_-=.".contains(&c))
                    {
                        "[opaque value redacted]"
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_keep_failure_context_without_credential_values() {
        let summary = diagnostic_summary(
            "initialize: invalid protocol\nAuthorization: Bearer private\nhttps://user:private@registry.invalid\napi_key=private\nABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        );
        assert!(summary.contains("initialize: invalid protocol"));
        assert!(!summary.contains("private"));
        assert!(!summary.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        let unicode = diagnostic_summary(&"错误".repeat(5000));
        assert!(unicode.chars().count() <= 4096);
    }
}
