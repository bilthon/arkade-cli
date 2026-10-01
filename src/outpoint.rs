//! Parsing and validation of Arkade/Bitcoin outpoints of the form `<txid>:<vout>`.

use std::fmt;
use std::str::FromStr;

/// A transaction outpoint: a 32-byte txid (64 lowercase hex chars) and a vout index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outpoint {
    pub txid: String,
    pub vout: u32,
}

/// Errors produced while parsing an outpoint string.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OutpointParseError {
    #[error("outpoint must be in the form <txid>:<vout>")]
    MissingSeparator,
    #[error("txid must be 64 hexadecimal characters, got {0}")]
    TxidLength(usize),
    #[error("txid must be lowercase hexadecimal")]
    TxidNotHex,
    #[error("vout must be a non-negative integer: {0}")]
    InvalidVout(String),
}

/// Validate that `txid` is 64 lowercase hexadecimal characters.
pub fn validate_txid(txid: &str) -> Result<(), OutpointParseError> {
    if txid.len() != 64 {
        return Err(OutpointParseError::TxidLength(txid.len()));
    }
    if !txid.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(OutpointParseError::TxidNotHex);
    }
    Ok(())
}

impl Outpoint {
    /// Parse an outpoint from `<txid>:<vout>`.
    pub fn parse(input: &str) -> Result<Self, OutpointParseError> {
        let (txid, vout) = input
            .rsplit_once(':')
            .ok_or(OutpointParseError::MissingSeparator)?;

        validate_txid(txid)?;

        let vout: u32 = vout
            .parse()
            .map_err(|_| OutpointParseError::InvalidVout(vout.to_string()))?;

        Ok(Outpoint {
            txid: txid.to_string(),
            vout,
        })
    }
}

impl FromStr for Outpoint {
    type Err = OutpointParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Outpoint::parse(s)
    }
}

impl fmt::Display for Outpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.txid, self.vout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: &str = "aa11bb22cc33dd44ee55ff66778899001122334455667788990011223344abcd";

    #[test]
    fn parses_valid_outpoint() {
        let op = Outpoint::parse(&format!("{TXID}:0")).unwrap();
        assert_eq!(op.txid, TXID);
        assert_eq!(op.vout, 0);
    }

    #[test]
    fn parses_nonzero_vout() {
        let op = Outpoint::parse(&format!("{TXID}:7")).unwrap();
        assert_eq!(op.vout, 7);
    }

    #[test]
    fn round_trips_through_display() {
        let s = format!("{TXID}:3");
        assert_eq!(Outpoint::parse(&s).unwrap().to_string(), s);
    }

    #[test]
    fn rejects_missing_separator() {
        assert_eq!(
            Outpoint::parse(TXID).unwrap_err(),
            OutpointParseError::MissingSeparator
        );
    }

    #[test]
    fn rejects_short_txid() {
        assert_eq!(
            Outpoint::parse("abcd:0").unwrap_err(),
            OutpointParseError::TxidLength(4)
        );
    }

    #[test]
    fn rejects_non_hex_txid() {
        let bad = "zz11bb22cc33dd44ee55ff66778899001122334455667788990011223344abcd";
        assert_eq!(
            Outpoint::parse(&format!("{bad}:0")).unwrap_err(),
            OutpointParseError::TxidNotHex
        );
    }

    #[test]
    fn rejects_uppercase_txid() {
        let upper = TXID.to_uppercase();
        assert_eq!(
            Outpoint::parse(&format!("{upper}:0")).unwrap_err(),
            OutpointParseError::TxidNotHex
        );
    }

    #[test]
    fn validate_txid_accepts_and_rejects() {
        assert!(validate_txid(TXID).is_ok());
        assert_eq!(validate_txid("abcd").unwrap_err(), OutpointParseError::TxidLength(4));
        assert_eq!(
            validate_txid(&TXID.to_uppercase()).unwrap_err(),
            OutpointParseError::TxidNotHex
        );
    }

    #[test]
    fn rejects_non_numeric_vout() {
        assert_eq!(
            Outpoint::parse(&format!("{TXID}:x")).unwrap_err(),
            OutpointParseError::InvalidVout("x".to_string())
        );
    }

    #[test]
    fn rejects_negative_vout() {
        assert_eq!(
            Outpoint::parse(&format!("{TXID}:-1")).unwrap_err(),
            OutpointParseError::InvalidVout("-1".to_string())
        );
    }
}
