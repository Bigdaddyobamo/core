//! Real-time Soroban contract event streaming via JSON-RPC `getEvents`.
//! Replaces legacy REST polling with direct contract topic and event parsing.

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// JSON-RPC 2.0 Request envelope.
#[derive(Debug, Serialize)]
struct JsonRpcRequest<T> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: T,
}

/// Parameters for Soroban RPC `getEvents`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsParams {
    pub start_ledger: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<EventFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<PaginationParams>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EventFilter {
    #[serde(rename = "type")]
    pub event_type: String,
    pub contract_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<Vec<String>>,
}

#[derive(Debug, Serialize, Clone)]
pub struct PaginationParams {
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// JSON-RPC 2.0 Response envelope.
#[derive(Debug, Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEventsResult {
    pub events: Vec<SorobanEvent>,
    pub latest_ledger: u64,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// A parsed Soroban contract event emitted on-chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SorobanEvent {
    pub id: String,
    pub contract_id: String,
    pub ledger: u64,
    pub ledger_closed_at: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub topic: Vec<String>,
    pub value: serde_json::Value,
    pub in_successful_contract_call: bool,
}

/// Real-time Soroban contract event streaming engine.
pub struct SorobanEventStreamer {
    rpc_url: String,
    contract_ids: Vec<String>,
    last_cursor: Option<String>,
    current_ledger: u64,
    client: Client,
}

impl SorobanEventStreamer {
    pub fn new(rpc_url: impl Into<String>, contract_ids: Vec<String>, start_ledger: u64) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            contract_ids,
            last_cursor: None,
            current_ledger: start_ledger,
            client: Client::builder().build().unwrap_or_default(),
        }
    }

    /// Fetches the next batch of real-time Soroban events via RPC `getEvents`.
    pub async fn fetch_events(&mut self) -> Result<Vec<SorobanEvent>> {
        let filter = EventFilter {
            event_type: "contract".into(),
            contract_ids: self.contract_ids.clone(),
            topics: vec![],
        };

        let params = GetEventsParams {
            start_ledger: self.current_ledger,
            filters: vec![filter],
            pagination: Some(PaginationParams {
                limit: 100,
                cursor: self.last_cursor.clone(),
            }),
        };

        let request_body = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "getEvents",
            params,
        };

        let response = self
            .client
            .post(&self.rpc_url)
            .json(&request_body)
            .send()
            .await
            .context("failed to send getEvents RPC request")?;

        let rpc_res: JsonRpcResponse<GetEventsResult> = response
            .json()
            .await
            .context("failed to parse getEvents RPC response")?;

        if let Some(err) = rpc_res.error {
            return Err(anyhow!("RPC getEvents error {}: {}", err.code, err.message));
        }

        let result = rpc_res
            .result
            .ok_or_else(|| anyhow!("missing result in getEvents RPC response"))?;

        if let Some(c) = result.cursor {
            self.last_cursor = Some(c);
        }
        self.current_ledger = result.latest_ledger;

        debug!(
            events_count = result.events.len(),
            latest_ledger = result.latest_ledger,
            "streamed real-time soroban events"
        );

        Ok(result.events)
    }

    pub fn current_ledger(&self) -> u64 {
        self.current_ledger
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_filter_serialization() {
        let filter = EventFilter {
            event_type: "contract".into(),
            contract_ids: vec!["CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into()],
            topics: vec![vec!["transfer".into()]],
        };
        let json = serde_json::to_string(&filter).unwrap();
        assert!(json.contains("contractIds"));
        assert!(json.contains("type"));
        assert!(json.contains("topics"));
    }
}
