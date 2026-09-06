//! Newline-delimited JSON-RPC transport over one ACP agent channel.

use awaken_agent_channel::AgentChannel;
use serde::Serialize;
use std::collections::VecDeque;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

use crate::AcpError;

pub(super) const JSONRPC: &str = "2.0";
// This is transport admission, not a tool/result truncation policy. An over-limit
// peer fails explicitly; no partial JSON message reaches the application.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_QUEUED_FRAMES: usize = 256;

#[derive(Serialize)]
struct OutRequest<'a, P: Serialize> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: P,
}

/// Parsed inbound response, request, or notification.
#[derive(serde::Deserialize)]
pub(super) struct Incoming {
    #[serde(default)]
    pub(super) id: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) method: Option<String>,
    #[serde(default)]
    pub(super) params: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) result: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) error: Option<serde_json::Value>,
}

/// One-message-per-line transport used by the official ACP stdio connection.
pub(super) struct Wire<'a> {
    reader: BufReader<ReadHalf<&'a mut dyn AgentChannel>>,
    writer: WriteHalf<&'a mut dyn AgentChannel>,
    line: Vec<u8>,
    pending: VecDeque<Vec<u8>>,
    pending_bytes: usize,
    eof: bool,
}

impl<'a> Wire<'a> {
    pub(super) fn new(channel: &'a mut dyn AgentChannel) -> Self {
        let (reader, writer) = tokio::io::split(channel);
        Self {
            reader: BufReader::new(reader),
            writer,
            line: Vec::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            eof: false,
        }
    }

    pub(super) async fn send<P: Serialize>(&mut self, msg: &P) -> Result<(), AcpError> {
        let mut buf =
            serde_json::to_vec(msg).map_err(|error| AcpError::Frame(error.to_string()))?;
        buf.push(b'\n');
        // ACP emits unsolicited notifications, including history during load.
        // Both peers can therefore write more than a pipe's capacity at once.
        // Drain bytes while sending; only read() projects these frames, preserving
        // the existing ordered RunFactAppender and permission authority. The MCP
        // peer's spawned concurrent request handlers are not this ACP contract.
        let writing = async {
            self.writer.write_all(&buf).await?;
            self.writer.flush().await
        };
        tokio::pin!(writing);
        loop {
            tokio::select! {
                biased;
                result = &mut writing => return result.map_err(io_error),
                frame = read_frame(&mut self.reader, &mut self.line), if !self.eof => {
                    match frame? {
                        Some(frame) => {
                            if !read_ahead_admits(self.pending.len(), self.pending_bytes, frame.len())
                            {
                                return Err(AcpError::Frame("ACP read-ahead limit exceeded".into()));
                            }
                            self.pending_bytes += frame.len();
                            self.pending.push_back(frame);
                        }
                        None => self.eof = true,
                    }
                }
            }
        }
    }

    pub(super) async fn send_request<P: Serialize>(
        &mut self,
        id: u64,
        method: &str,
        params: P,
    ) -> Result<(), AcpError> {
        self.send(&OutRequest {
            jsonrpc: JSONRPC,
            id,
            method,
            params,
        })
        .await
    }

    pub(super) async fn read(&mut self) -> Result<Option<Incoming>, AcpError> {
        let frame = if let Some(frame) = self.pending.pop_front() {
            self.pending_bytes -= frame.len();
            Some(frame)
        } else if self.eof {
            None
        } else {
            read_frame(&mut self.reader, &mut self.line).await?
        };
        frame
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|error| AcpError::Frame(error.to_string()))
            })
            .transpose()
    }
}

fn read_ahead_admits(frames: usize, bytes: usize, incoming: usize) -> bool {
    frames < MAX_QUEUED_FRAMES && bytes <= MAX_FRAME_BYTES && incoming <= MAX_FRAME_BYTES - bytes
}

#[cfg(kani)]
mod proofs {
    #[kani::proof]
    fn acp_read_ahead_admission_never_overflows_or_exceeds_either_bound() {
        let frames: usize = kani::any();
        let bytes: usize = kani::any();
        let incoming: usize = kani::any();
        let allowed = super::read_ahead_admits(frames, bytes, incoming);
        let bounded = frames
            .checked_add(1)
            .is_some_and(|n| n <= super::MAX_QUEUED_FRAMES)
            && bytes
                .checked_add(incoming)
                .is_some_and(|n| n <= super::MAX_FRAME_BYTES);
        assert_eq!(allowed, bounded);
    }
}

fn io_error(error: std::io::Error) -> AcpError {
    AcpError::Io(error.to_string())
}

/// Cancellation preserves the partial line. fill_buf/consume performs no await
/// between copying bytes and consuming them; read_line's cancellation-unsafe
/// temporary UTF-8 buffer cannot discard an already received JSON prefix.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>, AcpError> {
    loop {
        let bytes = reader.fill_buf().await.map_err(io_error)?;
        let eof = bytes.is_empty();
        let end = bytes.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(bytes.len(), |index| index + 1);
        if count > MAX_FRAME_BYTES.saturating_sub(line.len()) {
            return Err(AcpError::Frame("ACP frame limit exceeded".into()));
        }
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if end.is_some() || eof {
            if !line.iter().all(u8::is_ascii_whitespace) {
                return Ok(Some(std::mem::take(line)));
            }
            line.clear();
            if eof {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn crossing_large_request_and_unsolicited_frames_preserves_wire_order() {
        // Cause/effect table: C1 both peers write beyond the 64-byte transport;
        // C2 the agent sends unsolicited frames before reading our request;
        // C3 the first frame exceeds BufReader capacity; C4 another frame and EOF.
        // D1 C1+C2+C3 => both writes finish without read/write circular wait.
        // D2 +C4 => exactly ordered, lossless frames followed by EOF. No frame is
        // projected by send(): the existing caller alone processes read() results.
        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut host, mut peer) = tokio::io::duplex(64);
            let incoming = "x".repeat(64 * 1024);
            let expected = incoming.clone();
            let remote = tokio::spawn(async move {
                for id in [1, 2] {
                    let frame = serde_json::json!({"id":id,"method":"session/update","params":{"text":incoming}});
                    peer.write_all(format!("{frame}\n").as_bytes()).await.unwrap();
                }
                let mut reader = BufReader::new(peer);
                let mut request = String::new();
                reader.read_line(&mut request).await.unwrap();
                assert_eq!(serde_json::from_str::<serde_json::Value>(&request).unwrap()["params"]["text"], "p".repeat(64 * 1024));
                reader.get_mut().shutdown().await.unwrap();
            });
            let mut wire = Wire::new(&mut host);
            wire.send_request(3, "session/prompt", serde_json::json!({"text":"p".repeat(64 * 1024)})).await.unwrap();
            for id in [1, 2] {
                let frame = wire.read().await.unwrap().unwrap();
                assert_eq!(frame.id.unwrap(), id);
                assert_eq!(frame.params.unwrap()["text"], expected);
            }
            assert!(wire.read().await.unwrap().is_none());
            remote.await.unwrap();
        }).await.expect("D1/D2 crossed writes make bounded progress");
    }

    #[tokio::test]
    async fn cancelled_read_preserves_the_partial_utf8_json_frame() {
        // D3 prefix consumed + reader cancelled + suffix/EOF => one exact frame.
        // Split within a multi-byte character, not merely between JSON tokens;
        // cancellation cannot silently discard bytes or publish a partial value.
        let (mut host, mut peer) = tokio::io::duplex(64);
        let frame = "{\"id\":7,\"result\":\"中文\"}".as_bytes();
        let split = frame.iter().position(|byte| *byte >= 128).unwrap() + 1;
        peer.write_all(&frame[..split]).await.unwrap();
        let mut wire = Wire::new(&mut host);
        let mut waiting = Box::pin(wire.read());
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(waiting);
        peer.write_all(&frame[split..]).await.unwrap();
        peer.shutdown().await.unwrap();
        let got = wire.read().await.unwrap().unwrap();
        assert_eq!(got.id.unwrap(), 7);
        assert_eq!(got.result.unwrap(), "中文");
        assert!(wire.read().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn blank_lines_eof_and_invalid_frames_keep_one_explicit_outcome() {
        // D4 blank lines + valid unterminated final frame => one value then EOF.
        // D5 malformed JSON / invalid UTF-8 => explicit frame error, never EOF.
        for (bytes, valid) in [
            (b" \n\t\r\n{\"id\":1}".as_slice(), true),
            (b"{bad}\n", false),
            (b"\xff\n", false),
        ] {
            let (mut host, mut peer) = tokio::io::duplex(64);
            peer.write_all(bytes).await.unwrap();
            peer.shutdown().await.unwrap();
            let mut wire = Wire::new(&mut host);
            if valid {
                assert_eq!(wire.read().await.unwrap().unwrap().id.unwrap(), 1);
                assert!(wire.read().await.unwrap().is_none());
            } else {
                assert!(matches!(wire.read().await, Err(AcpError::Frame(_))));
            }
        }
    }

    #[tokio::test]
    async fn unsolicited_flood_and_oversized_frame_fail_without_unbounded_buffering() {
        // D6 blocked large outbound + >256 unsolicited frames => explicit bounded
        // read-ahead failure. D7 >16MiB without a newline => bounded frame failure.
        // Neither rule truncates a tool result, authorizes a request, or invents EOF.
        for oversized in [false, true] {
            let (mut host, mut peer) = tokio::io::duplex(64);
            let remote = tokio::spawn(async move {
                if oversized {
                    let _ = peer.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]).await;
                } else {
                    for _ in 0..=MAX_QUEUED_FRAMES {
                        if peer.write_all(b"{\"id\":1}\n").await.is_err() {
                            break;
                        }
                    }
                }
                std::future::pending::<()>().await;
            });
            let mut wire = Wire::new(&mut host);
            let error =
                tokio::time::timeout(Duration::from_secs(5), wire.send(&"p".repeat(64 * 1024)))
                    .await
                    .expect("D6/D7 bounded failure")
                    .unwrap_err();
            assert!(matches!(error, AcpError::Frame(_)));
            assert!(wire.pending.len() <= MAX_QUEUED_FRAMES);
            assert!(wire.pending_bytes <= MAX_FRAME_BYTES);
            assert!(wire.line.len() <= MAX_FRAME_BYTES);
            remote.abort();
            let _ = remote.await;
        }
        // Admission boundary rows also include impossible/overflowing state,
        // not just states reached by the happy-path producer.
        for (frames, bytes, incoming, expected) in [
            (0, 0, MAX_FRAME_BYTES, true),
            (MAX_QUEUED_FRAMES - 1, MAX_FRAME_BYTES - 1, 1, true),
            (MAX_QUEUED_FRAMES, 0, 1, false),
            (0, MAX_FRAME_BYTES, 1, false),
            (0, usize::MAX, 1, false),
            (0, 1, usize::MAX, false),
        ] {
            assert_eq!(read_ahead_admits(frames, bytes, incoming), expected);
        }
    }
}
