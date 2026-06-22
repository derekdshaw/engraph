# OpenTelemetry metrics export

engraph can export its usage and token-savings telemetry to an OpenTelemetry
collector over OTLP/gRPC, in addition to the local SQLite `events` table that
`engraph gain` reads. This lets you build dashboards for "tokens trimmed by
compression / avoided by subgraph over time" in Grafana, Prometheus, etc.

It is **off by default** and gated two ways:

- **Compile time** — the heavy OpenTelemetry dependencies are behind the `otel`
  cargo feature, so the default build stays lean.
- **Run time** — even in an `otel` build, nothing is exported unless `ENGRAPH_OTEL`
  is set.

## 1. Build with the feature

```
cargo build --release -p engraph-cli --features otel \
  && install -m 755 target/release/engraph ~/.local/bin/engraph
```

> The Claude Code hooks invoke the **installed** binary. If that binary was not
> built with `--features otel`, `ENGRAPH_OTEL=1` is a silent no-op. Reinstall with
> the feature to get metrics from the live session.

## 2. Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `ENGRAPH_OTEL` | Enable export. Truthy: `1`, `true`, `yes` (case-insensitive). | unset → disabled |
| `ENGRAPH_OTEL_ENDPOINT` | OTLP/gRPC collector URL. | falls back to `OTEL_EXPORTER_OTLP_ENDPOINT`, then `http://localhost:4317` |
| `ENGRAPH_OTEL_SESSION` | Tag every metric with a `session.id` **resource** attribute (from `CLAUDE_SESSION_ID`) for per-session correlation. Opt-in because session ids are high-cardinality. | unset → no session tag |

Example (one-off):

```
ENGRAPH_OTEL=1 ENGRAPH_OTEL_ENDPOINT=http://localhost:4317 engraph run -- git status
```

To enable it for the live Claude Code session, set the vars in the hook
environment (e.g. your shell profile or the hook command env).

## 3. What gets exported

Transport: **OTLP/gRPC** (default collector port `4317`).

Metrics — emitted from the single `record_event` chokepoint, so every
compression / retrieval / wrapped-command / index event is covered:

| Instrument | Type | Meaning |
|---|---|---|
| `engraph.events` | counter | one per recorded event |
| `engraph.tokens.input` | counter | pre-processing / baseline tokens |
| `engraph.tokens.output` | counter | produced tokens |
| `engraph.tokens.saved` | counter | tokens trimmed (compression / wrapped-cmd / subgraph); only emitted when > 0 |
| `engraph.latency.ms` | histogram | per-event latency |

Attributes (low-cardinality, kept on the metrics): `kind`
(`compress`/`retrieve`/`hook`/`wrapped_cmd`/`index`), `feature`
(`output_filter`, `compress`, `recall`, `subgraph`, …).

Resource attributes: `service.name=engraph`, `service.version`, and — when
`ENGRAPH_OTEL_SESSION=1` — `session.id`. `session_id` is **not** a metric
attribute (it would explode series cardinality); it lives on the resource.

### Prometheus metric names

OTLP names are normalized by Prometheus (dots → underscores, counters get
`_total`, histograms get `_bucket`/`_count`/`_sum`):

```
engraph_events_total
engraph_tokens_input_total
engraph_tokens_output_total
engraph_tokens_saved_total
engraph_latency_ms_bucket | engraph_latency_ms_count | engraph_latency_ms_sum
```

## 4. Required collector change: delta → cumulative

**This is the step that makes totals work.** engraph emits **delta** temporality
because each invocation is a short-lived process: a cumulative counter would
reset to a tiny value on every run, and a plain Prometheus OTLP ingest cannot
reconstruct those per-process resets — `increase()`/totals come out ~0.

But Prometheus stores **cumulative** series and **silently drops delta**. So a
collector must convert delta → cumulative before Prometheus. Without this:

- delta straight to Prometheus → metrics **never appear**.
- (and forcing cumulative in engraph → metrics appear but `increase()` is always ~0)

Add the `deltatocumulative` processor to the collector's **metrics** pipeline.
In an OpenTelemetry Collector config (`otelcol-config.yaml`):

```yaml
processors:
  batch:
  deltatocumulative:        # <-- add

service:
  pipelines:
    metrics:
      receivers: [otlp]
      processors: [deltatocumulative, batch]   # <-- add deltatocumulative
      exporters: [otlp_http/metrics]           # (your Prometheus OTLP exporter)
```

Then restart the collector. `deltatocumulative` is an `otelcol-contrib`
processor; the `grafana/otel-lgtm` image ships contrib, so it is available there.

> **Alternative (no collector processor):** enable Prometheus's native OTLP delta
> handling with `--enable-feature=otlp-deltatocumulative` on the Prometheus
> process. Version-dependent; the collector processor is the more portable fix.

> **Persistence:** editing a config file inside a running container survives
> `docker restart` but **not** `docker rm`/recreate. Bake the change into your
> image or mount a custom `otelcol-config.yaml` to keep it.

## 5. Verifying it works (PromQL)

```promql
# Event counts per feature, accumulated across invocations
increase(engraph_events_total[1h])

# Total tokens trimmed in the last hour
sum(increase(engraph_tokens_saved_total[1h]))

# Savings broken down by what produced them
sum by (kind, feature) (increase(engraph_tokens_saved_total[24h]))

# p95 per-event latency
histogram_quantile(0.95, sum by (le) (rate(engraph_latency_ms_bucket[5m])))
```

### Correlating to a Claude session

With `ENGRAPH_OTEL_SESSION=1`, `session.id` lands on the `target_info` metric
(standard Prometheus handling of OTLP resource attributes). Join through it:

```promql
engraph_tokens_saved_total
  * on(job, instance) group_left(session_id) target_info{session_id="<id>"}
```

## 6. Notes

- Metrics export is best-effort: any exporter/connection error is logged
  (`ENGRAPH_LOG=…`) and never fails the engraph command.
- Default (non-`otel`) builds, and `otel` builds with `ENGRAPH_OTEL` unset, still
  write the local `events` table — `engraph gain` is unaffected either way.
- To debug what reaches the collector, add the contrib `debug` exporter
  (`verbosity: detailed`) to the metrics pipeline and watch the collector log.
