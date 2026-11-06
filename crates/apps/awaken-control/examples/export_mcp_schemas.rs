fn main() {
    println!("{}",serde_json::to_string_pretty(&serde_json::json!({"schemas":awaken_control::mcp_contract_schemas(),"paths":awaken_control::mcp_contract_paths()})).expect("schemas serialize"));
}
