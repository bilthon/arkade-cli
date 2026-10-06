//! Receive-only SDK composition. No key-bearing object survives an await.
use super::Error;
use crate::{
    config::ReceiveSettings,
    wallet::{ReceiveStore, Secret},
};
use ark_client::{Bip32KeyProvider, ContractManager, DiscoverableKeyProvider, SqliteContractStore};
use ark_core::{
    contract::{ContractContext, ContractState, DefaultVtxoContract},
    server::Info,
};
use bitcoin::{
    bip32::{DerivationPath, Xpriv},
    Network, XOnlyPublicKey,
};
use std::str::FromStr;
use zeroize::Zeroizing;

pub(super) async fn operator(endpoint: &str) -> Result<(ReceiveSettings, Info), Error> {
    use crate::indexer::{IndexerClient, IndexerError};
    let client = IndexerClient::new(endpoint, std::time::Duration::from_secs(10))
        .map_err(|_| Error::ReceiveConnection)?;
    let raw = client.get_info().await.map_err(|error| match error {
        IndexerError::Transport(_) => Error::ReceiveConnection,
        IndexerError::Response { .. } => Error::ReceiveHttp,
        IndexerError::Malformed(_) => Error::ReceiveInfo,
    })?;
    let environment = raw.network.as_deref().ok_or(Error::ReceiveInfo)?;
    let network =
        ark_core::server::Network::from_str(environment).map_err(|_| Error::ReceiveInfo)?;
    let settings = ReceiveSettings::new(environment, endpoint).map_err(|_| Error::ReceiveInfo)?;
    let info: Info = raw.try_into().map_err(|_| Error::ReceiveInfo)?;
    if Network::from(network) != info.network {
        return Err(Error::ReceiveInfo);
    }
    Ok((settings, info))
}

pub(super) fn owner(secret: &Secret, network: Network) -> Result<XOnlyPublicKey, Error> {
    let mnemonic = bip39::Mnemonic::parse_in(bip39::Language::English, secret.expose())
        .map_err(|_| Error::Receive)?;
    let seed = Zeroizing::new(mnemonic.to_seed(""));
    let master = Xpriv::new_master(network, seed.as_ref()).map_err(|_| Error::Receive)?;
    let path =
        DerivationPath::from_str(ark_core::DEFAULT_DERIVATION_PATH).map_err(|_| Error::Receive)?;
    // ponytail: fixed index, no cache or rotation cursor. Provider lives only in
    // this synchronous call; SDK private-key copies do not promise zeroization.
    let provider = Bip32KeyProvider::new(master, path);
    let key = provider
        .derive_at_discovery_index(0)
        .map_err(|_| Error::Receive)?
        .ok_or(Error::Receive)?;
    Ok(key.x_only_public_key().0)
}

pub(super) fn persist(
    store: &ReceiveStore,
    settings: &ReceiveSettings,
    info: &Info,
    owner: XOnlyPublicKey,
) -> Result<String, Error> {
    store
        .initialize(settings)
        .map_err(|_| Error::ReceiveStorage)?;
    // Revalidate all SQLite paths immediately before and after SDK pathname open.
    store
        .check_binding(settings)
        .map_err(|_| Error::ReceiveStorage)?;
    let sqlite =
        SqliteContractStore::new(store.database_path()).map_err(|_| Error::ReceiveStorage)?;
    store
        .check_binding(settings)
        .map_err(|_| Error::ReceiveStorage)?;
    let mut manager = ContractManager::new_with_builtins(info.network, Box::new(sqlite))
        .map_err(|_| Error::Receive)?;
    let contract = DefaultVtxoContract {
        server: info.signer_pk.x_only_public_key().0,
        owner,
        exit_delay: info.unilateral_exit_delay,
    };
    // Same persistence-before-address sequence as pinned SDK lib.rs:1290-1317.
    manager
        .insert_or_get(contract.clone(), ContractState::Active, Some(0))
        .map_err(|_| Error::Receive)?;
    let vtxo = contract
        .vtxo(&ContractContext::new(info.network))
        .map_err(|_| Error::Receive)?;
    store
        .check_binding(settings)
        .map_err(|_| Error::ReceiveStorage)?;
    Ok(vtxo.to_ark_address().to_string())
}
