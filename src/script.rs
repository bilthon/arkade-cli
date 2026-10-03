//! Structural script decoding, not execution or key validation.

use anyhow::Context;
use ark_rest::models::IndexerVtxo;
use bitcoin::ScriptBuf;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ScriptSummary {
    pub hex: String,
    #[serde(rename = "type")]
    pub script_type: &'static str,
    pub asm: String,
    pub witness_version: Option<u8>,
    pub p2tr_output_key: Option<String>,
}

impl ScriptSummary {
    pub fn decode(raw: &str) -> anyhow::Result<Self> {
        let script = ScriptBuf::from_hex(raw).context("invalid script hex")?;
        for instruction in script.instructions() {
            instruction.context("truncated script push instruction")?;
        }
        let script_type = if script.is_p2tr() {
            "p2tr"
        } else if script.is_p2wpkh() {
            "p2wpkh"
        } else if script.is_p2wsh() {
            "p2wsh"
        } else if script.is_p2pkh() {
            "p2pkh"
        } else if script.is_p2sh() {
            "p2sh"
        } else if script.is_p2pk() {
            "p2pk"
        } else if script.is_multisig() {
            "multisig"
        } else if script.is_op_return() {
            "op_return"
        } else {
            "unknown"
        };
        let hex = script.to_hex_string();
        // is_p2tr checks the exact 34-byte structure, not curve membership.
        let p2tr_output_key = script.is_p2tr().then(|| hex[4..].to_owned());
        Ok(Self {
            hex,
            script_type,
            asm: script.to_asm_string(),
            witness_version: script.witness_version().map(|version| version.to_num()),
            p2tr_output_key,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LookupStatus {
    Complete,
    Unsupported,
}

#[derive(Debug, Serialize)]
pub struct ScriptResult {
    pub script: ScriptSummary,
    pub lookup_status: LookupStatus,
    pub vtxos: Option<Vec<IndexerVtxo>>,
}

impl ScriptResult {
    pub fn unsupported(script: ScriptSummary) -> Self {
        Self {
            script,
            lookup_status: LookupStatus::Unsupported,
            vtxos: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_classifies_standard_forms_and_unknown_witness_versions() {
        for (hex, kind, version) in [
            (format!("76a914{}88ac", "00".repeat(20)), "p2pkh", None),
            (format!("a914{}87", "00".repeat(20)), "p2sh", None),
            (format!("0014{}", "00".repeat(20)), "p2wpkh", Some(0)),
            (format!("0020{}", "00".repeat(32)), "p2wsh", Some(0)),
            (format!("2102{}ac", "00".repeat(32)), "p2pk", None),
            (format!("512102{}51ae", "00".repeat(32)), "multisig", None),
            ("52020000".into(), "unknown", Some(2)),
            ("51".into(), "unknown", None),
            ("".into(), "unknown", None),
        ] {
            let summary = ScriptSummary::decode(&hex).unwrap();
            assert_eq!(summary.script_type, kind, "{hex}");
            assert_eq!(summary.witness_version, version, "{hex}");
            assert_eq!(summary.p2tr_output_key, None);
        }
    }

    #[test]
    fn script_decode_validates_bytecode_and_preserves_output_key_bytes() {
        for raw in ["g0", "0", "01", "4c", "4c02ff", "4d0100", "4e01000000"] {
            assert!(ScriptSummary::decode(raw).is_err(), "{raw}");
        }
        for raw in ["", "4c0101", "ff"] {
            assert_eq!(ScriptSummary::decode(raw).unwrap().hex, raw);
        }
        let summary = ScriptSummary::decode("6A01FF").unwrap();
        assert_eq!(summary.hex, "6a01ff");
        assert_eq!(summary.script_type, "op_return");
        assert_eq!(summary.asm, "OP_RETURN OP_PUSHBYTES_1 ff");
        assert_eq!(summary.witness_version, None);
        assert_eq!(summary.p2tr_output_key, None);
        let key = "FF".repeat(32); // Structural P2TR, not a valid curve point.
        let summary = ScriptSummary::decode(&format!("5120{key}")).unwrap();
        assert_eq!(summary.script_type, "p2tr");
        assert_eq!(summary.witness_version, Some(1));
        assert_eq!(summary.p2tr_output_key, Some("ff".repeat(32)));
    }
}
