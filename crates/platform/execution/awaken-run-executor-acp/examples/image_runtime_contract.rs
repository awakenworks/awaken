fn main() {
    println!(
        "{}",
        awaken_run_executor_acp::image_runtime_contract_json()
            .expect("ACP image runtime contract must serialize")
    );
}
