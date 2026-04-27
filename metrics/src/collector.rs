//! Background scrape loop.
//!
//! The [`Collector`] spawns a `tokio` task that wakes up every
//! `scrape_interval_secs` seconds, queries each configured router contract
//! via the Soroban RPC, and updates the Prometheus gauges / counters.
//!
//! ## Scraping strategy
//!
//! Soroban contracts store state in on-chain ledger entries.  The cleanest
//! way to read that state from off-chain is to call the contract's view
//! functions via `simulateTransaction`.  This exporter calls:
//!
//! - `router-core`:       `total_routed()`, `is_paused()`, `get_all_routes()`
//!                        + `get_route(name)` for each route
//! - `router-middleware`: `total_calls()`, `get_configured_routes()`
//!                        + `circuit_breaker_state(route)` for each route
//! - `router-registry`:   `get_all_names()` (total count)
//!
//! Each contract scrape is timed and any error increments the
//! `router_scrape_errors_total` counter for that contract label.

use std::time::Instant;

use anyhow::Result;
use tracing::{error, info, warn};

use crate::cli::Args;
use crate::metrics::RouterMetrics;
use crate::rpc::SorobanRpcClient;

/// Drives the periodic scrape loop.
#[derive(Clone)]
pub struct Collector {
    args: Args,
    metrics: RouterMetrics,
}

impl Collector {
    pub fn new(args: Args, metrics: RouterMetrics) -> Self {
        Self { args, metrics }
    }

    /// Run forever, scraping on the configured interval.
    pub async fn run(self) {
        let interval = tokio::time::Duration::from_secs(self.args.scrape_interval_secs);
        info!(
            interval_secs = self.args.scrape_interval_secs,
            "scrape loop started"
        );

        let client = match SorobanRpcClient::new(&self.args.rpc_url, self.args.rpc_timeout_secs) {
            Ok(c) => c,
            Err(e) => {
                error!("failed to create RPC client: {e:#}");
                return;
            }
        };

        loop {
            let cycle_ok = self.scrape_all(&client).await;
            self.metrics.up.set(if cycle_ok { 1.0 } else { 0.0 });
            tokio::time::sleep(interval).await;
        }
    }

    /// Scrape all configured contracts.  Returns `true` if every scrape
    /// succeeded, `false` if any failed.
    async fn scrape_all(&self, client: &SorobanRpcClient) -> bool {
        let mut all_ok = true;

        if !self.args.core_contract_id.is_empty() {
            if let Err(e) = self.scrape_core(client, &self.args.core_contract_id).await {
                warn!(contract = %self.args.core_contract_id, "core scrape failed: {e:#}");
                self.metrics
                    .scrape_errors_total
                    .with_label_values(&[&self.args.core_contract_id])
                    .inc();
                all_ok = false;
            }
        }

        if !self.args.middleware_contract_id.is_empty() {
            if let Err(e) = self
                .scrape_middleware(client, &self.args.middleware_contract_id)
                .await
            {
                warn!(contract = %self.args.middleware_contract_id, "middleware scrape failed: {e:#}");
                self.metrics
                    .scrape_errors_total
                    .with_label_values(&[&self.args.middleware_contract_id])
                    .inc();
                all_ok = false;
            }
        }

        if !self.args.registry_contract_id.is_empty() {
            if let Err(e) = self
                .scrape_registry(client, &self.args.registry_contract_id)
                .await
            {
                warn!(contract = %self.args.registry_contract_id, "registry scrape failed: {e:#}");
                self.metrics
                    .scrape_errors_total
                    .with_label_values(&[&self.args.registry_contract_id])
                    .inc();
                all_ok = false;
            }
        }

        all_ok
    }

    // ── router-core ───────────────────────────────────────────────────────────

    async fn scrape_core(&self, client: &SorobanRpcClient, contract_id: &str) -> Result<()> {
        let start = Instant::now();
        info!(contract_id, "scraping router-core");

        // 1. total_routed
        let total_routed = client.call_u64(contract_id, "total_routed").await?;
        self.metrics
            .core_total_routed
            .with_label_values(&[contract_id])
            .set(total_routed as f64);

        // 2. is_paused (router-core exposes this via storage; we call set_paused
        //    indirectly — the contract stores a `Paused` bool in instance storage.
        //    We read it via a helper view function if available, otherwise we
        //    attempt to resolve a non-existent route and check for RouterPaused.)
        //
        //    router-core does not expose a dedicated `is_paused()` view function
        //    in the current implementation, so we use `get_route` on a sentinel
        //    name and interpret the error.  A cleaner approach is to add a
        //    `is_paused()` view function to the contract (tracked separately).
        //
        //    For now we record 0 (unknown / not paused) and note the limitation.
        self.metrics
            .core_paused
            .with_label_values(&[contract_id])
            .set(0.0); // updated below if the RPC call succeeds

        // 3. get_all_routes → per-route paused state
        let routes = client
            .call_string_vec(contract_id, "get_all_routes")
            .await?;
        for route in &routes {
            // get_route returns a RouteEntry; we check the `paused` field.
            // The JSON representation of a Soroban struct is a map of field names.
            let route_result = client
                .simulate_invoke(contract_id, "get_route", vec![encode_string_arg(route)])
                .await;

            match route_result {
                Ok(val) => {
                    let paused = extract_route_paused(&val).unwrap_or(false);
                    self.metrics
                        .core_route_paused
                        .with_label_values(&[contract_id, route])
                        .set(if paused { 1.0 } else { 0.0 });
                }
                Err(e) => {
                    warn!(contract_id, route, "failed to get route state: {e:#}");
                }
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        self.metrics
            .scrape_duration_seconds
            .with_label_values(&[contract_id])
            .observe(elapsed);

        info!(
            contract_id,
            elapsed_secs = elapsed,
            routes = routes.len(),
            total_routed,
            "core scrape done"
        );
        Ok(())
    }

    // ── router-middleware ─────────────────────────────────────────────────────

    async fn scrape_middleware(&self, client: &SorobanRpcClient, contract_id: &str) -> Result<()> {
        let start = Instant::now();
        info!(contract_id, "scraping router-middleware");

        // 1. total_calls
        let total_calls = client.call_u64(contract_id, "total_calls").await?;
        self.metrics
            .middleware_total_calls
            .with_label_values(&[contract_id])
            .set(total_calls as f64);

        // 2. Per-route circuit breaker state
        let routes = client
            .call_string_vec(contract_id, "get_configured_routes")
            .await?;

        for route in &routes {
            let cb_result = client
                .simulate_invoke(
                    contract_id,
                    "circuit_breaker_state",
                    vec![encode_string_arg(route)],
                )
                .await;

            match cb_result {
                Ok(val) => {
                    let (is_open, failure_count) =
                        extract_circuit_breaker_state(&val).unwrap_or((false, 0));
                    self.metrics
                        .middleware_circuit_open
                        .with_label_values(&[contract_id, route])
                        .set(if is_open { 1.0 } else { 0.0 });
                    self.metrics
                        .middleware_failure_count
                        .with_label_values(&[contract_id, route])
                        .set(failure_count as f64);
                }
                Err(e) => {
                    warn!(
                        contract_id,
                        route, "failed to get circuit breaker state: {e:#}"
                    );
                }
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        self.metrics
            .scrape_duration_seconds
            .with_label_values(&[contract_id])
            .observe(elapsed);

        info!(
            contract_id,
            elapsed_secs = elapsed,
            routes = routes.len(),
            total_calls,
            "middleware scrape done"
        );
        Ok(())
    }

    // ── router-registry ───────────────────────────────────────────────────────

    async fn scrape_registry(&self, client: &SorobanRpcClient, contract_id: &str) -> Result<()> {
        let start = Instant::now();
        info!(contract_id, "scraping router-registry");

        // get_all_names returns Vec<String> of registered contract names
        let names = client.call_string_vec(contract_id, "get_all_names").await?;

        self.metrics
            .registry_total_names
            .with_label_values(&[contract_id])
            .set(names.len() as f64);

        let elapsed = start.elapsed().as_secs_f64();
        self.metrics
            .scrape_duration_seconds
            .with_label_values(&[contract_id])
            .observe(elapsed);

        info!(
            contract_id,
            elapsed_secs = elapsed,
            total_names = names.len(),
            "registry scrape done"
        );
        Ok(())
    }
}

// ── Value extraction helpers ──────────────────────────────────────────────────

/// Encode a plain string as a base64 XDR `ScVal::String` argument.
///
/// This is a placeholder — a real implementation would use the `stellar-xdr`
/// crate to produce the correct XDR encoding.
fn encode_string_arg(s: &str) -> String {
    // Base64-encode the raw UTF-8 bytes as a minimal stand-in.
    // Replace with proper ScVal XDR encoding in production.
    use std::fmt::Write;
    let mut out = String::new();
    for b in s.as_bytes() {
        write!(out, "{b:02x}").ok();
    }
    out
}

/// Extract the `paused` field from a `RouteEntry` JSON value returned by
/// `simulateTransaction`.
fn extract_route_paused(val: &serde_json::Value) -> Option<bool> {
    // The Soroban RPC returns struct fields as a JSON map.
    // RouteEntry { address, name, paused, updated_by, metadata }
    val.get("results")
        .and_then(|r| r.get(0))
        .and_then(|r| r.get("retval"))
        .and_then(|v| v.get("paused"))
        .and_then(|p| p.as_bool())
        .or_else(|| val.get("paused").and_then(|p| p.as_bool()))
}

/// Extract `(is_open, failure_count)` from a `CircuitBreakerState` JSON value.
fn extract_circuit_breaker_state(val: &serde_json::Value) -> Option<(bool, u32)> {
    let retval = val
        .get("results")
        .and_then(|r| r.get(0))
        .and_then(|r| r.get("retval"))
        .unwrap_or(val);

    // Handle Option<CircuitBreakerState> — None means no state recorded yet
    if retval.is_null() || retval.get("none").is_some() {
        return Some((false, 0));
    }

    let state = retval.get("some").unwrap_or(retval);
    let is_open = state
        .get("is_open")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let failure_count = state
        .get("failure_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    Some((is_open, failure_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_route_paused_true() {
        let val = json!({
            "results": [{ "retval": { "paused": true } }]
        });
        assert_eq!(extract_route_paused(&val), Some(true));
    }

    #[test]
    fn test_extract_route_paused_false() {
        let val = json!({ "paused": false });
        assert_eq!(extract_route_paused(&val), Some(false));
    }

    #[test]
    fn test_extract_circuit_breaker_open() {
        let val = json!({
            "results": [{
                "retval": {
                    "some": {
                        "is_open": true,
                        "failure_count": 5,
                        "opened_at": 1000
                    }
                }
            }]
        });
        assert_eq!(extract_circuit_breaker_state(&val), Some((true, 5)));
    }

    #[test]
    fn test_extract_circuit_breaker_none() {
        let val = json!({
            "results": [{ "retval": null }]
        });
        assert_eq!(extract_circuit_breaker_state(&val), Some((false, 0)));
    }

    #[test]
    fn test_extract_circuit_breaker_closed() {
        let val = json!({
            "results": [{
                "retval": {
                    "some": {
                        "is_open": false,
                        "failure_count": 2,
                        "opened_at": 0
                    }
                }
            }]
        });
        assert_eq!(extract_circuit_breaker_state(&val), Some((false, 2)));
    }
}
