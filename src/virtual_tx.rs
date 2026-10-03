//! Decoded operator-provided virtual transaction PSBTs, without signature verification.

use anyhow::{ensure, Context};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct VirtualTx {
    pub txid: String,
    pub version: i32,
    pub locktime: u32,
    pub input_count: usize,
    pub output_count: usize,
    pub output_total_sat: u64,
    pub inputs: Vec<VirtualTxInput>,
    pub outputs: Vec<VirtualTxOutput>,
    pub psbt: String,
}

#[derive(Debug, Serialize)]
pub struct VirtualTxInput {
    pub previous_outpoint: String,
    pub sequence: u32,
}

#[derive(Debug, Serialize)]
pub struct VirtualTxOutput {
    pub index: usize,
    pub amount_sat: u64,
    pub script_hex: String,
}

impl VirtualTx {
    /// Decode and bind the unsigned transaction to the requested ID; do not verify signatures.
    pub fn decode(requested_txid: &str, original_base64: &str) -> anyhow::Result<Self> {
        let bytes = STANDARD.decode(original_base64).context("invalid virtual transaction base64")?;
        let psbt = bitcoin::psbt::Psbt::deserialize(&bytes).context("invalid virtual transaction PSBT")?;
        let tx = psbt.unsigned_tx;
        let txid = tx.compute_txid().to_string();
        ensure!(txid == requested_txid, "virtual transaction txid mismatch");
        let output_total_sat = tx.output.iter().try_fold(0u64, |sum, output| {
            sum.checked_add(output.value.to_sat()).context("virtual transaction output total overflow")
        })?;
        Ok(Self {
            txid,
            version: tx.version.0,
            locktime: tx.lock_time.to_consensus_u32(),
            input_count: tx.input.len(),
            output_count: tx.output.len(),
            output_total_sat,
            inputs: tx.input.iter().map(|input| VirtualTxInput {
                previous_outpoint: input.previous_output.to_string(),
                sequence: input.sequence.to_consensus_u32(),
            }).collect(),
            outputs: tx.output.iter().enumerate().map(|(index, output)| VirtualTxOutput {
                index,
                amount_sat: output.value.to_sat(),
                script_hex: output.script_pubkey.to_hex_string(),
            }).collect(),
            psbt: original_base64.to_owned(),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use bitcoin::{absolute, transaction, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut};

    pub(crate) fn fixture(amounts: [u64; 2]) -> (String, String) {
        let tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::from_consensus(123),
            input: vec![TxIn {
                previous_output: OutPoint { txid: "11".repeat(32).parse().unwrap(), vout: 7 },
                sequence: Sequence(42),
                ..Default::default()
            }],
            output: amounts.into_iter().map(|amount| TxOut {
                value: Amount::from_sat(amount),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51; 40]),
            }).collect(),
        };
        let txid = tx.compute_txid().to_string();
        let psbt = bitcoin::psbt::Psbt::from_unsigned_tx(tx).unwrap();
        (txid, STANDARD.encode(psbt.serialize()))
    }

    #[test]
    fn virtual_tx_rejects_invalid_encoding_identity_and_overflow() {
        let (txid, psbt) = fixture([5_000_000_000, 330]);
        for (id, encoded, message) in [
            (txid.clone(), "!".into(), "base64"),
            (txid.clone(), STANDARD.encode(b"not a PSBT"), "PSBT"),
            ("00".repeat(32), psbt, "txid mismatch"),
        ] {
            assert!(VirtualTx::decode(&id, &encoded).unwrap_err().to_string().contains(message));
        }
        let (txid, psbt) = fixture([u64::MAX, 1]);
        assert!(VirtualTx::decode(&txid, &psbt).unwrap_err().to_string().contains("overflow"));
    }

    #[test]
    fn virtual_tx_decodes_lossless_psbt_and_integer_fields() {
        let (txid, psbt) = fixture([5_000_000_000, 330]);
        let decoded = VirtualTx::decode(&txid, &psbt).unwrap();
        let json = serde_json::to_value(decoded).unwrap();
        assert_eq!(json, serde_json::json!({
            "txid": txid, "psbt": psbt, "version": 2, "locktime": 123,
            "input_count": 1, "output_count": 2, "output_total_sat": 5_000_000_330u64,
            "inputs": [{"previous_outpoint": format!("{}:7", "11".repeat(32)), "sequence": 42}],
            "outputs": [
                {"index": 0, "amount_sat": 5_000_000_000u64, "script_hex": "51".repeat(40)},
                {"index": 1, "amount_sat": 330, "script_hex": "51".repeat(40)}
            ]
        }));
    }
}
