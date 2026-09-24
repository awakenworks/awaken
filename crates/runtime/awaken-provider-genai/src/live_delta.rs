//! Bounded coalescing of best-effort provider stream observations.

use std::time::{Duration, Instant};

#[cfg(test)]
use async_trait::async_trait;
use awaken_runtime_contract::llm::DeltaSink;

// Remote Workers publish each live delta through a claim-fenced HTTP request.
// Bound both the wait for the first visible delta and the number of requests
// made for a provider that emits one character at a time.
const LIVE_DELTA_MAX_DELAY: Duration = Duration::from_millis(75);
const LIVE_DELTA_MAX_BYTES: usize = 512;

pub(super) enum LiveDelta {
    Text(String),
    Reasoning(String),
    Tool {
        call_id: String,
        name: String,
        args: String,
    },
}

impl LiveDelta {
    fn len(&self) -> usize {
        match self {
            Self::Text(text) | Self::Reasoning(text) => text.len(),
            Self::Tool { args, .. } => args.len(),
        }
    }

    fn compatible(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(_), Self::Text(_)) | (Self::Reasoning(_), Self::Reasoning(_)) => true,
            (
                Self::Tool {
                    call_id: left_id,
                    name: left_name,
                    ..
                },
                Self::Tool {
                    call_id: right_id,
                    name: right_name,
                    ..
                },
            ) => left_id == right_id && left_name == right_name,
            _ => false,
        }
    }

    fn append(&mut self, next: Self) {
        match (self, next) {
            (Self::Text(current), Self::Text(next))
            | (Self::Reasoning(current), Self::Reasoning(next)) => current.push_str(&next),
            (Self::Tool { args: current, .. }, Self::Tool { args: next, .. }) => {
                current.push_str(&next);
            }
            _ => unreachable!("the caller checks delta compatibility"),
        }
    }
}

#[derive(Default)]
pub(super) struct BufferedLiveDelta {
    pending: Option<LiveDelta>,
    started: Option<Instant>,
}

impl BufferedLiveDelta {
    pub(super) fn remaining(&self, now: Instant) -> Option<Duration> {
        self.started
            .map(|started| (started + LIVE_DELTA_MAX_DELAY).saturating_duration_since(now))
    }

    pub(super) async fn push(
        &mut self,
        next: LiveDelta,
        sink: &dyn DeltaSink,
        observed_at: Instant,
    ) {
        if let Some(pending) = self.pending.as_mut() {
            if pending.compatible(&next)
                && pending.len().saturating_add(next.len()) <= LIVE_DELTA_MAX_BYTES
            {
                pending.append(next);
                if pending.len() >= LIVE_DELTA_MAX_BYTES {
                    self.flush(sink).await;
                }
                return;
            }
            self.flush(sink).await;
        }
        self.started = Some(observed_at);
        self.pending = Some(next);
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.len() >= LIVE_DELTA_MAX_BYTES)
        {
            self.flush(sink).await;
        }
    }

    pub(super) async fn flush(&mut self, sink: &dyn DeltaSink) {
        self.started = None;
        match self.pending.take() {
            Some(LiveDelta::Text(text)) => sink.on_text(&text).await,
            Some(LiveDelta::Reasoning(text)) => sink.on_reasoning(&text).await,
            Some(LiveDelta::Tool {
                call_id,
                name,
                args,
            }) => {
                sink.on_tool_call_delta(&call_id, &name, &args).await;
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod live_delta_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait]
    impl DeltaSink for Recorder {
        async fn on_text(&self, chunk: &str) {
            self.0.lock().unwrap().push(format!("text:{chunk}"));
        }
        async fn on_reasoning(&self, chunk: &str) {
            self.0.lock().unwrap().push(format!("reasoning:{chunk}"));
        }
        async fn on_tool_call_delta(&self, call_id: &str, tool_id: &str, args: &str) {
            self.0
                .lock()
                .unwrap()
                .push(format!("tool:{call_id}:{tool_id}:{args}"));
        }
    }

    #[tokio::test]
    async fn live_delta_buffer_preserves_kind_and_tool_boundaries() {
        // Causes: C1 adjacent text/reasoning/tool suffixes share a kind and
        // call identity; C2 the kind or tool call changes; C3 the stream ends.
        // Effects: E1 same-kind bytes coalesce; E2 boundaries preserve exact
        // order and tool identity; E3 the terminal flush leaves no partial.
        // Rules B1=C1=>E1, B2=C2=>E2, B3=C3=>E3. This is live observation
        // only: the provider SDK still owns the committed response.
        let sink = Recorder::default();
        let mut live = BufferedLiveDelta::default();
        let observed_at = Instant::now();
        live.push(LiveDelta::Text("a".into()), &sink, observed_at)
            .await;
        live.push(LiveDelta::Text("中".into()), &sink, observed_at)
            .await;
        live.push(LiveDelta::Reasoning("r".into()), &sink, observed_at)
            .await;
        live.push(LiveDelta::Reasoning("s".into()), &sink, observed_at)
            .await;
        live.push(
            LiveDelta::Tool {
                call_id: "one".into(),
                name: "ask".into(),
                args: "{".into(),
            },
            &sink,
            observed_at,
        )
        .await;
        live.push(
            LiveDelta::Tool {
                call_id: "one".into(),
                name: "ask".into(),
                args: "}".into(),
            },
            &sink,
            observed_at,
        )
        .await;
        live.push(
            LiveDelta::Tool {
                call_id: "two".into(),
                name: "ask".into(),
                args: "[]".into(),
            },
            &sink,
            observed_at,
        )
        .await;
        live.flush(&sink).await;
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            [
                "text:a中",
                "reasoning:rs",
                "tool:one:ask:{}",
                "tool:two:ask:[]",
            ]
        );
    }

    #[tokio::test]
    async fn live_delta_buffer_bounds_bytes_without_losing_a_prefix() {
        // Causes: C1 adjacent UTF-8 text reaches the byte ceiling; C2 more
        // text follows; C3 the model fails after that partial prefix. Effects:
        // E1 the ceiling flushes one bounded observation; E2 the failure flush
        // sends the exact remaining prefix. Rule B4=C1+C2+C3=>E1+E2.
        let sink = Recorder::default();
        let mut live = BufferedLiveDelta::default();
        let observed_at = Instant::now();
        live.push(LiveDelta::Text("x".repeat(256)), &sink, observed_at)
            .await;
        live.push(LiveDelta::Text("y".repeat(256)), &sink, observed_at)
            .await;
        assert_eq!(sink.0.lock().unwrap().len(), 1, "ceiling flushes promptly");
        live.push(LiveDelta::Text("尾".into()), &sink, observed_at)
            .await;
        live.flush(&sink).await;
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            [
                format!("text:{}{}", "x".repeat(256), "y".repeat(256)),
                "text:尾".into(),
            ]
        );
    }
}
