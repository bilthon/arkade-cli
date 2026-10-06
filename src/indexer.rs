//! Thin read-only wrapper over the Arkade operator indexer, via `ark-rest`.

use ark_rest::apis::configuration::Configuration;
use ark_rest::apis::{ark_service_api, indexer_service_api};
use ark_rest::apis::Error as ArkRestError;
use ark_rest::models::{GetInfoResponse, IndexerVtxo};

use std::time::Duration;

use crate::commitment::CommitmentTx;
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

    /// Fetch one virtual transaction PSBT; only an explicit empty list means not found.
    pub async fn get_virtual_tx(&self, txid: &str) -> anyhow::Result<Option<crate::virtual_tx::VirtualTx>> {
        use anyhow::{ensure, Context};
        crate::outpoint::validate_txid(txid)?;
        let response = indexer_service_api::indexer_service_get_virtual_txs(
            &self.config, vec![txid.to_owned()], None, None, None, None, None,
        ).await.map_err(map_err)?;
        let txs = response.txs.context("virtual transaction response missing txs")?;
        ensure!(txs.len() <= 1, "multiple virtual transaction records returned");
        txs.first().map(|psbt| crate::virtual_tx::VirtualTx::decode(txid, psbt)).transpose()
    }

    /// Fetch operator information. All HTTP failures, including 404, are errors.
    pub async fn get_info(&self) -> Result<GetInfoResponse, IndexerError> {
        ark_service_api::ark_service_get_info(&self.config)
            .await
            .map_err(map_err)
    }

    /// Validate locally, verify the operator identity, then buffer every page.
    pub async fn get_address(&self, raw: &str) -> anyhow::Result<crate::address::AddressResult> {
        use crate::address::{Address, AddressResult};

        let address = Address::decode(raw)?;
        address.check_operator(&self.get_info().await?)?;
        let scripts = address.scripts();
        let vtxos = self.get_script_vtxos(scripts.clone()).await?;
        Ok(AddressResult { address: raw.to_string(), scripts, vtxos })
    }

    /// Decode locally and query only structurally P2TR scripts, without operator identity checks.
    pub async fn get_script(&self, raw: &str) -> anyhow::Result<crate::script::ScriptResult> {
        use crate::script::{LookupStatus, ScriptResult, ScriptSummary};
        let script = ScriptSummary::decode(raw)?;
        if script.p2tr_output_key.is_none() {
            return Ok(ScriptResult::unsupported(script));
        }
        let vtxos = self.get_script_vtxos(vec![script.hex.clone()]).await?;
        Ok(ScriptResult { script, lookup_status: LookupStatus::Complete, vtxos: Some(vtxos) })
    }

    pub(crate) async fn get_script_vtxos(&self, scripts: Vec<String>) -> anyhow::Result<Vec<IndexerVtxo>> {
        use anyhow::Context;
        let mut vtxos = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cursor = 0;
        loop {
            let response = indexer_service_api::indexer_service_get_vtxos(
                &self.config, Some(scripts.clone()), None,
                None, None, None, None, None, None, None, Some(100), Some(cursor),
            ).await.map_err(map_err)?;
            let page = response.page.context("address response missing page metadata")?;
            let next = page.next.context("address response missing page.next")?;
            let total = page.total.context("address response missing page.total")?;
            anyhow::ensure!(next >= 0 && total >= 0 && next <= total && next >= cursor
                && (next == total || next > cursor), "invalid or nonadvancing address pagination");
            for vtxo in response.vtxos.unwrap_or_default() {
                let outpoint = vtxo.outpoint.as_ref().context("address VTXO missing outpoint")?;
                let txid = outpoint.txid.as_deref().context("address VTXO missing txid")?;
                let vout = outpoint.vout.context("address VTXO missing vout")?;
                crate::outpoint::validate_txid(txid)?;
                anyhow::ensure!(vout >= 0, "invalid address VTXO vout");
                anyhow::ensure!(seen.insert((txid.to_ascii_lowercase(), vout)),
                    "duplicate VTXO in address pagination");
                vtxos.push(vtxo);
            }
            if next == total {
                break;
            }
            cursor = next;
        }
        Ok(vtxos)
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

    /// Fetch the commitment transaction `txid`, or `Ok(None)` if the indexer has
    /// no such commitment (HTTP 404).
    ///
    /// We issue this request ourselves instead of using ark-rest's generated
    /// method: the operator serializes `uint64` amounts as JSON strings, but the
    /// generated `GetCommitmentTxResponse` types them as `i32`, so its decode
    /// fails. Our [`CommitmentTx`] model accepts strings or numbers.
    pub async fn get_commitment_tx(
        &self,
        txid: &str,
    ) -> Result<Option<CommitmentTx>, IndexerError> {
        let url = format!("{}/v1/indexer/commitmentTx/{txid}", self.config.base_path);
        let resp = self
            .config
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| IndexerError::Transport(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = resp
            .text()
            .await
            .map_err(|e| IndexerError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(IndexerError::Response { status, body });
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| IndexerError::Malformed(e.to_string()))
    }
}

fn map_err<T>(err: ArkRestError<T>) -> IndexerError {
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

    #[tokio::test]
    async fn virtual_tx_single_unpaginated_anonymous_request() {
        let server = MockServer::start().await;
        let (txid, psbt) = crate::virtual_tx::tests::fixture([5_000_000_000, 330]);
        Mock::given(method("GET"))
            .and(path(format!("/v1/indexer/virtualTx/{txid}")))
            .and(header_regex("user-agent", "^akcli/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"txs": [psbt]})))
            .expect(1).mount(&server).await;
        let tx = client(&server.uri()).get_virtual_tx(&txid).await.unwrap().unwrap();
        assert_eq!(tx.psbt, psbt);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.query().is_none());
        assert!(!requests[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn virtual_tx_empty_missing_null_multiple_and_corrupt_records() {
        let (txid, psbt) = crate::virtual_tx::tests::fixture([1, 2]);
        for (body, expected) in [
            (serde_json::json!({"txs": []}), None),
            (serde_json::json!({}), Some("missing txs")),
            (serde_json::json!({"txs": null}), Some("missing txs")),
            (serde_json::json!({"txs": [psbt, psbt]}), Some("multiple")),
            (serde_json::json!({"txs": [psbt, "other"]}), Some("multiple")),
            (serde_json::json!({"txs": ["!"]}), Some("base64")),
            (serde_json::json!({"txs": ["bm90IHBzYnQ="]}), Some("PSBT")),
        ] {
            let server = MockServer::start().await;
            Mock::given(path(format!("/v1/indexer/virtualTx/{txid}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1).mount(&server).await;
            let result = client(&server.uri()).get_virtual_tx(&txid).await;
            match expected {
                None => assert!(result.unwrap().is_none()),
                Some(message) => assert!(result.unwrap_err().to_string().contains(message)),
            }
        }
    }

    #[tokio::test]
    async fn virtual_tx_errors_preserve_status_body_malformed_and_timeout() {
        for status in [404, 403, 500, 200] {
            let server = MockServer::start().await;
            Mock::given(path(format!("/v1/indexer/virtualTx/{TXID}")))
                .respond_with(ResponseTemplate::new(status).set_body_string("operator failure"))
                .expect(1).mount(&server).await;
            let error = client(&server.uri()).get_virtual_tx(TXID).await.unwrap_err();
            match error.downcast_ref::<IndexerError>().unwrap() {
                IndexerError::Response { status: actual, body } => {
                    assert_eq!(actual.as_u16(), status);
                    assert_eq!(body, "operator failure");
                }
                IndexerError::Malformed(_) => assert_eq!(status, 200),
                other => panic!("unexpected {other:?}"),
            }
        }
        let server = MockServer::start().await;
        Mock::given(path(format!("/v1/indexer/virtualTx/{TXID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"txs": []}))
                .set_delay(Duration::from_millis(400)))
            .mount(&server).await;
        let client = IndexerClient::new(&server.uri(), Duration::from_millis(50)).unwrap();
        let error = client.get_virtual_tx(TXID).await.unwrap_err();
        assert!(matches!(error.downcast_ref::<IndexerError>(), Some(IndexerError::Transport(_))));
    }

    #[tokio::test]
    async fn virtual_tx_invalid_id_never_sends_http() {
        let server = MockServer::start().await;
        for invalid in ["bad".into(), TXID.to_uppercase(), "g".repeat(64)] {
            assert!(client(&server.uri()).get_virtual_tx(&invalid).await.is_err());
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn script_malformed_and_unsupported_inputs_never_send_http() {
        let server = MockServer::start().await;
        let client = client(&server.uri());
        for raw in ["g0", "0", "4c02ff"] {
            assert!(client.get_script(raw).await.is_err());
        }
        for raw in ["", "6A", "4c0101", "52020000"] {
            let result = client.get_script(raw).await.unwrap();
            let json = serde_json::to_value(result).unwrap();
            assert_eq!(json["lookup_status"], "unsupported");
            assert!(json["vtxos"].is_null());
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    fn client(uri: &str) -> IndexerClient {
        IndexerClient::new(uri, Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn info_get_preserves_protobuf_strings_and_sends_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/info"))
            .and(header_regex("user-agent", "^akcli/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": "0.9.0", "network": "bitcoin", "signerPubkey": "02abcd",
                "utxoMaxAmount": "5000000000", "vtxoMaxAmount": "-1", "dust": "330",
                "sessionDuration": "30", "boardingExitDelay": "1024"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let info = client(&server.uri()).get_info().await.unwrap();
        assert_eq!(info.utxo_max_amount.as_deref(), Some("5000000000"));
        assert_eq!(info.vtxo_max_amount.as_deref(), Some("-1"));
        assert_eq!(info.network.as_deref(), Some("bitcoin"));
    }

    #[tokio::test]
    async fn info_http_errors_preserve_status_and_body_including_404() {
        for status in [404, 500] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/info"))
                .respond_with(ResponseTemplate::new(status).set_body_string("operator failure"))
                .mount(&server)
                .await;
            match client(&server.uri()).get_info().await.unwrap_err() {
                IndexerError::Response { status: actual, body } => {
                    assert_eq!(actual.as_u16(), status);
                    assert_eq!(body, "operator failure");
                }
                other => panic!("expected HTTP error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn info_malformed_response_is_error() {
        for body in ["not json", r#"{"dust":330}"#] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/info"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            assert!(matches!(
                client(&server.uri()).get_info().await.unwrap_err(),
                IndexerError::Malformed(_)
            ));
        }
    }

    #[tokio::test]
    async fn info_honors_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({}))
                .set_delay(Duration::from_millis(400)))
            .mount(&server)
            .await;
        let client = IndexerClient::new(&server.uri(), Duration::from_millis(50)).unwrap();
        assert!(matches!(client.get_info().await.unwrap_err(), IndexerError::Transport(_)));
    }

    #[tokio::test]
    async fn address_uses_one_p2tr_lookup_script_accepted_by_operator() {
        const SCRIPT: &str = "512025a43cecfa0e1b1a4f72d64ad15f4cfa7a84d0723e8511c969aa543638ea9967";
        let server = MockServer::start().await;
        Mock::given(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "network": "signet",
                "signerPubkey": "0233ffb3dee353b1a9ebe4ced64b946238d0a4ac364f275d771da6ad2445d07ae0"
            }))).mount(&server).await;
        let record = serde_json::json!({
            "outpoint": {"txid": TXID, "vout": 0}, "amount": "1000",
            "script": SCRIPT, "isSpent": false
        });
        let response_record = record.clone();
        Mock::given(method("GET")).and(path("/v1/indexer/vtxos"))
            .respond_with(move |request: &wiremock::Request| {
                let scripts: Vec<_> = request.url.query_pairs()
                    .filter(|(key, _)| key == "scripts").map(|(_, value)| value).collect();
                // Model arkd's hex/P2TR boundary, not the SDK's CSV encoding.
                for script in &scripts {
                    let message = if script.len() % 2 != 0
                        || !script.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        Some("invalid script format, must be hex")
                    } else if script.len() != 68 || !script.starts_with("5120") {
                        Some("invalid script, must be P2TR")
                    } else {
                        None
                    };
                    if let Some(message) = message {
                        return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                            "code": 3, "message": message, "details": []
                        }));
                    }
                }
                assert_eq!(scripts, [SCRIPT], "exactly one P2TR lookup script");
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "vtxos": [response_record], "page": {"current": 1, "next": 1, "total": 1}
                }))
            }).expect(1).mount(&server).await;
        let result = client(&server.uri()).get_address(crate::address::tests::ADDRESS)
            .await.expect("operator must accept the address lookup");
        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["address"], crate::address::tests::ADDRESS);
        assert_eq!(json["scripts"], serde_json::json!([SCRIPT]));
        assert_eq!(json["vtxos"], serde_json::json!([record]));
    }

    #[tokio::test]
    async fn address_fetches_every_cursor_and_preserves_full_records() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        Mock::given(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "network": "signet",
                "signerPubkey": "0233ffb3dee353b1a9ebe4ced64b946238d0a4ac364f275d771da6ad2445d07ae0"
            })))
            .mount(&server).await;
        let record = serde_json::json!({
            "outpoint": {"txid": TXID, "vout": 0}, "amount": "123456789",
            "spentBy": TXID, "arkTxid": TXID, "settledBy": TXID,
            "commitmentTxids": [TXID], "isSpent": true
        });
        // A short nonterminal page must not stop traversal; next is a cursor,
        // not the number of records returned. The terminal page can be empty.
        for (cursor, body) in [
            (0, serde_json::json!({"vtxos": [record.clone()], "page": {"next": 7, "total": 9}})),
            (7, serde_json::json!({"vtxos": [], "page": {"next": 9, "total": 9}})),
        ] {
            Mock::given(method("GET")).and(path("/v1/indexer/vtxos"))
                .and(query_param("page.index", cursor.to_string()))
                .and(query_param("page.size", "100"))
                .and(query_param("scripts", "512025a43cecfa0e1b1a4f72d64ad15f4cfa7a84d0723e8511c969aa543638ea9967"))
                .and(header_regex("user-agent", "^akcli/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1).mount(&server).await;
        }
        let result = client(&server.uri()).get_address(crate::address::tests::ADDRESS).await.unwrap();
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["address"], crate::address::tests::ADDRESS);
        assert_eq!(json["scripts"].as_array().unwrap().len(), 1);
        assert_eq!(json["vtxos"], serde_json::json!([record]));
        for request in server.received_requests().await.unwrap() {
            if request.url.path() == "/v1/indexer/vtxos" {
                assert_eq!(request.url.query_pairs().count(), 3, "no status filters");
            }
        }
    }

    #[tokio::test]
    async fn address_fails_closed_on_incomplete_or_inconsistent_pages() {
        use wiremock::matchers::query_param;
        let record = serde_json::json!({"outpoint": {"txid": TXID, "vout": 0}});
        for (body, status, message) in [
            (serde_json::json!({"page": {"next": -1, "total": -1}}), 200, "pagination"),
            (serde_json::json!({"page": {"next": 10, "total": 9}}), 200, "pagination"),
            (serde_json::json!({"page": {"next": 7, "total": 9}}), 200, "pagination"),
            (serde_json::json!({"page": {"next": 0, "total": 9}}), 200, "pagination"),
            (serde_json::json!({}), 200, "missing page"),
            (serde_json::json!({"page": {"total": 9}}), 200, "missing page.next"),
            (serde_json::json!({"page": {"next": 9}}), 200, "missing page.total"),
            (serde_json::json!({"vtxos": [record.clone()], "page": {"next": 9, "total": 9}}), 200, "duplicate"),
            (serde_json::json!({"error": "later failure"}), 500, "HTTP 500"),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/v1/info"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "network": "signet",
                    "signerPubkey": "0233ffb3dee353b1a9ebe4ced64b946238d0a4ac364f275d771da6ad2445d07ae0"
                }))).mount(&server).await;
            Mock::given(path("/v1/indexer/vtxos")).and(query_param("page.index", "0"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "vtxos": [record.clone()], "page": {"next": 7, "total": 9}
                }))).up_to_n_times(1).mount(&server).await;
            Mock::given(path("/v1/indexer/vtxos")).and(query_param("page.index", "7"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .up_to_n_times(1).mount(&server).await;
            let error = client(&server.uri()).get_address(crate::address::tests::ADDRESS)
                .await.unwrap_err().to_string();
            assert!(error.contains(message), "{error} should contain {message}");
        }
    }

    #[tokio::test]
    async fn address_empty_result_succeeds_but_duplicate_or_unidentifiable_records_fail() {
        let record = serde_json::json!({"outpoint": {"txid": TXID, "vout": 0}});
        for (records, succeeds) in [
            (serde_json::json!([]), true),
            (serde_json::json!([record.clone(), record]), false),
            (serde_json::json!([{}]), false),
            (serde_json::json!([{"outpoint": {"txid": TXID}}]), false),
            (serde_json::json!([{"outpoint": {"txid": TXID, "vout": -1}}]), false),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/v1/info"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "network": "signet",
                    "signerPubkey": "0233ffb3dee353b1a9ebe4ced64b946238d0a4ac364f275d771da6ad2445d07ae0"
                }))).mount(&server).await;
            Mock::given(path("/v1/indexer/vtxos"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "vtxos": records, "page": {"next": 0, "total": 0}
                }))).expect(1).mount(&server).await;
            let result = client(&server.uri()).get_address(crate::address::tests::ADDRESS).await;
            assert_eq!(result.is_ok(), succeeds);
            if let Ok(result) = result {
                assert!(result.vtxos.is_empty());
                assert_eq!(serde_json::to_value(result).unwrap()["vtxos"], serde_json::json!([]));
            }
        }
    }

    #[tokio::test]
    async fn address_invalid_input_never_sends_http_and_missing_identity_never_queries_scripts() {
        let server = MockServer::start().await;
        assert!(client(&server.uri()).get_address("invalid").await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
        Mock::given(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1).mount(&server).await;
        assert!(client(&server.uri()).get_address(crate::address::tests::ADDRESS).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
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
    async fn returns_commitment_tx_when_present() {
        let server = MockServer::start().await;
        // Amounts as strings, as the real operator returns them (proto uint64).
        let body = serde_json::json!({
            "startedAt": "100",
            "endedAt": "200",
            "totalInputAmount": "147992953",
            "totalInputVtxos": 12,
            "totalOutputAmount": "147000000",
            "totalOutputVtxos": 12,
            "batches": {}
        });
        Mock::given(method("GET"))
            .and(path(format!("/v1/indexer/commitmentTx/{TXID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let resp = client(&server.uri()).get_commitment_tx(TXID).await.unwrap().unwrap();
        assert_eq!(resp.total_input_vtxos, Some(12));
        assert_eq!(resp.total_output_amount.as_deref(), Some("147000000"));
    }

    #[tokio::test]
    async fn commitment_404_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/indexer/commitmentTx/{TXID}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        assert!(client(&server.uri()).get_commitment_tx(TXID).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn commitment_500_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/indexer/commitmentTx/{TXID}")))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let err = client(&server.uri()).get_commitment_tx(TXID).await.unwrap_err();
        assert!(matches!(err, IndexerError::Response { .. }));
    }

    #[tokio::test]
    async fn commitment_500_batch_not_found_is_response_error() {
        let server = MockServer::start().await;
        let body = r#"{"code":13, "message":"batch not found", "details":[]}"#;
        Mock::given(method("GET"))
            .and(path(format!("/v1/indexer/commitmentTx/{TXID}")))
            .respond_with(ResponseTemplate::new(500).set_body_string(body))
            .mount(&server)
            .await;

        let err = client(&server.uri()).get_commitment_tx(TXID).await.unwrap_err();
        match err {
            IndexerError::Response { status, body: actual_body } => {
                assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(actual_body, body);
            }
            other => panic!("expected Response error, got {other:?}"),
        }
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
