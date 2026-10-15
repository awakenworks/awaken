// @formal surface exclude=observability_adapter
// @formal owner crates/runtime/awaken-observability/src/otel.rs
// @formal reason: Configures external trace exporters and a JSON-lines diagnostic sink; failures
// @formal reason: intentionally affect telemetry only and never authorize or mutate a product aggregate.
//! Exporter wiring from typed deployment configuration, with shutdown flushing.
//!
//! Two collector sinks are supported, in priority order:
//!   1. `trace_file` — bounded, asynchronous JSON-line diagnostics. This
//!      is the collector-free sink the e2e trace validator reads back, so span
//!      trees can be asserted without a running OTLP collector.
//!   2. configured OTLP endpoint — standard OTLP/HTTP
//!      export over a Tokio batch span processor (Phoenix / Jaeger / collector).
//!
//! When neither is configured the layer is `None` and the process keeps the
//! fmt-only logging path. The W3C `traceparent` propagator is installed
//! unconditionally so an inbound trace continues across plane boundaries.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use tracing_subscriber::registry::LookupSpan;

use crate::config::{ObservabilityConfig, OtelConfig, OtelProtocol};

/// Kept for the process lifetime so the batch span processor keeps exporting;
/// also the handle [`shutdown`] uses to flush buffered spans on exit.
static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Build the OpenTelemetry layer when a trace sink is configured. Generic over the
/// subscriber `S` so the concrete layered type is inferred at the call site.
/// Returns `None` when no sink is configured (fmt-only path).
pub(crate) fn build_layer<S>(
    observability: &ObservabilityConfig,
) -> Option<tracing_opentelemetry::OpenTelemetryLayer<S, SdkTracer>>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    // Install the W3C propagator regardless of sink so inbound/outbound context is
    // carried across plane boundaries (ingress → runtime → provider → tool).
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    if let Some(path) = &observability.trace_file {
        let exporter = file_exporter::JsonFileSpanExporter::new(path.as_os_str().to_owned());
        let provider = file_exporter::provider(exporter);
        let tracer = provider.tracer("awaken-observability");
        opentelemetry::global::set_tracer_provider(provider.clone());
        let _ = PROVIDER.set(provider);
        return Some(tracing_opentelemetry::layer().with_tracer(tracer));
    }

    let config = &observability.otel;
    if !config.is_configured() {
        return None;
    }
    let (provider, tracer) = match init_otlp_tracer(config) {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!(%error, "OTLP tracer init failed; continuing without trace export");
            return None;
        }
    };
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = PROVIDER.set(provider);
    Some(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Initialise an OTLP/HTTP tracer from the given configuration. Returns the
/// provider (kept alive by the caller) and a tracer for the subscriber layer.
pub fn init_otlp_tracer(
    config: &OtelConfig,
) -> Result<(SdkTracerProvider, SdkTracer), Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::runtime;
    use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;

    // Only HTTP/protobuf is compiled in (the `http-proto` exporter feature); a grpc
    // request would need the tonic transport, so we fall back to HTTP. Surface the
    // downgrade so a `grpc` misconfiguration is observable instead of silent.
    if matches!(config.effective_traces_protocol(), OtelProtocol::Grpc) {
        tracing::warn!(
            "OTLP traces protocol 'grpc' requested but only http/protobuf is compiled in; \
             exporting traces over HTTP"
        );
    }

    let endpoint = config
        .effective_traces_endpoint()
        .ok_or("No OTLP endpoint configured")?;

    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()?;

    let mut resource_attrs = vec![];
    if let Some(name) = &config.service_name {
        resource_attrs.push(KeyValue::new("service.name", name.clone()));
    }
    if let Some(version) = &config.service_version {
        resource_attrs.push(KeyValue::new("service.version", version.clone()));
    }

    let batch = BatchSpanProcessor::builder(exporter, runtime::Tokio).build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(batch)
        .with_resource(Resource::builder().with_attributes(resource_attrs).build())
        .build();

    let tracer = provider.tracer("awaken-observability");
    Ok((provider, tracer))
}

/// Flush and shut down the exporter, if one was installed, so buffered spans are
/// delivered before the process exits. Like the meter's [`shutdown_meter`], the
/// flush + shutdown run on a detached OS thread with a bounded wait: a stalled OTLP
/// export (e.g. a collector that vanished at teardown) must never hang process
/// exit. Freeing the calling thread also lets the runtime's workers drive the async
/// batch export, so a reachable collector still receives the final spans.
///
/// [`shutdown_meter`]: crate::metrics::shutdown_meter
pub(crate) fn shutdown() {
    let Some(provider) = PROVIDER.get().cloned() else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = provider.force_flush();
        let _ = provider.shutdown();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
}

/// A collector-free `SpanExporter` that appends each finished span to a file as one
/// JSON line (`{name, trace_id, span_id, parent_span_id, attributes}`).
mod file_exporter {
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::Mutex;

    use opentelemetry::trace::SpanId;
    use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};

    const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

    pub(super) fn provider(exporter: impl SpanExporter + 'static) -> super::SdkTracerProvider {
        use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor};
        let config = BatchConfigBuilder::default()
            .with_max_queue_size(512)
            .with_max_export_batch_size(128)
            .build();
        super::SdkTracerProvider::builder()
            .with_span_processor(
                BatchSpanProcessor::builder(exporter)
                    .with_batch_config(config)
                    .build(),
            )
            .build()
    }

    #[derive(Debug)]
    pub(crate) struct JsonFileSpanExporter {
        path: OsString,
        max_bytes: u64,
        // Serialize concurrent batch writes so JSON lines never interleave.
        lock: Mutex<()>,
    }

    impl JsonFileSpanExporter {
        pub(crate) fn new(path: OsString) -> Self {
            Self {
                path,
                max_bytes: MAX_FILE_BYTES,
                lock: Mutex::new(()),
            }
        }

        fn write_batch(&self, batch: &[SpanData]) -> std::io::Result<()> {
            self.write_lines(batch.iter().map(|span| {
                let attrs: serde_json::Map<String, serde_json::Value> = span
                    .attributes
                    .iter()
                    .map(|kv| (kv.key.to_string(), serde_json::Value::String(kv.value.to_string())))
                    .collect();
                let parent = span.parent_span_id;
                let line = serde_json::json!({
                    "name": span.name.as_ref(),
                    "trace_id": format!("{:032x}", u128::from_be_bytes(span.span_context.trace_id().to_bytes())),
                    "span_id": format!("{:016x}", u64::from_be_bytes(span.span_context.span_id().to_bytes())),
                    "parent_span_id": if parent == SpanId::INVALID {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(format!("{:016x}", u64::from_be_bytes(parent.to_bytes())))
                    },
                    "attributes": attrs,
                });
                let mut bytes = serde_json::to_vec(&line)?;
                bytes.push(b'\n');
                Ok(bytes)
            }))
        }

        fn write_lines(
            &self,
            lines: impl IntoIterator<Item = std::io::Result<Vec<u8>>>,
        ) -> std::io::Result<()> {
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut rollover = self.path.clone();
            rollover.push(".1");
            // An older append-only sink may already exceed the new retention
            // budget. Retain no oversized diagnostic file across this upgrade.
            match std::fs::metadata(&rollover) {
                Ok(metadata) if metadata.len() > self.max_bytes => std::fs::remove_file(&rollover)?,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            let mut size = file.metadata()?.len();
            if size > self.max_bytes {
                // Windows append-only access cannot truncate. Keep append
                // semantics for the writer and use a short-lived write handle
                // for legacy cleanup under the same exporter mutex.
                OpenOptions::new()
                    .write(true)
                    .open(&self.path)?
                    .set_len(0)?;
                size = 0;
            }
            for line in lines {
                let line = line?;
                let length = u64::try_from(line.len()).map_err(std::io::Error::other)?;
                if length > self.max_bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "trace record exceeds file retention budget",
                    ));
                }
                if size > self.max_bytes - length {
                    file.flush()?;
                    drop(file);
                    match std::fs::remove_file(&rollover) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                    std::fs::rename(&self.path, &rollover)?;
                    file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)?;
                    size = 0;
                }
                file.write_all(&line)?;
                size += length;
            }
            file.flush()
        }
    }

    impl SpanExporter for JsonFileSpanExporter {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            let result = self
                .write_batch(&batch)
                .map_err(|e| OTelSdkError::InternalFailure(e.to_string()));
            async move { result }
        }
    }

    #[cfg(test)]
    mod retention_tests {
        use super::*;

        fn fixture() -> (std::path::PathBuf, JsonFileSpanExporter) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "awaken-trace-retention-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let mut exporter = JsonFileSpanExporter::new(path.join("trace.jsonl").into_os_string());
            exporter.max_bytes = 8;
            (path, exporter)
        }

        #[test]
        fn capacity_decision_table_keeps_two_bounded_whole_line_files() {
            // Causes: current size S, record size R including newline, cap B.
            // R<=B and S+R<=B -> append; R<=B and S+R>B -> replace one rollover;
            // S>B -> discard legacy excess; R>B -> error with no record write.
            // Boundary rows use exactly B, B+1 and multiple rotations. Each
            // retained line must remain valid JSON, never a truncated record.
            let (dir, exporter) = fixture();
            let write = |value: &str| exporter.write_lines([Ok(value.as_bytes().to_vec())]);
            write("0\n").unwrap();
            write("1\n2\n3\n").unwrap();
            assert_eq!(
                std::fs::read_to_string(&exporter.path).unwrap(),
                "0\n1\n2\n3\n",
                "exact cap appends"
            );
            write("4\n").unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join("trace.jsonl.1")).unwrap(),
                "0\n1\n2\n3\n"
            );
            write("5\n6\n7\n").unwrap();
            write("8\n").unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join("trace.jsonl.1")).unwrap(),
                "4\n5\n6\n7\n",
                "one prior file replaces older rollover"
            );
            let before = std::fs::read(&exporter.path).unwrap();
            assert_eq!(
                write("12345678\n").unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
            assert_eq!(std::fs::read(&exporter.path).unwrap(), before);
            for path in [&exporter.path, &dir.join("trace.jsonl.1").into_os_string()] {
                let contents = std::fs::read_to_string(path).unwrap();
                assert!(contents.len() <= 8);
                for line in contents.lines() {
                    serde_json::from_str::<serde_json::Value>(line).unwrap();
                }
            }
            std::fs::write(&exporter.path, "0123456789").unwrap();
            std::fs::write(dir.join("trace.jsonl.1"), "0123456789").unwrap();
            write("9\n").unwrap();
            assert_eq!(std::fs::read_to_string(&exporter.path).unwrap(), "9\n");
            assert!(
                !dir.join("trace.jsonl.1").exists(),
                "legacy excess cannot remain in rollover"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn legacy_cleanup_preserves_bounded_rollover_and_rotates_exact_capacity() {
            // C1 current file exceeds B / equals B; C2 rollover absent / within
            // B / exceeds B. Six rows cross C1/C2. Legacy current resets before
            // append and retains only a bounded prior file; exact capacity
            // rotates its complete lines into that same prior slot. A new
            // record always appends as one complete JSON line on both OSes.
            for current in ["0\n1\n2\n3\n4\n", "0\n1\n2\n3\n"] {
                for prior in [None, Some("7\n"), Some("0\n1\n2\n3\n4\n")] {
                    let (dir, exporter) = fixture();
                    let rollover = dir.join("trace.jsonl.1");
                    std::fs::write(&exporter.path, current).unwrap();
                    if let Some(prior) = prior {
                        std::fs::write(&rollover, prior).unwrap();
                    }
                    exporter.write_lines([Ok(b"9\n".to_vec())]).unwrap();
                    assert_eq!(std::fs::read_to_string(&exporter.path).unwrap(), "9\n");
                    let expected = if current.len() == 8 {
                        Some(current)
                    } else {
                        prior.filter(|value| value.len() <= 8)
                    };
                    assert_eq!(std::fs::read_to_string(&rollover).ok().as_deref(), expected);
                    std::fs::remove_dir_all(dir).unwrap();
                }
            }
        }

        #[test]
        fn missing_parent_returns_export_failure_without_creating_storage() {
            // Missing parent -> I/O error; diagnostics never provision another
            // directory or invent successful persistence after this failure.
            let (dir, mut exporter) = fixture();
            exporter.path = dir.join("absent/trace.jsonl").into_os_string();
            assert_eq!(
                exporter
                    .write_lines([Ok(b"0\n".to_vec())])
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::NotFound
            );
            assert!(!dir.join("absent").exists());
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn concurrent_rotation_retains_complete_lines_within_the_same_budget() {
            // Concurrent writers + repeated rollover -> the existing exporter
            // mutex serializes rename/write. The retained tail may drop older
            // records, but neither file can grow or contain a partial JSON line.
            let (dir, exporter) = fixture();
            let exporter = std::sync::Arc::new(exporter);
            let writers = (0..4)
                .map(|value| {
                    let exporter = exporter.clone();
                    std::thread::spawn(move || {
                        for _ in 0..50 {
                            exporter
                                .write_lines([Ok(format!("{value}\n").into_bytes())])
                                .unwrap();
                        }
                    })
                })
                .collect::<Vec<_>>();
            for writer in writers {
                writer.join().unwrap();
            }
            for path in [dir.join("trace.jsonl"), dir.join("trace.jsonl.1")] {
                let bytes = std::fs::read(&path).unwrap();
                assert!(bytes.len() <= 8);
                assert_eq!(bytes.last(), Some(&b'\n'));
                for line in String::from_utf8(bytes).unwrap().lines() {
                    let value: u8 = serde_json::from_str(line).unwrap();
                    assert!(value < 4);
                }
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[cfg(test)]
mod file_sink_tests {
    use super::file_exporter::JsonFileSpanExporter;
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn blocked_export_does_not_block_the_business_span_end() {
        use opentelemetry::trace::{Span as _, Tracer as _};
        use opentelemetry_sdk::trace::{SpanData, SpanExporter};
        use std::sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::Duration;

        // Causes: production SDK processor selected + first export blocked.
        // Effect: a second request's span can end before the export is released.
        // Channels establish that exact ordering; bounded receives prevent a
        // regressed synchronous exporter from hanging the suite. Release/join
        // happen before the assertion, including the negative control path.
        #[derive(Debug)]
        struct GatedExporter {
            first: AtomicBool,
            started: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl SpanExporter for GatedExporter {
            fn export(
                &self,
                _: Vec<SpanData>,
            ) -> impl std::future::Future<Output = opentelemetry_sdk::error::OTelSdkResult> + Send
            {
                if self.first.swap(false, Ordering::SeqCst) {
                    self.started.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                }
                async { Ok(()) }
            }
        }
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let provider = super::file_exporter::provider(GatedExporter {
            first: AtomicBool::new(true),
            started: started_tx,
            release: Mutex::new(release_rx),
        });
        let first = std::thread::spawn({
            let provider = provider.clone();
            move || {
                provider.tracer("file-sink-test").start("first").end();
                provider.force_flush().unwrap();
            }
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let second = std::thread::spawn({
            let tracer = provider.tracer("file-sink-test");
            move || {
                tracer.start("second").end();
                finished_tx.send(()).unwrap();
            }
        });
        let finished_before_release = finished_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        release_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        provider.shutdown().unwrap();
        assert!(
            finished_before_release,
            "a diagnostic file stall cannot delay a business span end"
        );
    }

    /// The configured trace-file sink is the collector-free contract the e2e trace
    /// validator reads back: each finished span appended as ONE JSON line with a
    /// fixed shape. This drives a real span through the exporter (exactly as
    /// `build_layer` wires it, minus the env/global install) and asserts the emitted
    /// line's fields round-trip through the production batch processor and explicit flush.
    #[test]
    fn exported_span_round_trips_as_one_json_line() {
        // Unique temp path (no `tempfile` dev-dep); cleaned up at the end.
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "awaken-obs-trace-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let provider = super::file_exporter::provider(JsonFileSpanExporter::new(
            path.clone().into_os_string(),
        ));
        let tracer = provider.tracer("awaken-observability-file-sink-test");
        let subscriber =
            Registry::default().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("file.sink.span", sink.attr = "banana");
            span.in_scope(|| {});
        });
        provider
            .force_flush()
            .expect("flush the span to the file sink");

        let contents = std::fs::read_to_string(&path).expect("the sink file was written");
        let _ = std::fs::remove_file(&path);

        // Exactly one non-empty JSON line for the one finished span.
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "one span → one line: {contents:?}");

        let value: serde_json::Value =
            serde_json::from_str(lines[0]).expect("the line is valid JSON");
        let obj = value.as_object().expect("the line is a JSON object");

        // Field shape the validator keys on.
        assert_eq!(
            obj.get("name").and_then(|v| v.as_str()),
            Some("file.sink.span"),
            "name round-trips: {value}"
        );
        let trace_id = obj
            .get("trace_id")
            .and_then(|v| v.as_str())
            .expect("trace_id");
        assert_eq!(trace_id.len(), 32, "trace_id is 32 hex: {value}");
        assert!(
            trace_id.chars().all(|c| c.is_ascii_hexdigit()) && trace_id != "0".repeat(32),
            "trace_id is a valid non-zero hex id: {value}"
        );
        let span_id = obj
            .get("span_id")
            .and_then(|v| v.as_str())
            .expect("span_id");
        assert_eq!(span_id.len(), 16, "span_id is 16 hex: {value}");
        assert!(
            span_id.chars().all(|c| c.is_ascii_hexdigit()) && span_id != "0".repeat(16),
            "span_id is a valid non-zero hex id: {value}"
        );
        // A root span has an explicit null parent (not a missing key).
        assert!(
            obj.contains_key("parent_span_id") && obj["parent_span_id"].is_null(),
            "a root span carries an explicit null parent_span_id: {value}"
        );
        // Attributes round-trip as a string map.
        let attrs = obj
            .get("attributes")
            .and_then(|v| v.as_object())
            .expect("attributes is a JSON object");
        assert_eq!(
            attrs.get("sink.attr").and_then(|v| v.as_str()),
            Some("banana"),
            "the span attribute round-trips through the file line: {value}"
        );
    }
}
