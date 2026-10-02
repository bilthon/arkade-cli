//! Validated address queries and their complete indexer results.

use ark_core::ArkAddress;
use bech32::{primitives::decode::CheckedHrpstring, Bech32m};

#[derive(Debug, serde::Serialize)]
pub struct AddressResult {
    pub address: String,
    pub scripts: Vec<String>,
    pub vtxos: Vec<ark_rest::models::IndexerVtxo>,
}

pub struct Address {
    decoded: ArkAddress,
    mainnet: bool,
}

impl Address {
    pub fn decode(raw: &str) -> anyhow::Result<Self> {
        let checked = CheckedHrpstring::new::<Bech32m>(raw)?;
        let mainnet = match checked.hrp().as_str().to_ascii_lowercase().as_str() {
            "ark" => true,
            "tark" => false,
            _ => anyhow::bail!("unsupported Ark address HRP"),
        };
        anyhow::ensure!(checked.byte_iter().next() == Some(0), "unsupported Ark address version");
        Ok(Self { decoded: ArkAddress::decode(raw)?, mainnet })
    }

    pub fn check_operator(&self, info: &ark_rest::models::GetInfoResponse) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::str::FromStr;

        let signer = info.signer_pubkey.as_deref().context("operator missing signerPubkey")?;
        let signer = bitcoin::PublicKey::from_str(signer).context("invalid operator signerPubkey")?;
        anyhow::ensure!(signer.inner.x_only_public_key().0 == self.decoded.server(),
            "address signer does not match operator signerPubkey");
        let mainnet = match info.network.as_deref().context("operator missing network")? {
            "bitcoin" => true,
            "testnet" | "testnet4" | "signet" | "regtest" | "mutinynet" => false,
            network => anyhow::bail!("unsupported operator network: {network}"),
        };
        anyhow::ensure!(mainnet == self.mainnet, "address network family does not match operator");
        Ok(())
    }

    pub fn scripts(&self) -> Vec<String> {
        // The indexer accepts a P2TR lookup key, not the address's OP_RETURN subdust script.
        vec![self.decoded.to_p2tr_script_pubkey().to_hex_string()]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const ADDRESS: &str = "tark1qqellv77udfmr20tun8dvju5vgudpf9vxe8jwhthrkn26fz96pawqfdy8nk05rsmrf8h94j26905e7n6sng8y059z8ykn2j5xcuw4xt846qj6x";

    #[test]
    fn address_validates_encoding_and_operator_identity() {
        let (hrp, bytes) = bech32::decode(ADDRESS).unwrap();
        let encode = |hrp: &str, bytes: &[u8]| {
            bech32::encode::<Bech32m>(bech32::Hrp::parse(hrp).unwrap(), bytes).unwrap()
        };
        let mut bad_version = bytes.clone();
        bad_version[0] = 1;
        let mut bad_key = bytes.clone();
        bad_key[1..33].fill(255);
        for invalid in [
            "invalid".to_string(), ADDRESS.replace("tark1", "taRk1"),
            bech32::encode::<bech32::Bech32>(hrp, &bytes).unwrap(),
            encode("other", &bytes), encode("tark", &bad_version),
            encode("tark", &bad_key), encode("tark", &bytes[..64]),
        ] {
            assert!(Address::decode(&invalid).is_err(), "accepted {invalid}");
        }
        let address = Address::decode(ADDRESS).unwrap();
        let mut info = ark_rest::models::GetInfoResponse::default();
        assert!(address.check_operator(&info).is_err());
        info.signer_pubkey = Some("0233ffb3dee353b1a9ebe4ced64b946238d0a4ac364f275d771da6ad2445d07ae0".into());
        assert!(address.check_operator(&info).is_err());
        for network in ["testnet", "testnet4", "signet", "regtest", "mutinynet"] {
            info.network = Some(network.into());
            address.check_operator(&info).unwrap();
        }
        for network in ["bitcoin", "unknown", ""] {
            info.network = Some(network.into());
            assert!(address.check_operator(&info).is_err());
        }
        info.network = Some("bitcoin".into());
        Address::decode(&encode("ark", &bytes)).unwrap().check_operator(&info).unwrap();
        info.network = Some("signet".into());
        for signer in ["invalid", "0225a43cecfa0e1b1a4f72d64ad15f4cfa7a84d0723e8511c969aa543638ea9967"] {
            info.signer_pubkey = Some(signer.into());
            assert!(address.check_operator(&info).is_err());
        }
    }

    #[test]
    fn address_derives_p2tr_lookup_script() {
        let address = Address::decode(ADDRESS).unwrap();
        assert_eq!(Address::decode(&ADDRESS.to_ascii_uppercase()).unwrap().scripts(), address.scripts());
        assert_eq!(address.scripts(), [
            "512025a43cecfa0e1b1a4f72d64ad15f4cfa7a84d0723e8511c969aa543638ea9967",
        ]);
    }
}
