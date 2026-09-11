//! One test-only PostgreSQL configuration contract for the HTTP gate.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn database_url() -> Option<String> {
    match std::env::var("AWAKEN_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => {
            eprintln!("AWAKEN_TEST_DATABASE_URL absent; skipping PostgreSQL branch");
            None
        }
        Err(_) => panic!("configured PostgreSQL URL must be Unicode"),
    }
}

pub async fn connect<T, E: std::fmt::Debug>(
    connection: impl std::future::Future<Output = Result<T, E>>,
) -> T {
    let connected = tokio::time::timeout(Duration::from_secs(5), connection)
        .await
        .expect("configured PostgreSQL connection exceeded its test deadline")
        .expect("configured PostgreSQL must connect and initialize successfully");
    eprintln!("HTTP PostgreSQL fixture: real adapter connected");
    connected
}

pub fn assert_configuration_contract(tests: &[&str]) {
    // Cause/effect graph: C1 canonical PG URL absent/invalid; C2 actual test entry;
    // C3 independent child environment (never mutate a threaded test process).
    // R1=!C1(configured) => explicit skip, success, no default database access.
    // R2=C1(invalid) => failure identifying configured PostgreSQL, never skip.
    // Every caller supplies its actual production-adapter test(s), so changing
    // a caller back to an alternate environment name also breaks this test.
    for test in tests {
        // R3 valid URL + unreachable server => bounded failure, never skip.
        // Hold a private listener: the address is owned by this
        // parent and cannot accidentally select a service on a well-known port.
        let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        // A TCP listener that never speaks PostgreSQL also exercises timeout.
        let unreachable = format!(
            "postgres://fixture:fixture@{}/fixture",
            unavailable.local_addr().unwrap()
        );
        for url in [
            Some("invalid-postgres-fixture"),
            Some(unreachable.as_str()),
            None,
        ] {
            let configured = url.is_some();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", test, "--nocapture"])
                .env_remove("AWAKEN_TEST_DATABASE_URL")
                .env_remove("AWAKEN_TEST_POSTGRES_URL")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(url) = url {
                command.env("AWAKEN_TEST_DATABASE_URL", url);
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "{test}: configuration contract exceeded deadline: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                output.status.success(),
                !configured,
                "{test}: configured={configured}: {text}"
            );
            assert!(
                text.contains(if configured {
                    "configured PostgreSQL"
                } else {
                    "AWAKEN_TEST_DATABASE_URL absent"
                }),
                "{test}: {text}"
            );
        }
    }
}
