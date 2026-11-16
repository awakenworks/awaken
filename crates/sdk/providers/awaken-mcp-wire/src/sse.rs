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
    line_buf: Vec<u8>,
    /// `data:` lines accumulated for the event currently being built.
    data: Option<String>,
    frame_bytes: usize,
    pending_cr: bool,
    invalid_event: bool,
    exceeded_limit: bool,
    #[cfg(test)]
    frame_limit: Option<usize>,
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
        self.push_bytes(chunk.as_bytes())
    }

    /// Transport frames may divide a UTF-8 codepoint anywhere. Decode only
    /// complete lines; an invalid line rejects its event rather than inventing
    /// replacement characters in customer text or protocol identities.
    /// After an oversized event, the parser yields only events completed
    /// before the failure and ignores subsequent input on this stream.
    pub fn push_bytes(&mut self, mut chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        while !chunk.is_empty() && !self.exceeded_limit {
            if self.pending_cr {
                self.pending_cr = false;
                let crlf = chunk[0] == b'\n';
                self.finish_line(usize::from(crlf), &mut events);
                chunk = &chunk[usize::from(crlf)..];
                continue;
            }
            let end = chunk
                .iter()
                .position(|byte| matches!(byte, b'\r' | b'\n'))
                .unwrap_or(chunk.len());
            if !self.charge(end) {
                break;
            }
            self.line_buf.extend_from_slice(&chunk[..end]);
            let Some(delimiter) = chunk.get(end) else {
                break;
            };
            self.pending_cr = *delimiter == b'\r';
            if self.pending_cr {
                if !self.line_buf.is_empty() && !self.charge(1) {
                    break;
                }
            } else {
                self.finish_line(1, &mut events);
            }
            chunk = &chunk[end + 1..];
        }
        events
    }

    /// Whether this stream exceeded the raw SSE event byte limit.
    pub fn exceeded_limit(&self) -> bool {
        self.exceeded_limit
    }

    fn charge(&mut self, bytes: usize) -> bool {
        let limit = crate::MAX_FRAME_BYTES;
        #[cfg(test)]
        let limit = self.frame_limit.unwrap_or(limit);
        if bytes > limit - self.frame_bytes {
            self.exceeded_limit = true;
            self.line_buf.clear();
            self.data = None;
            return false;
        }
        self.frame_bytes += bytes;
        true
    }

    fn finish_line(&mut self, delimiter_bytes: usize, events: &mut Vec<String>) {
        if self.line_buf.is_empty() {
            if let Some(data) = self.data.take()
                && !self.invalid_event
            {
                events.push(data);
            }
            self.invalid_event = false;
            self.frame_bytes = 0;
            return;
        }
        if !self.charge(delimiter_bytes) {
            return;
        }
        match std::str::from_utf8(&self.line_buf) {
            Ok(line) => {
                if let Some(value) = line.strip_prefix("data:") {
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    if let Some(data) = &mut self.data {
                        data.push('\n');
                        data.push_str(value);
                    } else {
                        self.data = Some(value.to_owned());
                    }
                }
            }
            Err(_) => self.invalid_event = true,
        }
        self.line_buf.clear();
    }

    /// Finish an ended stream, treating EOF as the delimiter for its final
    /// complete event. This is the single termination rule shared by buffered,
    /// POST-stream, and standalone GET-stream consumers.
    pub fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.exceeded_limit {
            self.pending_cr = false;
            self.finish_line(0, &mut events);
            self.finish_line(0, &mut events);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_event_limit_counts_fields_and_rejects_before_a_delimiter() {
        let mut parser = SseParser::new();
        // Exercise the same accounting under Miri without interpreting
        // several full-size frames byte by byte.
        let limit = if cfg!(miri) {
            let limit = 64;
            parser.frame_limit = Some(limit);
            limit
        } else {
            crate::MAX_FRAME_BYTES
        };
        // Two complete events can share a chunk even if the chunk is over the cap.
        let prefix = b": ignored\r\ndata: ok";
        let mut exact = prefix.to_vec();
        exact.resize(limit - 2, b' ');
        exact.extend_from_slice(b"\r\n\r\n");
        let events = parser.push_bytes(&[exact.as_slice(), exact.as_slice()].concat());
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|data| data.trim_end() == "ok"));
        assert!(!parser.exceeded_limit());

        assert!(parser.push("data: first\r\n: padding ").is_empty());
        assert!(parser.push_bytes(&vec![b'x'; limit]).is_empty());
        assert!(parser.exceeded_limit());
        assert!(parser.push("\n\ndata: later\n\n").is_empty());
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn all_utf8_byte_boundaries_preserve_events_and_invalid_input_never_becomes_text() {
        // U1 valid multibyte data split at EVERY byte -> exact same payload;
        // U2 one-byte chunks -> same result; U3 invalid/truncated UTF-8 -> no
        // invented text; U4 next valid event survives a rejected predecessor.
        let data = "data: {\"text\":\"预算五万元🙂\"}\r\n\r\n";
        let expected = vec!["{\"text\":\"预算五万元🙂\"}".to_owned()];
        for split in 0..=data.len() {
            let mut parser = SseParser::new();
            let mut events = parser.push_bytes(&data.as_bytes()[..split]);
            events.extend(parser.push_bytes(&data.as_bytes()[split..]));
            assert_eq!(events, expected, "U1 byte {split}");
        }
        let mut parser = SseParser::new();
        let events: Vec<_> = data
            .as_bytes()
            .chunks(1)
            .flat_map(|b| parser.push_bytes(b))
            .collect();
        assert_eq!(events, expected, "U2");
        assert!(parser.push_bytes(b"data: \xff\n\n").is_empty(), "U3");
        assert_eq!(parser.push_bytes(data.as_bytes()), expected, "U4");
        assert!(parser.push_bytes(b"data: \xe4").is_empty());
        assert!(parser.finish().is_empty(), "U3 truncated EOF");
    }

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
