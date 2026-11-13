use super::*;

fn request_for_raw_argument_budget(
    budget: usize,
) -> Result<http::Request<Vec<u8>>, kube::core::request::Error> {
    let argv = vec!["true".into(), "/".repeat(budget - "true".len() - 2)];
    let command = pc::MaterializedCommand {
        argv,
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: Vec::new(),
        stdio: pc::Stdio::Null,
    };
    let plan = k8s_exec_argv("uri-budget-fixture", command).unwrap();
    kube::core::Request::new(format!("/api/v1/namespaces/{}/pods", "n".repeat(63))).exec(
        &"p".repeat(253),
        plan.argv,
        &AttachParams::default()
            .container("agent")
            .stdout(true)
            .stderr(false),
    )
}

#[test]
fn raw_64k_argv_reproduces_the_real_kubernetes_uri_rejection() {
    // Consumer rule U1: literal '/' expands to %2F in command query values;
    // the real process wrapper and kube/http request builders must reproduce
    // the deployed failure before any API call or container effect occurs.
    let error = request_for_raw_argument_budget(64 * 1024).unwrap_err();
    assert!(error.to_string().contains("uri too long"), "U1: {error}");
}

#[test]
fn portable_16k_argv_fits_the_real_wrapped_kubernetes_request() {
    // U2: a complete 16 KiB literal argv, worst-case percent expansion, maximum
    // Pod/Namespace names and the ACTUAL production exec wrapper still fit the
    // SDK's URI ceiling. This independently verifies the producer's reserved
    // headroom; it does not replace status/fence/byte-integrity acceptance.
    let request = request_for_raw_argument_budget(16 * 1024).unwrap();
    assert!(request.uri().to_string().len() < 65_534, "U2");
}
