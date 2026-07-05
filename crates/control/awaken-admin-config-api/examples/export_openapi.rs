//! Emit the OpenAPI 3.1 contract for the admin config plane to stdout. This is
//! what `scripts/contract/generate-contracts.sh` writes to `contracts/openapi.json`
//! — the input for frontend codegen (openapi-typescript), mirroring
//! oversight-next's `export_openapi`.
//!
//! ```sh
//! cargo run -p awaken-admin-config-api --features schema --example export_openapi
//! ```

fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&awaken_admin_config_api::openapi::openapi_document())
            .expect("document serializes")
    );
}
