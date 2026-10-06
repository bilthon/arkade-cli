//! Operator-reported offchain holdings, not a proof of funds or spendability.
//! Pagination is not an atomic snapshot; SDK expiry predicates use the local clock.
use super::{ReceiveStore, WalletName};
use crate::{
    config::{ReceiveOperator, ReceiveSettings},
    indexer::IndexerClient,
};
use ark_client::{AnnotatedVtxoList, ContractManager, SqliteContractStore};
use ark_core::{
    contract::ContractType,
    server::{Info, VirtualTxOutPoint},
};
use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    pub offchain_balance_sats: u64,
    pub confirmed_sats: u64,
    pub preconfirmed_sats: u64,
    pub recoverable_sats: u64,
    pub pending_recovery_sats: u64,
}

/// Bounded domain failures: never retain backend bodies, paths, or operator URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceError {
    MissingState,
    Storage,
    Operator,
    Contract,
    Query,
    InvalidVtxo,
    DuplicateOutpoint,
    Overflow,
    Clock,
}

/// Read existing receive state only. SDK opening may initialize SQLite schema;
/// this operation never inserts contracts or derives receiving addresses.
pub async fn read_balance(
    home: &Path,
    name: &WalletName,
    endpoint: &str,
) -> Result<Balance, BalanceError> {
    let store = ReceiveStore::open_existing(home, name).map_err(|_| BalanceError::Storage)?;
    let endpoint = ReceiveOperator::new(endpoint).map_err(|_| BalanceError::Operator)?;
    let client = IndexerClient::new(endpoint.url(), Duration::from_secs(10))
        .map_err(|_| BalanceError::Operator)?;
    let raw = client.get_info().await.map_err(|_| BalanceError::Operator)?;
    let settings = ReceiveSettings::new(
        raw.network.as_deref().ok_or(BalanceError::Operator)?,
        endpoint.url(),
    )
    .map_err(|_| BalanceError::Operator)?;
    let network: ark_core::server::Network = settings
        .environment()
        .parse()
        .map_err(|_| BalanceError::Operator)?;
    let info: Info = raw.try_into().map_err(|_| BalanceError::Operator)?;
    if bitcoin::Network::from(network) != info.network {
        return Err(BalanceError::Operator);
    }
    if !store
        .is_initialized(&settings)
        .map_err(|_| BalanceError::Storage)?
    {
        return Err(BalanceError::MissingState);
    }
    let sqlite = SqliteContractStore::new(store.database_path()).map_err(|_| BalanceError::Storage)?;
    store
        .check_binding(&settings)
        .map_err(|_| BalanceError::Storage)?;
    let manager = ContractManager::new_with_builtins(info.network, Box::new(sqlite))
        .map_err(|_| BalanceError::Contract)?;
    let result = aggregate(&manager, &client, &info).await?;
    store
        .check_binding(&settings)
        .map_err(|_| BalanceError::Storage)?;
    Ok(result)
}

async fn aggregate(
    manager: &ContractManager,
    client: &IndexerClient,
    info: &Info,
) -> Result<Balance, BalanceError> {
    let mut scripts = BTreeSet::new();
    for contract in manager.list().map_err(|_| BalanceError::Contract)? {
        // Validate even inactive, duplicate-script, and zero-holdings contracts.
        manager
            .spendable_selections(&contract)
            .map_err(|_| BalanceError::Contract)?;
        // Only default VTXO contracts belong to this CLI's receive lifecycle,
        // including historical signer/policy variants. Other builtins fail closed.
        if contract.contract_type != ContractType::default_vtxo()
            || !contract.script_pubkey.is_p2tr()
        {
            return Err(BalanceError::Contract);
        }
        scripts.insert(contract.script_pubkey);
    }
    let mut seen = HashSet::new();
    let mut vtxos = Vec::new();
    for script in scripts {
        // One script per request: the generated REST client's CSV encoding is
        // not evidence that an operator accepts multiple scripts.
        for raw in client
            .get_script_vtxos(vec![script.to_hex_string()])
            .await
            .map_err(|_| BalanceError::Query)?
        {
            let vout = raw
                .outpoint
                .as_ref()
                .and_then(|o| o.vout)
                .ok_or(BalanceError::InvalidVtxo)?;
            if u32::try_from(vout).is_err()
                || u32::try_from(raw.depth.unwrap_or_default()).is_err()
                || raw.assets.as_ref().is_some_and(|assets| {
                    assets.iter().any(|asset| {
                        asset.amount.is_none_or(|amount| amount < 0) || asset.asset_id.is_none()
                    })
                })
            {
                return Err(BalanceError::InvalidVtxo);
            }
            // Proto3 boolean omissions mean false, as in the SDK conversion.
            // Guard its lossy integer casts above rather than reimplementing it.
            let vtxo: VirtualTxOutPoint = raw.try_into().map_err(|_| BalanceError::InvalidVtxo)?;
            if vtxo.script != script {
                return Err(BalanceError::InvalidVtxo);
            }
            if !seen.insert(vtxo.outpoint) {
                return Err(BalanceError::DuplicateOutpoint);
            }
            vtxos.push(vtxo);
        }
    }
    let list = AnnotatedVtxoList::new(
        info.dust,
        manager.annotate_vtxos(vtxos).map_err(|_| BalanceError::Contract)?,
    );
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BalanceError::Clock)?
            .as_secs(),
    )
    .map_err(|_| BalanceError::Clock)?;
    let eligible: HashSet<_> = list
        .spendable_offchain_at(info, now)
        .map(|entry| entry.vtxo().outpoint)
        .collect();
    // Same bucket predicates and signer filtering as Client::offchain_balance,
    // with checked sums instead of Amount's unchecked addition.
    let confirmed_sats = checked_sum(list.confirmed()
        .filter(|e| eligible.contains(&e.vtxo().outpoint))
        .map(|e| e.vtxo().amount.to_sat()))?;
    let preconfirmed_sats = checked_sum(list.pre_confirmed()
        .filter(|e| eligible.contains(&e.vtxo().outpoint))
        .map(|e| e.vtxo().amount.to_sat()))?;
    let recoverable_sats = checked_sum(list.recoverable().map(|e| e.vtxo().amount.to_sat()))?;
    let pending_recovery_sats = checked_sum(list
        .pending_recovery_due_to_signer_at(info, now)
        .map(|e| e.vtxo().amount.to_sat()))?;
    let offchain_balance_sats = checked_sum([
        confirmed_sats, preconfirmed_sats, recoverable_sats, pending_recovery_sats,
    ])?;
    Ok(Balance {
        offchain_balance_sats,
        confirmed_sats,
        preconfirmed_sats,
        recoverable_sats,
        pending_recovery_sats,
    })
}

fn checked_sum(amounts: impl IntoIterator<Item = u64>) -> Result<u64, BalanceError> {
    amounts.into_iter().try_fold(0u64, |total, amount| {
        total.checked_add(amount).ok_or(BalanceError::Overflow)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ReceiveSettings, wallet::{ReceiveStore, WalletName}};
    use std::{fs, os::unix::fs::PermissionsExt};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wiremock::matchers::path;

    fn key(byte: u8) -> bitcoin::secp256k1::PublicKey {
        bitcoin::secp256k1::SecretKey::from_slice(&[byte; 32]).unwrap()
            .public_key(&bitcoin::secp256k1::Secp256k1::new())
    }

    fn info() -> serde_json::Value {
        let signer = key(2);
        serde_json::json!({
            "version": "synthetic", "signerPubkey": signer.to_string(),
            "forfeitPubkey": signer.to_string(),
            "forfeitAddress": bitcoin::Address::p2tr(
                &bitcoin::secp256k1::Secp256k1::new(), signer.x_only_public_key().0,
                None, bitcoin::Network::Regtest).to_string(),
            "network": "regtest", "checkpointTapscript": "",
            "unilateralExitDelay": "144", "boardingExitDelay": "144",
            "sessionDuration": "10", "dust": "330",
            "maxTxWeight": "0", "maxOpReturnOutputs": "0"
        })
    }

    async fn fixture() -> (tempfile::TempDir, WalletName, MockServer, ReceiveStore, ReceiveSettings) {
        let home = tempfile::tempdir().unwrap();
        let name = WalletName::new("alice").unwrap();
        for part in [".akcli", ".akcli/wallets", ".akcli/wallets/alice"] {
            let path = home.path().join(part);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let server = MockServer::start().await;
        Mock::given(path("/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(info()))
            .mount(&server).await;
        let settings = ReceiveSettings::new("regtest", &server.uri()).unwrap();
        let store = ReceiveStore::open_existing(home.path(), &name).unwrap();
        (home, name, server, store, settings)
    }

    fn manager(store: &ReceiveStore) -> ContractManager {
        ContractManager::new_with_builtins(bitcoin::Network::Regtest,
            Box::new(SqliteContractStore::new(store.database_path()).unwrap())).unwrap()
    }

    fn insert(manager: &mut ContractManager, signer: u8, delay: u16, inactive: bool) -> String {
        use ark_core::contract::{ContractState, DefaultVtxoContract};
        let contract = DefaultVtxoContract {
            server: key(signer).x_only_public_key().0,
            owner: key(1).x_only_public_key().0,
            exit_delay: bitcoin::Sequence::from_height(delay),
        };
        let stored = manager.insert_or_get(contract,
            if inactive { ContractState::Inactive } else { ContractState::Active }, Some(0)).unwrap();
        stored.script_pubkey.to_hex_string()
    }

    fn record(script: &str, id: u32, amount: u64) -> serde_json::Value {
        serde_json::json!({
            "outpoint": {"txid": "11".repeat(32), "vout": id},
            "script": script, "amount": amount.to_string(),
            "createdAt": "1", "expiresAt": "4102444800"
        })
    }

    async fn page(server: &MockServer, script: &str, cursor: u32, next: u32, total: u32, records: Vec<serde_json::Value>) {
        use wiremock::matchers::{method, query_param};
        Mock::given(method("GET")).and(path("/v1/indexer/vtxos"))
            .and(query_param("scripts", script))
            .and(query_param("page.index", cursor.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "vtxos": records, "page": {"next": next, "total": total}
            }))).expect(1).mount(server).await;
    }

    #[tokio::test]
    async fn historical_inactive_policies_all_pages_and_omitted_flags_are_included() {
        let (home, name, server, store, settings) = fixture().await;
        store.initialize(&settings).unwrap();
        let mut manager = manager(&store);
        let current = insert(&mut manager, 2, 144, false);
        let historical = insert(&mut manager, 3, 288, true);
        let policy = insert(&mut manager, 2, 288, true);
        let before = manager.list().unwrap();
        page(&server, &current, 0, 7, 9, vec![record(&current, 0, 1000)]).await;
        page(&server, &current, 7, 9, 9, vec![record(&current, 1, 2000)]).await;
        page(&server, &historical, 0, 1, 1, vec![record(&historical, 2, 3000)]).await;
        page(&server, &policy, 0, 0, 0, vec![]).await;
        let balance = read_balance(home.path(), &name, &server.uri()).await.unwrap();
        assert_eq!(balance, Balance { confirmed_sats: 6000, offchain_balance_sats: 6000, ..Balance::default() });
        assert_eq!(manager.list().unwrap(), before, "no insertion, activation, or derivation index changes");
        for request in server.received_requests().await.unwrap() {
            assert_eq!(request.method.as_str(), "GET");
            if request.url.path() == "/v1/indexer/vtxos" {
                assert_eq!(request.url.query_pairs().count(), 3, "one script, pagination, no filters");
            }
        }
    }

    #[tokio::test]
    async fn global_duplicate_outpoints_and_wrong_scripts_fail_without_totals() {
        for duplicate in [true, false] {
            let (home, name, server, store, settings) = fixture().await;
            store.initialize(&settings).unwrap();
            let mut manager = manager(&store);
            let first = insert(&mut manager, 2, 144, false);
            let second = insert(&mut manager, 3, 144, true);
            // Sorted requests make the duplicate deterministic.
            let mut scripts = [first, second];
            scripts.sort();
            page(&server, &scripts[0], 0, 1, 1, vec![record(&scripts[0], 0, 1000)]).await;
            let script = if duplicate { &scripts[1] } else { &scripts[0] };
            page(&server, &scripts[1], 0, 1, 1, vec![record(script, 0, 2000)]).await;
            assert_eq!(read_balance(home.path(), &name, &server.uri()).await,
                Err(if duplicate { BalanceError::DuplicateOutpoint } else { BalanceError::InvalidVtxo }));
        }
    }

    #[tokio::test]
    async fn later_page_failure_stalled_cursor_and_page_duplicates_abort() {
        use wiremock::matchers::query_param;
        for failure in ["http", "cursor", "duplicate"] {
            let (home, name, server, store, settings) = fixture().await;
            store.initialize(&settings).unwrap();
            let script = insert(&mut manager(&store), 2, 144, false);
            page(&server, &script, 0, 7, 9, vec![record(&script, 0, 1000)]).await;
            if failure == "http" {
                Mock::given(path("/v1/indexer/vtxos")).and(query_param("page.index", "7"))
                    .respond_with(ResponseTemplate::new(500).set_body_string("synthetic private backend details"))
                    .expect(1).mount(&server).await;
            } else {
                page(&server, &script, 7, if failure == "cursor" { 7 } else { 9 }, 9,
                    vec![record(&script, 0, 1000)]).await;
            }
            assert_eq!(read_balance(home.path(), &name, &server.uri()).await, Err(BalanceError::Query));
        }
    }

    #[tokio::test]
    async fn sdk_buckets_include_expiry_dust_and_deprecated_signer_cutoffs() {
        let (home, name, server, store, settings) = fixture().await;
        let mut operator = info();
        operator["deprecatedSigners"] = serde_json::json!([
            {"pubkey": key(3).to_string(), "cutoffDate": "1"},
            {"pubkey": key(4).to_string(), "cutoffDate": "0"},
            {"pubkey": key(5).to_string(), "cutoffDate": "4102444800"}
        ]);
        Mock::given(path("/v1/info")).respond_with(ResponseTemplate::new(200).set_body_json(operator))
            .with_priority(1).mount(&server).await;
        store.initialize(&settings).unwrap();
        let mut manager = manager(&store);
        let current = insert(&mut manager, 2, 144, false);
        let mut records = vec![];
        for (id, amount, changes) in [
            (0, 1000, serde_json::json!({})),
            (1, 2000, serde_json::json!({"isPreconfirmed": true})),
            (2, 3000, serde_json::json!({"isSwept": true})),
            (3, 100, serde_json::json!({})),
            (4, 4000, serde_json::json!({"expiresAt": "1"})),
            (5, 5000, serde_json::json!({"isSpent": true, "isSwept": true})),
            (6, 6000, serde_json::json!({"isUnrolled": true})),
        ] {
            let mut value = record(&current, id, amount);
            value.as_object_mut().unwrap().extend(changes.as_object().unwrap().clone());
            records.push(value);
        }
        page(&server, &current, 0, 1, 1, records).await;
        for (signer, amount) in [(3, 7000), (4, 8000), (5, 9000)] {
            let script = insert(&mut manager, signer, 144, true);
            page(&server, &script, 0, 1, 1, vec![record(&script, signer as u32 + 10, amount)]).await;
        }
        assert_eq!(read_balance(home.path(), &name, &server.uri()).await.unwrap(), Balance {
            confirmed_sats: 18000, preconfirmed_sats: 2000, recoverable_sats: 7100,
            pending_recovery_sats: 7000, offchain_balance_sats: 34100,
        });
    }

    #[tokio::test]
    async fn malformed_records_and_lossy_sdk_casts_are_rejected() {
        for patch in [
            serde_json::json!({"outpoint": {"txid": "11".repeat(32), "vout": 4294967296u64}}),
            serde_json::json!({"depth": -1}),
            serde_json::json!({"depth": 4294967296u64}),
            serde_json::json!({"amount": "18446744073709551616"}),
            serde_json::json!({"amount": "-1"}),
            serde_json::json!({"createdAt": null}),
            serde_json::json!({"expiresAt": "invalid"}),
            serde_json::json!({"spentBy": "invalid"}),
            serde_json::json!({"script": "00"}),
            serde_json::json!({"assets": [{"amount": -1, "assetId": "invalid"}]}),
            serde_json::json!({"assets": [{}]}),
        ] {
            let (home, name, server, store, settings) = fixture().await;
            store.initialize(&settings).unwrap();
            let script = insert(&mut manager(&store), 2, 144, false);
            let mut value = record(&script, 0, 1000);
            value.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
            page(&server, &script, 0, 1, 1, vec![value]).await;
            // Generated REST models use i32: larger unsigned protocol values
            // fail JSON decoding before reaching the SDK's unchecked casts.
            let expected = if patch["outpoint"]["vout"].as_u64() == Some(4294967296)
                || patch["depth"].as_u64() == Some(4294967296) {
                BalanceError::Query
            } else {
                BalanceError::InvalidVtxo
            };
            assert_eq!(read_balance(home.path(), &name, &server.uri()).await, Err(expected), "{patch}");
        }
    }

    #[tokio::test]
    async fn checked_overflow_in_bucket_and_across_buckets() {
        for cross_bucket in [false, true] {
            let (home, name, server, store, settings) = fixture().await;
            store.initialize(&settings).unwrap();
            let script = insert(&mut manager(&store), 2, 144, false);
            let mut second = record(&script, 1, 1000);
            second["isPreconfirmed"] = cross_bucket.into();
            page(&server, &script, 0, 1, 1, vec![record(&script, 0, u64::MAX), second]).await;
            assert_eq!(read_balance(home.path(), &name, &server.uri()).await, Err(BalanceError::Overflow));
        }
    }

    #[tokio::test]
    async fn partial_and_incompatible_bindings_are_unchanged() {
        for mode in ["partial", "environment", "operator", "profile", "version", "unsafe"] {
            let (home, name, server, store, settings) = fixture().await;
            if mode != "partial" {
                store.initialize(&settings).unwrap();
            }
            let metadata = store.database_path().with_file_name("metadata.json");
            let mut binding = serde_json::json!({
                "version": 2, "environment": "regtest", "operator_rest_url": server.uri(),
                "derivation_profile": "sdk65b383e-default-empty-passphrase-index0"
            });
            match mode {
                "environment" => binding["environment"] = "signet".into(),
                "operator" => binding["operator_rest_url"] = "http://localhost:1".into(),
                "profile" => binding["derivation_profile"] = "unsupported".into(),
                "version" => binding["version"] = 1.into(),
                _ => {}
            }
            let bytes = serde_json::to_vec(&binding).unwrap();
            fs::write(&metadata, &bytes).unwrap();
            fs::set_permissions(&metadata, fs::Permissions::from_mode(if mode == "unsafe" { 0o644 } else { 0o600 })).unwrap();
            let expected = if mode == "partial" { BalanceError::MissingState } else { BalanceError::Storage };
            assert_eq!(read_balance(home.path(), &name, &server.uri()).await, Err(expected));
            assert_eq!(fs::read(metadata).unwrap(), bytes);
            if mode == "partial" {
                assert!(!store.database_path().exists());
            } else {
                assert_eq!(fs::metadata(store.database_path()).unwrap().len(), 0, "no SDK open on invalid binding");
            }
        }
    }

    #[tokio::test]
    async fn valid_boarding_contracts_are_rejected_before_querying_holdings() {
        use ark_core::contract::{BoardingContract, ContractState};

        for state in [ContractState::Active, ContractState::Inactive] {
            let (home, name, server, store, settings) = fixture().await;
            store.initialize(&settings).unwrap();
            let mut manager = manager(&store);
            manager.insert_or_get(
                BoardingContract {
                    server: key(2).x_only_public_key().0,
                    owner: key(1).x_only_public_key().0,
                    exit_delay: bitcoin::Sequence::from_height(144),
                },
                state,
                Some(0),
            ).unwrap();
            let before = manager.list().unwrap();
            assert_eq!(before[0].contract_type, ContractType::boarding());
            manager.spendable_selections(&before[0]).unwrap();
            // A valid empty REST response makes accidental acceptance observable
            // as zero, rather than an unrelated query error.
            Mock::given(path("/v1/indexer/vtxos"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "vtxos": [], "page": {"next": 0, "total": 0}
                })))
                .mount(&server).await;

            assert_eq!(
                read_balance(home.path(), &name, &server.uri()).await,
                Err(BalanceError::Contract),
            );
            assert_eq!(manager.list().unwrap(), before);
            assert!(server.received_requests().await.unwrap().iter()
                .all(|request| request.url.path() == "/v1/info"));
        }
    }

    #[tokio::test]
    async fn malformed_or_unknown_saved_contracts_fail_even_without_holdings() {
        use ark_client::ContractStore;
        use ark_core::contract::ContractType;
        for mode in ["type", "version", "data", "script"] {
            let (home, name, server, store, settings) = fixture().await;
            store.initialize(&settings).unwrap();
            let mut manager = manager(&store);
            insert(&mut manager, 2, 144, false);
            let mut invalid = manager.list().unwrap().pop().unwrap();
            // Public store API permits synthetic invalid rows. Production must
            // validate the whole list, not just contracts with returned VTXOs.
            invalid.script_pubkey = bitcoin::ScriptBuf::new();
            match mode {
                "type" => invalid.contract_type = ContractType::new("unknown").unwrap(),
                "version" => invalid.contract_version = u32::MAX,
                "data" => invalid.data = serde_json::json!({}),
                _ => {}
            }
            let mut sqlite = SqliteContractStore::new(store.database_path()).unwrap();
            sqlite.insert(invalid).unwrap();
            assert_eq!(read_balance(home.path(), &name, &server.uri()).await, Err(BalanceError::Contract));
            assert!(server.received_requests().await.unwrap().iter().all(|r| r.url.path() == "/v1/info"));
            assert_eq!(sqlite.list().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn duplicate_stored_scripts_are_queried_once_without_store_writes() {
        use ark_client::ContractStore;
        use ark_core::contract::{ContractState, StoredContract};
        // SQLite's primary key prevents duplicate rows. Exercise defensive
        // deduplication via a test-only view over the real public SQLite store.
        struct Repeated(SqliteContractStore);
        impl ContractStore for Repeated {
            fn list(&self) -> Result<Vec<StoredContract>, ark_client::Error> {
                let rows = self.0.list()?;
                Ok(rows.iter().chain(rows.iter()).cloned().collect())
            }
            fn get_by_script(&self, script: &bitcoin::Script) -> Result<Option<StoredContract>, ark_client::Error> {
                self.0.get_by_script(script)
            }
            fn insert(&mut self, _: StoredContract) -> Result<(), ark_client::Error> {
                panic!("balance must not insert contracts")
            }
            fn update_state(&mut self, _: &bitcoin::Script, _: ContractState) -> Result<(), ark_client::Error> {
                panic!("balance must not update contract state")
            }
        }
        let (_home, _name, server, store, settings) = fixture().await;
        store.initialize(&settings).unwrap();
        let script = insert(&mut manager(&store), 2, 144, false);
        let manager = ContractManager::new_with_builtins(bitcoin::Network::Regtest,
            Box::new(Repeated(SqliteContractStore::new(store.database_path()).unwrap()))).unwrap();
        let info: Info = serde_json::from_value::<ark_rest::models::GetInfoResponse>(info()).unwrap().try_into().unwrap();
        let client = IndexerClient::new(&server.uri(), Duration::from_secs(5)).unwrap();
        page(&server, &script, 0, 1, 1, vec![record(&script, 0, 1000)]).await;
        assert_eq!(aggregate(&manager, &client, &info).await.unwrap().offchain_balance_sats, 1000);
    }

    #[tokio::test]
    async fn sqlite_wal_auxiliaries_remain_private_and_are_not_claimed_read_only() {
        let (home, name, server, store, settings) = fixture().await;
        store.initialize(&settings).unwrap();
        drop(manager(&store));
        let connection = rusqlite::Connection::open(store.database_path()).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; BEGIN; SELECT * FROM contracts;").unwrap();
        for suffix in ["wal", "shm"] {
            let file = store.database_path().with_file_name(format!("contracts.db-{suffix}"));
            assert!(file.exists());
            assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(read_balance(home.path(), &name, &server.uri()).await.unwrap(), Balance::default());
        store.check_binding(&settings).unwrap();
        // SQLite owns auxiliary lifetime/contents; byte-for-byte immutability
        // and protection from hostile same-user pathname swaps are not promised.
    }

    #[tokio::test]
    async fn missing_state_is_not_zero_and_initialized_empty_store_is_zero() {
        let (home, name, server, store, settings) = fixture().await;
        assert_eq!(read_balance(home.path(), &name, &server.uri()).await,
            Err(BalanceError::MissingState));
        store.initialize(&settings).unwrap();
        assert_eq!(fs::metadata(store.database_path()).unwrap().len(), 0);
        assert_eq!(read_balance(home.path(), &name, &server.uri()).await.unwrap(), Balance::default());
        // Accepted SDK opening initializes the precreated empty SQLite file.
        assert!(fs::metadata(store.database_path()).unwrap().len() > 0);
        let sqlite = ark_client::SqliteContractStore::new(store.database_path()).unwrap();
        assert!(ark_client::ContractStore::list(&sqlite).unwrap().is_empty());
    }
}
