//! Shared NATS connection helper.

use awaken_server_contract::contract::storage::StorageError;

pub(crate) async fn connect(
    url: &str,
    credentials: Option<&str>,
) -> Result<async_nats::Client, StorageError> {
    let credentials = credentials.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    });
    let result = match credentials {
        Some(credentials) => {
            async_nats::ConnectOptions::with_credentials(credentials)
                .map_err(|error| StorageError::Io(format!("nats credentials: {error}")))?
                .connect(url)
                .await
        }
        None => async_nats::connect(url).await,
    };
    result.map_err(|error| StorageError::Io(format!("connect: {error}")))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn invalid_credentials_fail_before_connect() {
        let error = super::connect("nats://127.0.0.1:1", Some("not a creds file"))
            .await
            .unwrap_err();
        assert!(
            format!("{error}").contains("nats credentials"),
            "invalid credentials should fail during credentials parsing"
        );
    }
}
