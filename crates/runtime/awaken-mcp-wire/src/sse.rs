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
        while let Some(pos) = self.line_buf.find('\n') {
            let raw: String = self.line_buf.drain(..=pos).collect();
            let line = raw.trim_end_matches(['\r', '\n']);
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
}
