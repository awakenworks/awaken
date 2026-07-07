# Trace collection (Phoenix + Jaeger + OTel Collector)

Two ways to inspect Awaken's OpenTelemetry traces:

## 1. Collector-free (automated, no Docker) — what CI asserts

`awaken-observability` writes every finished span to a file when `AWAKEN_TRACE_FILE`
is set. `trace_capture_e2e.mjs` drives real scenarios through the instrumented
server, reads that file back, and asserts the span trees against the OTel GenAI
conventions and W3C `traceparent` propagation:

```bash
cd e2e && node trace_capture_e2e.mjs
```

No collector, no API key: the `echo` model drives a full turn, so the whole
`ingress → sessions.events.send → invoke_agent → chat` chain is exercised.

## 2. Live OTLP (interactive) — view the same traces in a UI

Bring up the stack (one OTLP stream fans out to both backends):

```bash
docker compose -f e2e/phoenix/docker-compose.yml up -d --wait
```

Run the server pointed at the collector, then drive any traffic:

```bash
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces \
OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf \
OTEL_SERVICE_NAME=awaken-server-local \
AWAKEN_MODEL_MODE=echo AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
  cargo run -p awaken-server-local

# in another shell, drive a turn (or run an e2e against port 38080), then open:
#   Phoenix  http://localhost:6006     (GenAI-native: invoke_agent / chat / execute_tool)
#   Jaeger   http://localhost:16686
```

Tear down:

```bash
docker compose -f e2e/phoenix/docker-compose.yml down
```

### Recognized environment

| Variable | Effect |
| --- | --- |
| `AWAKEN_TRACE_FILE=<path>` | Collector-free JSON-lines span sink (takes priority over OTLP). |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP/HTTP traces endpoint; enables live export. |
| `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | `http/protobuf` (the compiled exporter). |
| `OTEL_SERVICE_NAME` / `OTEL_SERVICE_VERSION` | Resource attributes on exported spans. |
| `AWAKEN_LOG_FORMAT=json` | Structured JSON log lines instead of text. |
| `RUST_LOG` | Log/span filter (default `info`). |
