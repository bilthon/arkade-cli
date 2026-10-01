//! Our own commitment-transaction model.
//!
//! The operator serializes protobuf `uint64`/`int64` fields as JSON strings
//! (proto3 JSON mapping), but ark-rest's generated `GetCommitmentTxResponse`
//! types the amounts as `i32`, which both mis-types them and cannot hold a
//! `uint64`. We therefore deserialize the endpoint ourselves into a tolerant
//! model that accepts a string or a number for every numeric field.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A commitment transaction as reported by the indexer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommitmentTx {
    #[serde(rename = "startedAt", default, deserialize_with = "opt_str")]
    pub started_at: Option<String>,
    #[serde(rename = "endedAt", default, deserialize_with = "opt_str")]
    pub ended_at: Option<String>,
    #[serde(rename = "totalInputAmount", default, deserialize_with = "opt_str")]
    pub total_input_amount: Option<String>,
    #[serde(rename = "totalInputVtxos", default, deserialize_with = "opt_i64")]
    pub total_input_vtxos: Option<i64>,
    #[serde(rename = "totalOutputAmount", default, deserialize_with = "opt_str")]
    pub total_output_amount: Option<String>,
    #[serde(rename = "totalOutputVtxos", default, deserialize_with = "opt_i64")]
    pub total_output_vtxos: Option<i64>,
    #[serde(default)]
    pub batches: Option<HashMap<String, CommitmentBatch>>,
}

/// A batch within a commitment transaction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommitmentBatch {
    #[serde(rename = "totalOutputAmount", default, deserialize_with = "opt_str")]
    pub total_output_amount: Option<String>,
    #[serde(rename = "totalOutputVtxos", default, deserialize_with = "opt_i64")]
    pub total_output_vtxos: Option<i64>,
    #[serde(rename = "expiresAt", default, deserialize_with = "opt_str")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub swept: Option<bool>,
}

/// Deserialize an optional field that may be a JSON string, number, or null
/// into `Option<String>` (numbers are stringified).
fn opt_str<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// Deserialize an optional field that may be a JSON string or number into
/// `Option<i64>`.
fn opt_i64<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => s.parse().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_string_amounts_from_operator() {
        // Shape that broke the ark-rest model: amounts as strings.
        let tx: CommitmentTx = serde_json::from_str(
            r#"{"startedAt":"1","endedAt":"2","totalInputAmount":"147992953","totalInputVtxos":3,"totalOutputAmount":"147000000","totalOutputVtxos":3,"batches":{}}"#,
        )
        .unwrap();
        assert_eq!(tx.total_input_amount.as_deref(), Some("147992953"));
        assert_eq!(tx.total_input_vtxos, Some(3));
        assert_eq!(tx.total_output_amount.as_deref(), Some("147000000"));
    }

    #[test]
    fn also_accepts_numeric_amounts_and_string_counts() {
        let tx: CommitmentTx = serde_json::from_str(
            r#"{"totalInputAmount":1200000,"totalInputVtxos":"12"}"#,
        )
        .unwrap();
        assert_eq!(tx.total_input_amount.as_deref(), Some("1200000"));
        assert_eq!(tx.total_input_vtxos, Some(12));
    }

    #[test]
    fn missing_fields_are_none() {
        let tx: CommitmentTx = serde_json::from_str("{}").unwrap();
        assert!(tx.total_input_amount.is_none());
        assert!(tx.batches.is_none());
    }

    #[test]
    fn parses_batches() {
        let tx: CommitmentTx = serde_json::from_str(
            r#"{"batches":{"0":{"totalOutputAmount":"147000000","totalOutputVtxos":3,"expiresAt":"300","swept":false}}}"#,
        )
        .unwrap();
        let b = &tx.batches.unwrap()["0"];
        assert_eq!(b.total_output_amount.as_deref(), Some("147000000"));
        assert_eq!(b.swept, Some(false));
    }
}
