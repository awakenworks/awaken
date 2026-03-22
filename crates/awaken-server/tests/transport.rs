use awaken_server::http_sse::format_sse_data;

#[test]
fn transport_sse_line_format() {
    let chunk = format_sse_data("{\"x\":1}");
    let s = String::from_utf8(chunk.to_vec()).unwrap();
    assert!(s.starts_with("data: "));
    assert!(s.ends_with("\n\n"));
}
