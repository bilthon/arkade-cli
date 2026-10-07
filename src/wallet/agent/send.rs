//! Public-SDK composition seam for a single retained send (no transport or storage).

use ark_core::{
    intent::{self, Intent, IntentMessage},
    send::{
        bind_checkpoint_transactions, bind_pending_checkpoint_transactions,
        verify_signed_ark_transaction, OffchainTransactions, VtxoInput,
    },
};
use bitcoin::{
    psbt,
    secp256k1::{schnorr, Message},
    Psbt, ScriptBuf, Txid, XOnlyPublicKey,
};
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("invalid retained send material")]
    Retained,
    #[error("pending transaction does not match retained send")]
    Candidate,
    #[error("duplicate pending transaction")]
    Duplicate,
    #[error("unable to construct pending ownership intent")]
    Intent,
}

pub(super) struct PendingCandidate {
    pub(super) ark_txid: Txid,
    pub(super) ark_tx: Psbt,
    pub(super) checkpoint_txs: Vec<Psbt>,
}

pub(super) struct PreparedSend {
    transactions: OffchainTransactions,
    inputs: Vec<VtxoInput>,
    exit_scripts: Vec<ScriptBuf>,
}

impl PreparedSend {
    pub(super) fn new(
        transactions: OffchainTransactions,
        inputs: Vec<VtxoInput>,
        exit_scripts: Vec<ScriptBuf>,
    ) -> Result<Self, Error> {
        let ark = &transactions.ark_tx;
        let checkpoints = &transactions.checkpoint_txs;
        if inputs.is_empty()
            || exit_scripts.is_empty()
            || exit_scripts.iter().any(|script| script.is_empty())
            || checkpoints.len() != inputs.len()
            || ark.unsigned_tx.input.len() != inputs.len()
            || ark.inputs.len() != inputs.len()
            || ark.unsigned_tx.output.is_empty()
            || ark.outputs.len() != ark.unsigned_tx.output.len()
            || inputs.iter().any(|input| !input.assets().is_empty())
            || inputs
                .iter()
                .map(VtxoInput::outpoint)
                .collect::<HashSet<_>>()
                .len()
                != inputs.len()
            || checkpoints.iter().any(|checkpoint| {
                checkpoint.inputs.len() != 1
                    || checkpoint.unsigned_tx.input.len() != 1
                    || checkpoint.unsigned_tx.output.is_empty()
                    || checkpoint.outputs.len() != checkpoint.unsigned_tx.output.len()
            })
        {
            return Err(Error::Retained);
        }
        let rebuilt =
            bind_pending_checkpoint_transactions(&inputs, &exit_scripts, checkpoints.clone())
                .map_err(|_| Error::Retained)?;
        bind_checkpoint_transactions(checkpoints, rebuilt.clone()).map_err(|_| Error::Retained)?;
        for (saved, rebuilt) in checkpoints.iter().zip(&rebuilt) {
            let saved_input = &saved.inputs[0];
            let rebuilt_input = &rebuilt.inputs[0];
            if saved_input.witness_utxo != rebuilt_input.witness_utxo
                || saved_input.tap_scripts != rebuilt_input.tap_scripts
                || saved_input.witness_script != rebuilt_input.witness_script
            {
                return Err(Error::Retained);
            }
        }
        let mut remaining = checkpoints
            .iter()
            .map(|checkpoint| {
                (
                    checkpoint.unsigned_tx.compute_txid(),
                    &checkpoint.unsigned_tx.output[0],
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        for (spent, metadata) in ark.unsigned_tx.input.iter().zip(&ark.inputs) {
            let output = remaining
                .remove(&spent.previous_output.txid)
                .ok_or(Error::Retained)?;
            if spent.previous_output.vout != 0
                || metadata.witness_utxo.as_ref() != Some(output)
                || metadata.tap_scripts.len() != 1
            {
                return Err(Error::Retained);
            }
        }
        Ok(Self {
            transactions,
            inputs,
            exit_scripts,
        })
    }

    pub(super) fn txid(&self) -> Txid {
        self.transactions.ark_tx.unsigned_tx.compute_txid()
    }

    /// None is unresolved: it proves neither completion nor permission to resend.
    pub(super) fn select_pending(
        &self,
        candidates: Vec<PendingCandidate>,
    ) -> Result<Option<PendingCandidate>, Error> {
        let mut matching = candidates
            .into_iter()
            .filter(|candidate| candidate.ark_txid == self.txid());
        let candidate = matching.next();
        if matching.next().is_some() {
            return Err(Error::Duplicate);
        }
        Ok(candidate)
    }

    pub(super) fn validate_pending(&self, candidate: PendingCandidate) -> Result<Vec<Psbt>, Error> {
        if candidate.ark_txid != self.txid()
            || candidate.ark_tx.unsigned_tx.compute_txid() != self.txid()
        {
            return Err(Error::Candidate);
        }
        verify_signed_ark_transaction(&self.transactions.ark_tx, &candidate.ark_tx)
            .map_err(|_| Error::Candidate)?;
        let rebuilt = bind_pending_checkpoint_transactions(
            &self.inputs,
            &self.exit_scripts,
            candidate.checkpoint_txs,
        )
        .map_err(|_| Error::Candidate)?;
        // Reconstruction alone admits subsets. Bind to the complete saved vector
        // last, so signing metadata is exclusively the originally retained data.
        bind_checkpoint_transactions(&self.transactions.checkpoint_txs, rebuilt)
            .map_err(|_| Error::Candidate)
    }
}

pub(super) fn pending_ownership_intent<S>(
    inputs: Vec<intent::Input>,
    sign_owner: S,
) -> Result<Intent, Error>
where
    S: Fn(
        &mut psbt::Input,
        Message,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error>,
{
    if inputs.is_empty()
        || inputs
            .iter()
            .any(|input| input.is_swept() || !input.assets().is_empty())
    {
        return Err(Error::Intent);
    }
    intent::make_intent(
        sign_owner,
        |_, _| {
            Err(ark_core::Error::ad_hoc(
                "onchain ownership intent is unsupported",
            ))
        },
        inputs,
        vec![],
        IntentMessage::GetPendingTx { expire_at: 0 },
    )
    .map_err(|_| Error::Intent)
}

#[cfg(test)]
mod tests {
    use super::{pending_ownership_intent, PendingCandidate, PreparedSend};
    use ark_core::{
        intent::{self, IntentMessage},
        script::{csv_sig_script, multisig_script},
        send::{
            bind_checkpoint_transactions, bind_pending_checkpoint_transactions,
            build_offchain_transactions, sign_ark_transaction, verify_signed_ark_transaction,
            OffchainTransactions, SendReceiver, VtxoInput,
        },
        server::Info,
        ArkAddress,
    };
    use bitcoin::{
        hashes::Hash,
        psbt,
        secp256k1::{schnorr, Keypair, Message, Secp256k1, SecretKey},
        taproot::{LeafVersion, TaprootBuilder},
        Amount, Network, OutPoint, Psbt, ScriptBuf, Sequence, TxOut, Txid, XOnlyPublicKey,
    };

    // Adapted from ark-core send.rs and send/checkpoint/tests.rs at 86bed03.
    // Synthetic keys exist only on the stack; no wallet, network or persistence.
    fn key(n: u8) -> Keypair {
        Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }

    fn pk(n: u8) -> XOnlyPublicKey {
        key(n).x_only_public_key().0
    }

    fn info() -> Info {
        Info {
            version: "test".into(),
            signer_pk: key(1).public_key(),
            forfeit_pk: key(1).public_key(),
            forfeit_address: "bcrt1q8frde3yn78tl9ecgq4anlz909jh0clefhucdur"
                .parse::<bitcoin::Address<_>>()
                .unwrap()
                .require_network(Network::Regtest)
                .unwrap(),
            checkpoint_tapscript: csv_sig_script(Sequence::from_height(144), pk(1)),
            network: Network::Regtest,
            session_duration: 0,
            unilateral_exit_delay: Sequence::from_height(144),
            boarding_exit_delay: Sequence::from_height(144),
            utxo_min_amount: None,
            utxo_max_amount: None,
            vtxo_min_amount: Some(Amount::from_sat(330)),
            vtxo_max_amount: None,
            dust: Amount::from_sat(330),
            fees: None,
            scheduled_session: None,
            deprecated_signers: vec![],
            service_status: Default::default(),
            digest: "test".into(),
            max_tx_weight: 40_000,
            max_op_return_outputs: 3,
        }
    }

    struct Fixture {
        transactions: OffchainTransactions,
        inputs: Vec<VtxoInput>,
        exit_scripts: Vec<ScriptBuf>,
        intent_inputs: Vec<intent::Input>,
    }

    impl Fixture {
        fn new() -> Self {
            let info = info();
            let spend = multisig_script(pk(1), pk(2));
            let scripts = vec![
                spend.clone(),
                csv_sig_script(info.unilateral_exit_delay, pk(2)),
            ];
            let internal: bitcoin::PublicKey = ark_core::UNSPENDABLE_KEY.parse().unwrap();
            let tree = TaprootBuilder::new()
                .add_leaf(1, scripts[0].clone())
                .unwrap()
                .add_leaf(1, scripts[1].clone())
                .unwrap()
                .finalize(&Secp256k1::new(), internal.inner.x_only_public_key().0)
                .unwrap();
            let control = tree
                .control_block(&(spend.clone(), LeafVersion::TapScript))
                .unwrap();
            let address = ArkAddress::new(Network::Regtest, pk(1), tree.output_key());
            let mut inputs = Vec::new();
            let mut intent_inputs = Vec::new();
            for tag in [10, 11] {
                let outpoint = OutPoint::new(Txid::from_byte_array([tag; 32]), 0);
                let amount = Amount::from_sat(1000);
                inputs.push(VtxoInput::new(
                    spend.clone(),
                    None,
                    control.clone(),
                    scripts.clone(),
                    address.to_p2tr_script_pubkey(),
                    amount,
                    outpoint,
                    vec![],
                ));
                intent_inputs.push(intent::Input::new(
                    outpoint,
                    Sequence::MAX,
                    None,
                    TxOut {
                        value: amount,
                        script_pubkey: address.to_p2tr_script_pubkey(),
                    },
                    scripts.clone(),
                    (spend.clone(), control.clone()),
                    false,
                    false,
                    vec![],
                ));
            }
            let receiver = ArkAddress::new(
                Network::Regtest,
                pk(1),
                TaprootBuilder::new()
                    .add_leaf(0, multisig_script(pk(1), pk(3)))
                    .unwrap()
                    .finalize(&Secp256k1::new(), internal.inner.x_only_public_key().0)
                    .unwrap()
                    .output_key(),
            );
            let mut transactions = build_offchain_transactions(
                &[SendReceiver::bitcoin(receiver, Amount::from_sat(1500))],
                &address,
                &inputs,
                &info,
            )
            .unwrap();
            // Retained signing metadata not regenerated by the pending binder.
            transactions.checkpoint_txs[0].inputs[0].sighash_type =
                Some(bitcoin::TapSighashType::Default.into());
            Self {
                transactions,
                inputs,
                intent_inputs,
                exit_scripts: vec![info.checkpoint_tapscript],
            }
        }

        fn prepared(&self) -> PreparedSend {
            PreparedSend::new(
                self.transactions.clone(),
                self.inputs.clone(),
                self.exit_scripts.clone(),
            )
            .unwrap()
        }

        fn candidate(&self) -> PendingCandidate {
            let mut ark_tx = self.transactions.ark_tx.clone();
            for index in 0..ark_tx.inputs.len() {
                sign_ark_transaction(
                    |_, msg| {
                        Ok([1, 2]
                            .into_iter()
                            .map(|n| {
                                (
                                    Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &key(n)),
                                    pk(n),
                                )
                            })
                            .collect())
                    },
                    &mut ark_tx,
                    index,
                )
                .unwrap();
            }
            PendingCandidate {
                ark_txid: ark_tx.unsigned_tx.compute_txid(),
                ark_tx,
                checkpoint_txs: self.transactions.checkpoint_txs.clone(),
            }
        }
    }

    #[test]
    fn local_id_exists_before_submission_and_foreign_returned_id_is_rejected() {
        let f = Fixture::new();
        let saved = f.prepared(); // No submit callback or response is needed.
        assert_eq!(
            saved.txid(),
            f.transactions.ark_tx.unsigned_tx.compute_txid()
        );
        let mut candidate = f.candidate();
        candidate.ark_txid = Txid::from_byte_array([99; 32]);
        assert!(saved.validate_pending(candidate).is_err());
    }

    #[test]
    fn selection_is_exact_unique_and_absence_remains_unresolved() {
        let f = Fixture::new();
        let saved = f.prepared();
        let mut foreign = f.candidate();
        foreign.ark_txid = Txid::from_byte_array([99; 32]);
        // Ok(None) means unknown/unresolved, never completed or safe to resend.
        assert!(saved.select_pending(vec![]).unwrap().is_none());
        assert!(saved.select_pending(vec![foreign]).unwrap().is_none());
        let mut foreign = f.candidate();
        foreign.ark_txid = Txid::from_byte_array([99; 32]);
        let selected = saved
            .select_pending(vec![foreign, f.candidate()])
            .unwrap()
            .unwrap();
        assert_eq!(selected.ark_txid, saved.txid());
        assert!(saved
            .select_pending(vec![f.candidate(), f.candidate()])
            .is_err());
    }

    #[test]
    fn complete_saved_checkpoint_vector_rejects_a_valid_pending_subset() {
        let f = Fixture::new();
        let mut candidate = f.candidate();
        candidate.checkpoint_txs.pop();
        // Establish why pending reconstruction alone is not the local contract.
        let subset = bind_pending_checkpoint_transactions(
            &f.inputs,
            &f.exit_scripts,
            candidate.checkpoint_txs.clone(),
        )
        .unwrap();
        assert!(bind_checkpoint_transactions(&f.transactions.checkpoint_txs, subset).is_err());
        assert!(f.prepared().validate_pending(candidate).is_err());
    }

    #[test]
    fn exact_intent_succeeds_and_signing_metadata_comes_from_saved_psbts() {
        let f = Fixture::new();
        let mut candidate = f.candidate();
        verify_signed_ark_transaction(&f.transactions.ark_tx, &candidate.ark_tx).unwrap();
        candidate.checkpoint_txs.reverse();
        for checkpoint in &mut candidate.checkpoint_txs {
            checkpoint.inputs[0] = psbt::Input::default();
        }
        let bound = f.prepared().validate_pending(candidate).unwrap();
        let mut expected = f.transactions.checkpoint_txs.clone();
        expected.reverse();
        assert_eq!(bound, expected);
    }

    #[test]
    fn changed_ark_transaction_or_missing_signature_is_rejected() {
        let f = Fixture::new();
        let saved = f.prepared();
        let mut changed = f.candidate();
        changed.ark_tx.unsigned_tx.output[0].value = Amount::from_sat(1499);
        assert!(saved.validate_pending(changed).is_err());
        let mut unsigned = f.candidate();
        unsigned.ark_tx.inputs[0].tap_script_sigs.clear();
        assert!(saved.validate_pending(unsigned).is_err());
    }

    #[test]
    fn changed_checkpoint_or_inconsistent_saved_reconstruction_is_rejected() {
        let f = Fixture::new();
        let mut changed = f.candidate();
        changed.checkpoint_txs[0].unsigned_tx.output[0].value = Amount::from_sat(999);
        assert!(f.prepared().validate_pending(changed).is_err());
        // Preserve cardinality and unique outpoints; only the retained amount
        // changes. Unchanged checkpoints must fail public reconstruction.
        let mut inputs = f.inputs.clone();
        let original = &f.inputs[0];
        let (script, control) = original.spend_info();
        inputs[0] = VtxoInput::new(
            script.clone(),
            None,
            control.clone(),
            f.intent_inputs[0].tapscripts().to_vec(),
            original.script_pubkey(),
            Amount::from_sat(999),
            original.outpoint(),
            vec![],
        );
        assert_eq!(inputs.len(), f.inputs.len());
        assert_eq!(
            inputs.iter().map(VtxoInput::outpoint).collect::<Vec<_>>(),
            f.inputs.iter().map(VtxoInput::outpoint).collect::<Vec<_>>(),
        );
        assert!(matches!(
            PreparedSend::new(f.transactions.clone(), inputs, f.exit_scripts.clone()),
            Err(super::Error::Retained),
        ));
    }

    fn sign_owner(
        _: &mut psbt::Input,
        msg: Message,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
        Ok(vec![(
            Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &key(2)),
            pk(2),
        )])
    }

    #[test]
    fn pending_ownership_proof_uses_public_intent_message_and_signing_callback() {
        let f = Fixture::new();
        let expected = intent::make_intent(
            sign_owner,
            |_, _| panic!("BTC VTXOs only"),
            f.intent_inputs.clone(),
            vec![],
            IntentMessage::GetPendingTx { expire_at: 0 },
        )
        .unwrap();
        let actual = pending_ownership_intent(f.intent_inputs, sign_owner).unwrap();
        assert_eq!(
            actual.serialize_message().unwrap(),
            expected.serialize_message().unwrap()
        );
        assert_eq!(actual.serialize_proof(), expected.serialize_proof());
        assert!(!actual.serialize_proof().is_empty());
        assert!(actual
            .proof
            .inputs
            .iter()
            .all(|input| !input.tap_script_sigs.is_empty()));
        let decoded = Psbt::deserialize(&actual.proof.serialize()).unwrap();
        assert_eq!(decoded, actual.proof);
    }
}
