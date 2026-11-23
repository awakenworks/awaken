//! Qualify the URL implementation already re-exported by the HTTP client.

#[test]
fn nested_blob_origin_is_bounded_in_an_isolated_process() {
    // C1 ordinary HTTP(S), C2 one blob: over HTTP(S), C3 a nested or other
    // inner scheme, C4 10,000 nested prefixes on a normal 2 MiB thread stack.
    // R1 C1/C2 -> preserve the canonical tuple origin. R2 C3/C4 -> opaque
    // origin with bounded stack use, never recursive descent/process abort.
    // RUSTSEC-2026-0592 aborts the process rather than unwinding; a real child
    // is the smallest faithful failure boundary. The ignored test is invoked
    // explicitly here, not omitted from the regression. No parser or transport
    // fallback is implemented locally; the upstream URL crate owns the fix.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "url_origin_probe", "--ignored", "--nocapture"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "URL origin probe failed: {:?}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("url-origin-probe-complete"),
        "the child must execute the actual probe, not pass an empty test selection"
    );
}

#[test]
#[ignore = "private subprocess probe invoked by nested_blob_origin_is_bounded_in_an_isolated_process"]
fn url_origin_probe() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let deeply_nested = format!("{}https://example.com/", "blob:".repeat(10_000));
            assert!(
                !reqwest::Url::parse(&deeply_nested)
                    .unwrap()
                    .origin()
                    .is_tuple(),
                "R2 deep blob"
            );
            for (input, expected) in [
                ("https://EXAMPLE.com:443/a", "https://example.com"),
                ("http://example.com:8080/a", "http://example.com:8080"),
                ("blob:https://EXAMPLE.com:443/a", "https://example.com"),
                ("blob:http://example.com:8080/a", "http://example.com:8080"),
            ] {
                assert_eq!(
                    reqwest::Url::parse(input)
                        .unwrap()
                        .origin()
                        .ascii_serialization(),
                    expected,
                    "R1 {input}"
                );
            }
            for input in [
                "blob:blob:https://example.com/",
                "blob:ws://example.com/",
                "blob:data:text/plain,fixture",
            ] {
                assert!(
                    !reqwest::Url::parse(input).unwrap().origin().is_tuple(),
                    "R2 {input}"
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
    println!("url-origin-probe-complete");
}
