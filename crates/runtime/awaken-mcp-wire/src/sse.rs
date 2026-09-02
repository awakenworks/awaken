//! Incremental Server-Sent Events parser.
//!
//! Streamable HTTP delivers server->client JSON-RPC messages as SSE, both in a
//! POST response body and over a standalone GET stream. This parser turns a
//! byte/text stream (arriving in arbitrary chunks) into the `data` payload of
//! each completed event, so the HTTP transport can route them through the same
//! demux the stdio transport uses. It is pure — fed synthetic chunks under test,
//! no network.

/// Accumulates SSE input across chunks and yields completed events' data.
#[derive(Default)]
pub struct SseParser {
    /// Bytes received but not yet terminated by a newline.
    line_buf: String,
    /// `data:` lines accumulated for the event currently being built.
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a text chunk; return the `data` payload of every event completed by
    /// it. An event completes on a blank line (SSE dispatch), with its multiple
    /// `data:` lines joined by newlines. Non-`data` fields (`event:`, `id:`,
    /// `retry:`, comments) are ignored.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.line_buf.push_str(chunk);
        let mut events = Vec::new();
        while let Some((pos, terminator_len)) = next_line_ending(&self.line_buf) {
            let end = pos
                .checked_add(terminator_len)
                .expect("SSE line terminator offset overflowed");
            assert!(
                end > pos && end <= self.line_buf.len(),
                "SSE line terminator must consume one or two bytes"
            );
            let raw: String = self.line_buf.drain(..end).collect();
            let line = &raw[..pos];
            if line.is_empty() {
                // Blank line: dispatch the accumulated event, if any.
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                // A single optional leading space after the colon is stripped.
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).to_string());
            }
            // Other fields and `:` comments are ignored.
        }
        events
    }

    /// Finish an ended stream, treating EOF as the delimiter for its final
    /// complete event. This is the single termination rule shared by buffered,
    /// POST-stream, and standalone GET-stream consumers.
    pub fn finish(&mut self) -> Vec<String> {
        self.push("\n\n")
    }
}

/// Locate LF, CRLF, or bare CR without consuming a CR at the end of a chunk:
/// the next chunk may begin with LF and the pair is one terminator.
fn next_line_ending(input: &str) -> Option<(usize, usize)> {
    let bytes = input.as_bytes();
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'\n' => return Some((index, 1)),
            b'\r' if index + 1 == bytes.len() => return None,
            b'\r' => return Some((index, usize::from(bytes[index + 1] == b'\n') + 1)),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_event_yields_its_data() {
        let mut parser = SseParser::new();
        let events = parser.push("event: message\ndata: {\"a\":1}\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn data_without_leading_space_is_kept() {
        let mut parser = SseParser::new();
        assert_eq!(parser.push("data:{\"a\":1}\n\n"), vec!["{\"a\":1}"]);
    }

    #[test]
    fn multiple_data_lines_join_with_newlines() {
        let mut parser = SseParser::new();
        let events = parser.push("data: line1\ndata: line2\n\n");
        assert_eq!(events, vec!["line1\nline2".to_string()]);
    }

    #[test]
    fn an_event_split_across_chunks_completes_later() {
        let mut parser = SseParser::new();
        assert!(parser.push("data: {\"partia").is_empty());
        assert!(parser.push("l\":true}").is_empty());
        // Only the terminating blank line dispatches the event.
        let events = parser.push("\n\n");
        assert_eq!(events, vec!["{\"partial\":true}".to_string()]);
    }

    #[test]
    fn eof_finishes_a_complete_event_without_a_blank_delimiter() {
        // Causes: C1 a data field is complete; C2 a blank delimiter is absent;
        // C3 EOF occurs. Effect E1 dispatches the data exactly once. Decision
        // rule F1 C1+!C2+C3 -> E1. Constraint: incomplete non-data input never
        // creates an event. FMECA: losing F1 discards a successful JSON-RPC
        // response and can trigger a duplicate side-effecting tool retry.
        let mut parser = SseParser::new();
        assert!(
            parser
                .push("event: message\ndata: {\"ok\":true}")
                .is_empty()
        );
        assert_eq!(parser.finish(), vec!["{\"ok\":true}".to_string()]);
        assert!(parser.finish().is_empty(), "EOF is idempotent");
    }

    #[test]
    fn two_events_in_one_chunk() {
        let mut parser = SseParser::new();
        let events = parser.push("data: a\n\ndata: b\n\n");
        assert_eq!(events, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn comments_and_other_fields_are_ignored() {
        let mut parser = SseParser::new();
        let events = parser.push(": keep-alive\nid: 7\nevent: message\ndata: x\n\n");
        assert_eq!(events, vec!["x".to_string()]);
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let mut parser = SseParser::new();
        let events = parser.push("data: y\r\n\r\n");
        assert_eq!(events, vec!["y".to_string()]);
    }

    #[test]
    fn blank_lines_without_data_yield_no_event() {
        // Keep-alive blank lines with no preceding `data:` must not dispatch a
        // spurious empty event.
        let mut parser = SseParser::new();
        assert!(parser.push("\n\n\n").is_empty());
    }

    #[test]
    fn event_field_without_data_dispatches_nothing() {
        // An event that carries only non-`data` fields produces no payload.
        let mut parser = SseParser::new();
        assert!(parser.push("event: message\nid: 1\n\n").is_empty());
    }

    #[test]
    fn only_one_leading_space_after_colon_is_stripped() {
        // SSE strips exactly one optional space after `data:`; a second space is
        // part of the payload.
        let mut parser = SseParser::new();
        assert_eq!(parser.push("data:  x\n\n"), vec![" x".to_string()]);
    }

    #[test]
    fn empty_data_line_dispatches_an_empty_payload() {
        // `data:` with an empty value is still a data line, so the terminating
        // blank line dispatches an (empty) event.
        let mut parser = SseParser::new();
        assert_eq!(parser.push("data:\n\n"), vec![String::new()]);
    }

    #[test]
    fn parsing_is_independent_of_chunk_boundaries() {
        // Property: the incremental parser must yield the same event sequence no
        // matter where the byte stream is split. A fixed input mixing a comment,
        // an `event:` field, CRLF, multi-`data` joining, a keep-alive blank line,
        // and two events; fed split at EVERY offset. (No proptest dev-dep, so a
        // manual all-offsets loop; the input is ASCII, so every offset is a valid
        // char boundary.)
        let input = ": keep-alive\nevent: message\r\ndata: {\"a\":1}\r\n\r\ndata: l1\ndata: l2\n\n\ndata: {\"b\":2}\n\n";

        // The whole-input parse is the oracle.
        let expected = SseParser::new().push(input);
        assert_eq!(
            expected,
            vec![
                "{\"a\":1}".to_string(),
                "l1\nl2".to_string(),
                "{\"b\":2}".to_string(),
            ],
            "sanity: the oracle parse yields the three expected events",
        );

        for split in 0..=input.len() {
            let (head, tail) = input.split_at(split);
            let mut parser = SseParser::new();
            let mut events = parser.push(head);
            events.extend(parser.push(tail));
            assert_eq!(
                events, expected,
                "splitting at offset {split} changed the parse"
            );
        }
    }

    #[test]
    fn bare_cr_and_split_crlf_are_exact_line_terminators() {
        /* Cause/effect graph: C1 SSE uses bare CR delimiters; C2 CRLF is split
         * across chunks; C3 a CR is the final byte currently observed; C4 two
         * data fields in one event use CRLF. Effects: E1 C1 dispatches the
         * complete event; E2 C2 remains one delimiter; E3 C3 waits for the next
         * byte/EOF rather than fabricating a line; E4 C4 consumes both CRLF
         * bytes and joins the fields. Decision S1=C1->E1; S2=C2->E2;
         * S3=C3->E3; S4=C4->E4. Losing S1/S2/S4 can turn a received MCP
         * response into SentUnknown and cause an unsafe retry. */
        let mut parser = SseParser::new();
        assert!(parser.push("data: x\rdata: y\r\r").is_empty(), "S3");
        assert_eq!(parser.finish(), vec!["x\ny"], "S1");

        let mut split = SseParser::new();
        assert!(split.push("data: z\r").is_empty(), "S3");
        assert_eq!(split.push("\n\r\n"), vec!["z"], "S2");

        let mut joined = SseParser::new();
        assert_eq!(
            joined.push("data: first\r\ndata: second\r\n\r\n"),
            vec!["first\nsecond"],
            "S4"
        );
    }
}
