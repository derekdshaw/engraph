//! Optional OpenTelemetry metrics export, gated by the `otel` cargo feature and
//! the `ENGRAPH_OTEL` env var.
//!
//! Env vars:
//! - `ENGRAPH_OTEL` (`1|true|yes`) — enable export.
//! - `ENGRAPH_OTEL_ENDPOINT` — OTLP/gRPC collector URL (falls back to
//!   `OTEL_EXPORTER_OTLP_ENDPOINT`, then `http://localhost:4317`).
//! - `ENGRAPH_OTEL_SESSION` (`1|true|yes`) — attach `CLAUDE_SESSION_ID` as the
//!   `session.id` resource attribute for per-session correlation. Off by default
//!   because session ids are high-cardinality.
//!
//! engraph is a short-lived CLI: each invocation runs for milliseconds and exits,
//! so the SDK's periodic export interval never fires. We therefore force-flush and
//! shut down the meter provider explicitly before the process ends — via the
//! [`OtelGuard`]'s `Drop` on normal return, and via an explicit `shutdown()` call
//! before the lone `std::process::exit` in the CLI (which skips destructors).
//!
//! Transport is OTLP/gRPC (tonic), which requires the meter provider to live
//! inside a tokio runtime. The CLI runs `main` on a `multi_thread` runtime with a
//! single worker **when the `otel` feature is on** so the worker keeps driving the
//! gRPC export while the main thread blocks on `shutdown()` — a `current_thread`
//! runtime would deadlock there (see opentelemetry_sdk PeriodicReader docs).
//!
//! NOTE: because this is feature-gated and the Claude Code hooks invoke the
//! installed *release* binary, `ENGRAPH_OTEL=1` is a silent no-op unless the
//! release was built with `--features otel`.

#[cfg(feature = "otel")]
mod imp {
    use opentelemetry::{KeyValue, global};
    use opentelemetry_otlp::{MetricExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};

    /// Holds the meter provider for the process lifetime. Flushes + shuts down on
    /// drop; `shutdown()` does the same explicitly for paths that bypass `Drop`.
    pub struct OtelGuard {
        provider: SdkMeterProvider,
    }

    impl OtelGuard {
        /// Force-flush any buffered metrics and shut the provider down. Safe to
        /// call from the async `main` task because the CLI uses a multi-threaded
        /// runtime under the `otel` feature.
        pub fn shutdown(self) {
            // `Drop` will run flush + shutdown; calling it explicitly here just
            // means we take `self` by value so the move-before-exit is clean.
            drop(self);
        }
    }

    impl Drop for OtelGuard {
        fn drop(&mut self) {
            // shutdown() performs a final collect + export before tearing the
            // provider down, so an explicit force_flush() first would just be a
            // second, redundant OTLP round-trip per invocation.
            if let Err(e) = self.provider.shutdown() {
                tracing::warn!(?e, "otel: shutdown failed");
            }
        }
    }

    /// Build and install the global meter provider when `ENGRAPH_OTEL` is truthy.
    /// Returns `None` (and metrics become a no-op against the default global
    /// provider) when disabled or on any setup error — telemetry never fails the CLI.
    pub fn init_from_env() -> Option<OtelGuard> {
        if !enabled() {
            return None;
        }
        let endpoint = endpoint();
        match build(&endpoint) {
            Ok(provider) => {
                global::set_meter_provider(provider.clone());
                tracing::debug!(%endpoint, "otel: metrics export enabled");
                Some(OtelGuard { provider })
            }
            Err(e) => {
                tracing::warn!(?e, %endpoint, "otel: init failed; metrics disabled");
                None
            }
        }
    }

    fn enabled() -> bool {
        matches!(
            std::env::var("ENGRAPH_OTEL").ok().as_deref().map(str::trim),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    }

    /// `ENGRAPH_OTEL_ENDPOINT` wins, then the standard `OTEL_EXPORTER_OTLP_ENDPOINT`,
    /// then the gRPC default.
    fn endpoint() -> String {
        std::env::var("ENGRAPH_OTEL_ENDPOINT")
            .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT"))
            .unwrap_or_else(|_| "http://localhost:4317".to_string())
    }

    fn build(endpoint: &str) -> Result<SdkMeterProvider, opentelemetry_otlp::ExporterBuildError> {
        // Delta temporality. Each engraph run is a short-lived process: a
        // cumulative counter would reset to a tiny value every invocation, and a
        // plain Prometheus OTLP ingest can't reconstruct those per-process resets
        // (instantaneous reads show the last run's count; increase()/totals come
        // out ~0). Delta reports just this run's increment, which a collector
        // `deltatocumulative` processor (or a delta-native backend) accumulates
        // into one monotonic series. NOTE: a backend/collector that does NOT
        // deltatocumulative will silently DROP delta metrics — pair this with the
        // processor (see the LGTM collector's metrics pipeline).
        let exporter = MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_temporality(Temporality::Delta)
            .build()?;
        let reader = PeriodicReader::builder(exporter).build();
        let mut resource = Resource::builder()
            .with_service_name("engraph")
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));
        // Tag every metric from this process with the Claude session id so it can
        // be correlated back to a session. Opt-in (ENGRAPH_OTEL_SESSION): session
        // ids are high-cardinality, so this is left off by default to keep the
        // metric series count low. As a resource attribute it identifies the whole
        // process (one invocation == one session) and is directly queryable on an
        // OTLP-native backend.
        if let Some(sid) = session_id() {
            resource = resource.with_attribute(KeyValue::new("session.id", sid));
        }
        Ok(SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource.build())
            .build())
    }

    /// `CLAUDE_SESSION_ID`, but only when `ENGRAPH_OTEL_SESSION` opts in. `None`
    /// when the flag is off or the CLI runs outside a Claude session.
    fn session_id() -> Option<String> {
        let opted_in = matches!(
            std::env::var("ENGRAPH_OTEL_SESSION")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        );
        if !opted_in {
            return None;
        }
        std::env::var("CLAUDE_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty())
    }
}

#[cfg(not(feature = "otel"))]
mod imp {
    /// No-op guard for builds without the `otel` feature.
    pub struct OtelGuard;

    impl OtelGuard {
        pub fn shutdown(self) {}
    }

    /// Always `None` without the `otel` feature.
    pub fn init_from_env() -> Option<OtelGuard> {
        None
    }
}

pub use imp::{OtelGuard, init_from_env};
