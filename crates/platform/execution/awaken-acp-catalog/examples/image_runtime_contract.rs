fn main() {
    println!(
        "{}",
        awaken_acp_catalog::image_runtime_contract_json()
            .expect("ACP image runtime contract must serialize")
    );
}
