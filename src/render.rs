//! Rendering of indexer VTXO records for humans and machines.

use ark_rest::models::IndexerVtxo;

use crate::commitment::CommitmentTx;
use crate::outpoint::Outpoint;

const INDEXER_NOTE: &str =
    "Note: values reported by the operator indexer; not independent cryptographic proof.";

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Decide whether to emit ANSI color, given an optional `--color` mode
/// (`always`/`never`/`auto`), whether `NO_COLOR` is set, and whether stdout is a
/// terminal. `auto` (or an unknown/absent mode) colors only on a TTY without
/// `NO_COLOR`.
pub fn should_color(mode: Option<&str>, no_color_env: bool, is_tty: bool) -> bool {
    match mode {
        Some("always") => true,
        Some("never") => false,
        _ => is_tty && !no_color_env,
    }
}

fn paint(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn or_dash(value: Option<&str>) -> &str {
    match value {
        Some(v) if !v.is_empty() => v,
        _ => "-",
    }
}

/// Insert thousands separators into a non-negative integer string.
fn with_thousands(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Format an optional satoshi amount as `<1,234> sat (<0.00001234> BTC)`.
/// Missing values render as `-`; non-integer values pass through unchanged.
fn format_amount(raw: Option<&str>) -> String {
    let raw = match raw {
        Some(v) if !v.is_empty() => v,
        _ => return "-".to_string(),
    };
    match raw.parse::<u64>() {
        Ok(sats) => {
            let btc = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
            format!("{} sat ({btc} BTC)", with_thousands(sats))
        }
        Err(_) => raw.to_string(),
    }
}

/// Format an optional unix-timestamp field as a human-readable UTC time with the
/// raw timestamp in parentheses. Missing values render as `-`; values that are
/// not an integer are passed through unchanged (we do not guess their meaning).
fn format_time(raw: Option<&str>) -> String {
    let raw = match raw {
        Some(v) if !v.is_empty() => v,
        _ => return "-".to_string(),
    };
    match raw.parse::<i64>() {
        Ok(secs) => match chrono::DateTime::from_timestamp(secs, 0) {
            Some(dt) => format!("{} ({raw})", dt.format("%Y-%m-%d %H:%M:%S UTC")),
            None => raw.to_string(),
        },
        Err(_) => raw.to_string(),
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

/// Human-readable, labeled rendering of a VTXO. `color` enables ANSI coloring
/// of the status value.
pub fn render_human(vtxo: &IndexerVtxo, outpoint: &Outpoint, color: bool) -> String {
    let spent = vtxo.is_spent.unwrap_or(false);
    let status_code = if spent { RED } else { GREEN };
    let status_display = paint(color, status_code, &status(vtxo));
    let commitment = vtxo
        .commitment_txids
        .as_ref()
        .map(|c| c.join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-".to_string());

    format!(
        "VTXO {outpoint}\n  \
         amount:      {amount}\n  \
         script:      {script}\n  \
         status:      {status}\n  \
         created at:  {created}\n  \
         expires at:  {expires}\n  \
         spent by:    {spent_by}    (checkpoint tx that spends this VTXO)\n  \
         ark txid:    {ark_txid}    (Ark tx that spends it, without the checkpoint)\n  \
         commitment:  {commitment}    (ancestor commitment txs)\n  \
         settled by:  {settled_by}    (commitment tx this VTXO was forfeited onto)\n{note}",
        amount = format_amount(vtxo.amount.as_deref()),
        script = or_dash(vtxo.script.as_deref()),
        status = status_display,
        created = format_time(vtxo.created_at.as_deref()),
        expires = format_time(vtxo.expires_at.as_deref()),
        spent_by = or_dash(vtxo.spent_by.as_deref()),
        ark_txid = or_dash(vtxo.ark_txid.as_deref()),
        commitment = commitment,
        settled_by = or_dash(vtxo.settled_by.as_deref()),
        note = INDEXER_NOTE,
    )
}

/// Machine-readable rendering: any indexer record as pretty JSON.
pub fn render_json<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value)
}

fn count_or_dash(n: Option<i64>) -> String {
    n.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Human-readable rendering of a commitment transaction.
pub fn render_commitment_human(resp: &CommitmentTx, txid: &str) -> String {
    let mut out = format!(
        "Commitment tx {txid}\n  \
         started:     {started}\n  \
         ended:       {ended}\n  \
         inputs:      {in_vtxos} VTXOs, {in_amount}\n  \
         outputs:     {out_vtxos} VTXOs, {out_amount}\n  \
         batches:     {nbatches}",
        started = format_time(resp.started_at.as_deref()),
        ended = format_time(resp.ended_at.as_deref()),
        in_vtxos = count_or_dash(resp.total_input_vtxos),
        in_amount = format_amount(resp.total_input_amount.as_deref()),
        out_vtxos = count_or_dash(resp.total_output_vtxos),
        out_amount = format_amount(resp.total_output_amount.as_deref()),
        nbatches = resp.batches.as_ref().map(|b| b.len()).unwrap_or(0),
    );

    if let Some(batches) = resp.batches.as_ref() {
        // Stable, deterministic ordering by batch id.
        let mut ids: Vec<&String> = batches.keys().collect();
        ids.sort();
        for id in ids {
            let b = &batches[id];
            out.push_str(&format!(
                "\n    {id}: {vtxos} VTXOs, {amount}, expires {expires}, swept {swept}",
                vtxos = count_or_dash(b.total_output_vtxos),
                amount = format_amount(b.total_output_amount.as_deref()),
                expires = format_time(b.expires_at.as_deref()),
                swept = b.swept.unwrap_or(false),
            ));
        }
    }

    out.push('\n');
    out.push_str(INDEXER_NOTE);
    out
}

/// Message shown when the indexer has no such commitment transaction.
pub fn render_commitment_not_found(txid: &str) -> String {
    format!("No commitment transaction {txid} found at the selected operator.")
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
        let out = render_human(&spent_vtxo(), &outpoint(), false);
        assert!(out.contains("amount:      1,500 sat (0.00001500 BTC)\n"));
        assert!(!out.contains("BTC) sat")); // no duplicated unit
        assert!(out.contains("0014deadbeef"));
    }

    #[test]
    fn format_amount_adds_thousands_and_btc() {
        assert_eq!(format_amount(Some("1500")), "1,500 sat (0.00001500 BTC)");
        assert_eq!(format_amount(Some("100000000")), "100,000,000 sat (1.00000000 BTC)");
        assert_eq!(format_amount(Some("0")), "0 sat (0.00000000 BTC)");
    }

    #[test]
    fn format_amount_missing_and_non_numeric() {
        assert_eq!(format_amount(None), "-");
        assert_eq!(format_amount(Some("")), "-");
        assert_eq!(format_amount(Some("abc")), "abc");
    }

    #[test]
    fn human_distinguishes_spent_by_from_ark_txid() {
        let out = render_human(&spent_vtxo(), &outpoint(), false);
        // Both identifiers appear, under distinct labels, and are not collapsed.
        assert!(out.contains("spent by:"));
        assert!(out.contains("ark txid:"));
        assert!(out.contains(SPENT_BY));
        assert!(out.contains(ARK_TXID));
        assert_ne!(SPENT_BY, ARK_TXID);
    }

    #[test]
    fn labels_reflect_official_spend_semantics() {
        let out = render_human(&spent_vtxo(), &outpoint(), false);
        // spent_by is the checkpoint tx; ark_txid is the Ark tx; both describe the spend.
        assert!(out.contains("(checkpoint tx that spends this VTXO)"));
        assert!(out.contains("(Ark tx that spends it, without the checkpoint)"));
        assert!(out.contains("(ancestor commitment txs)"));
        assert!(out.contains("(commitment tx this VTXO was forfeited onto)"));
    }

    #[test]
    fn human_marks_spent_status() {
        assert!(render_human(&spent_vtxo(), &outpoint(), false).contains("status:      spent"));
    }

    #[test]
    fn human_includes_indexer_provenance_note() {
        assert!(render_human(&spent_vtxo(), &outpoint(), false).contains("not independent cryptographic proof"));
    }

    #[test]
    fn human_renders_missing_fields_as_dash() {
        let bare: IndexerVtxo = serde_json::from_value(serde_json::json!({
            "outpoint": { "txid": TXID, "vout": 0 }
        }))
        .unwrap();
        let out = render_human(&bare, &outpoint(), false);
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
    fn formats_unix_timestamp_as_utc_with_raw_in_parens() {
        assert_eq!(format_time(Some("100")), "1970-01-01 00:01:40 UTC (100)");
    }

    #[test]
    fn format_time_missing_is_dash() {
        assert_eq!(format_time(None), "-");
        assert_eq!(format_time(Some("")), "-");
    }

    #[test]
    fn format_time_passes_through_non_numeric() {
        assert_eq!(format_time(Some("2026-10-01T00:00:00Z")), "2026-10-01T00:00:00Z");
    }

    #[test]
    fn human_shows_utc_time_for_created_and_expires() {
        let out = render_human(&spent_vtxo(), &outpoint(), false);
        assert!(out.contains("created at:  1970-01-01 00:01:40 UTC (100)"));
        assert!(out.contains("expires at:  1970-01-01 00:03:20 UTC (200)"));
    }

    #[test]
    fn should_color_modes() {
        assert!(should_color(Some("always"), true, false));
        assert!(!should_color(Some("never"), false, true));
        assert!(should_color(Some("auto"), false, true));
        assert!(!should_color(Some("auto"), true, true)); // NO_COLOR wins
        assert!(!should_color(Some("auto"), false, false)); // not a TTY
        assert!(!should_color(None, false, false));
        assert!(should_color(None, false, true));
    }

    #[test]
    fn color_wraps_status_value_only_when_enabled() {
        let colored = render_human(&spent_vtxo(), &outpoint(), true);
        let plain = render_human(&spent_vtxo(), &outpoint(), false);
        assert!(colored.contains("\x1b[31m")); // spent -> red
        assert!(colored.contains("\x1b[0m"));
        assert!(!plain.contains("\x1b["));
    }

    #[test]
    fn not_found_names_the_outpoint() {
        assert!(render_not_found(&outpoint()).contains(&outpoint().to_string()));
    }

    fn commitment() -> CommitmentTx {
        // Amounts as strings, mirroring the real operator (proto uint64 -> JSON string).
        serde_json::from_value(serde_json::json!({
            "startedAt": "100",
            "endedAt": "200",
            "totalInputAmount": "1200000",
            "totalInputVtxos": 12,
            "totalOutputAmount": "1199000",
            "totalOutputVtxos": 12,
            "batches": {
                "b1": { "expiresAt": "300", "swept": false, "totalOutputAmount": "1199000", "totalOutputVtxos": 12 }
            }
        }))
        .unwrap()
    }

    #[test]
    fn commitment_human_shows_totals_and_times() {
        let out = render_commitment_human(&commitment(), TXID);
        assert!(out.contains(&format!("Commitment tx {TXID}")));
        assert!(out.contains("inputs:      12 VTXOs, 1,200,000 sat (0.01200000 BTC)"));
        assert!(out.contains("outputs:     12 VTXOs, 1,199,000 sat (0.01199000 BTC)"));
        assert!(out.contains("started:     1970-01-01 00:01:40 UTC (100)"));
        assert!(out.contains("not independent cryptographic proof"));
    }

    #[test]
    fn commitment_human_lists_batches() {
        let out = render_commitment_human(&commitment(), TXID);
        assert!(out.contains("batches:     1"));
        assert!(out.contains("b1: 12 VTXOs, 1,199,000 sat (0.01199000 BTC), expires 1970-01-01 00:05:00 UTC (300), swept false"));
    }

    #[test]
    fn commitment_handles_empty_and_missing() {
        let bare: CommitmentTx = serde_json::from_value(serde_json::json!({})).unwrap();
        let out = render_commitment_human(&bare, TXID);
        assert!(out.contains("inputs:      - VTXOs, -"));
        assert!(out.contains("batches:     0"));
    }

    #[test]
    fn commitment_not_found_names_txid() {
        assert!(render_commitment_not_found(TXID).contains(TXID));
    }

    #[test]
    fn json_is_generic_over_records() {
        let json = render_json(&commitment()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["totalInputVtxos"], 12);
    }
}
