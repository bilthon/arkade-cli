//! Thin read-only wrapper over the Arkade operator indexer, via `ark-rest`.

use ark_rest::apis::configuration::Configuration;
use ark_rest::apis::indexer_service_api::{self, IndexerServiceGetVtxosError};
use ark_rest::apis::Error as ArkRestError;
use ark_rest::models::IndexerVtxo;

use std::time::Duration;

use crate::outpoint::Outpoint;

/// User-Agent sent to the operator, e.g. `akcli/0.1.0`.
const USER_AGENT: &str = concat!("akcli/", env!("CARGO_PKG_VERSION"));

/// A read-only client for one operator's indexer.
pub struct IndexerClient {
    config: Configuration,
}

/// Errors returned while querying the indexer.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error("request to operator failed: {0}")]
    Transport(String),
    #[error("operator returned HTTP {status}: {body}")]
    Response {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("could not decode operator response: {0}")]
    Malformed(String),
}

impl IndexerClient {
    /// Build a client pointed at `operator_base_url` (e.g. `http://localhost:7071`),
    /// applying a request `timeout` and the akcli User-Agent.
    pub fn new(operator_base_url: &str, timeout: Duration) -> Result<Self, IndexerError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| IndexerError::Transport(e.to_string()))?;
        let config = Configuration {
            base_path: operator_base_url.to_string(),
            user_agent: Some(USER_AGENT.to_string()),
            client,
            ..Configuration::new()
        };
        Ok(Self { config })
    }

    /// Fetch the VTXO at `outpoint`, or `Ok(None)` if the indexer has no record.
    pub async fn get_vtxo(&self, outpoint: &Outpoint) -> Result<Option<IndexerVtxo>, IndexerError> {
        let response = indexer_service_api::indexer_service_get_vtxos(
            &self.config,
            None,                            // scripts
            Some(vec![outpoint.to_string()]), // outpoints
            None, None, None, None, None, None, None, None, None,
        )
        .await
        .map_err(map_err)?;

        Ok(response.vtxos.and_then(|mut v| if v.is_empty() { None } else { Some(v.remove(0)) }))
    }
}

fn map_err(err: ArkRestError<IndexerServiceGetVtxosError>) -> IndexerError {
    match err {
        ArkRestError::Reqwest(e) => IndexerError::Transport(e.to_string()),
        ArkRestError::Io(e) => IndexerError::Transport(e.to_string()),
        ArkRestError::Serde(e) => IndexerError::Malformed(e.to_string()),
        ArkRestError::ResponseError(rc) => IndexerError::Response {
            status: rc.status,
            body: rc.content,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(uri: &str) -> IndexerClient {
        IndexerClient::new(uri, Duration::from_secs(5)).unwrap()
    }

    const TXID: &str = "aa11bb22cc33dd44ee55ff66778899001122334455667788990011223344abcd";

    fn outpoint() -> Outpoint {
        Outpoint::parse(&format!("{TXID}:0")).unwrap()
    }

    #[tokio::test]
    async fn returns_vtxo_when_indexer_has_one() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "vtxos": [{
                "amount": "1000",
                "script": "0014abcdef",
                "isSpent": false,
                "outpoint": { "txid": TXID, "vout": 0 }
            }]
        });
        Mock::given(method("GET"))
            .and(path("/v1/indexer/vtxos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let vtxo = client(&server.uri()).get_vtxo(&outpoint()).await.unwrap().unwrap();
        assert_eq!(vtxo.amount.as_deref(), Some("1000"));
        assert_eq!(vtxo.is_spent, Some(false));
    }

    #[tokio::test]
    async fn returns_none_when_indexer_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/indexer/vtxos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "vtxos": [] })))
            .mount(&server)
            .await;

        assert!(client(&server.uri()).get_vtxo(&outpoint()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn maps_server_error_to_response_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/indexer/vtxos"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = client(&server.uri()).get_vtxo(&outpoint()).await.unwrap_err();
        match err {
            IndexerError::Response { status, .. } => assert_eq!(status.as_u16(), 500),
            other => panic!("expected Response error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maps_transport_failure() {
        // Nothing listening on this port -> connection refused.
        let err = client("http://127.0.0.1:1").get_vtxo(&outpoint()).await.unwrap_err();
        assert!(matches!(err, IndexerError::Transport(_)));
    }

    #[tokio::test]
    async fn sends_akcli_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/indexer/vtxos"))
            .and(header_regex("user-agent", "^akcli/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "vtxos": [] })))
            .mount(&server)
            .await;
        // Only matches (and returns 200) if the User-Agent header is present.
        assert!(client(&server.uri()).get_vtxo(&outpoint()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn times_out_slow_operator() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/indexer/vtxos"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "vtxos": [] }))
                    .set_delay(Duration::from_millis(400)),
            )
            .mount(&server)
            .await;
        let client = IndexerClient::new(&server.uri(), Duration::from_millis(50)).unwrap();
        let err = client.get_vtxo(&outpoint()).await.unwrap_err();
        assert!(matches!(err, IndexerError::Transport(_)));
    }
}
