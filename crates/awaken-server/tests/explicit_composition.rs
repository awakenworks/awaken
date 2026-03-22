use awaken_server::routes::build_router;

#[test]
fn router_composes_all_modules() {
    let _ = build_router();
}
