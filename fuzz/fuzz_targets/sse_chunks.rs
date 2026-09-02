#![no_main]

use awaken_mcp_wire::SseParser;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let mut whole = SseParser::new();
    let mut expected = whole.push(input);
    expected.extend(whole.finish());

    // Metamorphic oracle: every valid UTF-8 split must produce the same event
    // sequence as one whole chunk. This covers CR/LF/CRLF boundaries and EOF.
    for split in (0..=input.len()).filter(|split| input.is_char_boundary(*split)) {
        let mut chunked = SseParser::new();
        let mut observed = chunked.push(&input[..split]);
        observed.extend(chunked.push(&input[split..]));
        observed.extend(chunked.finish());
        assert_eq!(observed, expected);
    }
});
