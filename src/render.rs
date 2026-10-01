//! Rendering of indexer VTXO records for humans and machines.

use ark_rest::models::IndexerVtxo;

use crate::outpoint::Outpoint;

const INDEXER_NOTE: &str =
    "Note: values reported by the operator indexer; not independent cryptographic proof.";

fn or_dash(value: Option<&str>) -> &str {
    match value {
        Some(v) if !v.is_empty() => v,
        _ => "-",
    }
}

/// Compose the status line from the VTXO's boolean flags.
fn status(vtxo: &IndexerVtxo) -> String {
    let mut flags = Vec::new();
    if vtxo.is_spent.unwrap_or(false) {
        flags.push("spent");
    } else {
        flags.push("unspent");
    }
    if vtxo.is_preconfirmed.unwrap_or(false) {
        flags.push("preconfirmed");
    }
    if vtxo.is_swept.unwrap_or(false) {
        flags.push("swept");
    }
    if vtxo.is_unrolled.unwrap_or(false) {
        flags.push("unrolled");
    }
    flags.join(", ")
}

/// Human-readable, labeled rendering of a VTXO.
pub fn render_human(vtxo: &IndexerVtxo, outpoint: &Outpoint) -> String {
    let commitment = vtxo
        .commitment_txids
        .as_ref()
        .map(|c| c.join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-".to_string());

    format!(
        "VTXO {outpoint}\n  \
         amount:      {amount} sat\n  \
         script:      {script}\n  \
         status:      {status}\n  \
         created at:  {created}\n  \
         expires at:  {expires}\n  \
         spent by:    {spent_by}    (spending/checkpoint tx per operator)\n  \
         ark txid:    {ark_txid}    (Arkade transaction)\n  \
         commitment:  {commitment}\n  \
         settled by:  {settled_by}\n{note}",
        amount = or_dash(vtxo.amount.as_deref()),
        script = or_dash(vtxo.script.as_deref()),
        status = status(vtxo),
        created = or_dash(vtxo.created_at.as_deref()),
        expires = or_dash(vtxo.expires_at.as_deref()),
        spent_by = or_dash(vtxo.spent_by.as_deref()),
        ark_txid = or_dash(vtxo.ark_txid.as_deref()),
        commitment = commitment,
        settled_by = or_dash(vtxo.settled_by.as_deref()),
        note = INDEXER_NOTE,
    )
}

/// Machine-readable rendering: the indexer VTXO as pretty JSON.
pub fn render_json(vtxo: &IndexerVtxo) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(vtxo)
}

/// Message shown when the indexer has no record of the outpoint.
pub fn render_not_found(outpoint: &Outpoint) -> String {
    format!("No VTXO found for {outpoint} at the selected operator.")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: &str = "aa11bb22cc33dd44ee55ff66778899001122334455667788990011223344abcd";
    const SPENT_BY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const ARK_TXID: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn outpoint() -> Outpoint {
        Outpoint::parse(&format!("{TXID}:0")).unwrap()
    }

    fn spent_vtxo() -> IndexerVtxo {
        serde_json::from_value(serde_json::json!({
            "amount": "1500",
            "script": "0014deadbeef",
            "isSpent": true,
            "spentBy": SPENT_BY,
            "arkTxid": ARK_TXID,
            "commitmentTxids": ["3333333333333333333333333333333333333333333333333333333333333333"],
            "createdAt": "100",
            "expiresAt": "200",
            "outpoint": { "txid": TXID, "vout": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn human_shows_amount_and_script() {
        let out = render_human(&spent_vtxo(), &outpoint());
        assert!(out.contains("1500 sat"));
        assert!(out.contains("0014deadbeef"));
    }

    #[test]
    fn human_distinguishes_spent_by_from_ark_txid() {
        let out = render_human(&spent_vtxo(), &outpoint());
        // Both identifiers appear, under distinct labels, and are not collapsed.
        assert!(out.contains("spent by:"));
        assert!(out.contains("ark txid:"));
        assert!(out.contains(SPENT_BY));
        assert!(out.contains(ARK_TXID));
        assert_ne!(SPENT_BY, ARK_TXID);
    }

    #[test]
    fn human_marks_spent_status() {
        assert!(render_human(&spent_vtxo(), &outpoint()).contains("status:      spent"));
    }

    #[test]
    fn human_includes_indexer_provenance_note() {
        assert!(render_human(&spent_vtxo(), &outpoint()).contains("not independent cryptographic proof"));
    }

    #[test]
    fn human_renders_missing_fields_as_dash() {
        let bare: IndexerVtxo = serde_json::from_value(serde_json::json!({
            "outpoint": { "txid": TXID, "vout": 0 }
        }))
        .unwrap();
        let out = render_human(&bare, &outpoint());
        assert!(out.contains("spent by:    -"));
        assert!(out.contains("status:      unspent"));
    }

    #[test]
    fn json_is_valid_and_contains_fields() {
        let json = render_json(&spent_vtxo()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["amount"], "1500");
        assert_eq!(parsed["spentBy"], SPENT_BY);
    }

    #[test]
    fn not_found_names_the_outpoint() {
        assert!(render_not_found(&outpoint()).contains(&outpoint().to_string()));
    }
}
