#[test]
fn safe_rest_send_and_reconciliation_primitives_are_public() {
    // Name-level assertions avoid assuming signatures for the PR #278 helpers.
    // These imports must fail to compile against the old unsafe SDK pin.
    #[allow(unused_imports)]
    use ark_core::send::{
        bind_checkpoint_transactions as _, bind_pending_checkpoint_transactions as _,
        verify_signed_ark_transaction as _,
    };

    // Referencing function items performs no requests and constructs no client.
    let _ = ark_rest::apis::ark_service_api::ark_service_get_pending_tx;
    let _ = ark_rest::Client::submit_offchain_transaction_request;
    let _ = ark_rest::Client::finalize_offchain_transaction;
}
