# arkade-cli (`akcli`)

A small, read-only explorer for Arkade virtual transactions and outputs — think
of the exploratory convenience of `bitcoin-cli`, but for Arkade VTXOs.

It is **read-only**: no signing, no seed phrases, no wallet, no service startup.

## Install

```sh
cargo install --path .
# or run from the repo:
cargo run -- vtxo <txid>:<vout> --operator http://localhost:7071
```

The binary is named `akcli`.

## Configuration

The operator base URL is resolved with this precedence:

1. `--operator <url>` flag
2. `operator` in `~/.akcli/config.toml`
3. otherwise, an error

Example `~/.akcli/config.toml`:

```toml
operator = "http://localhost:7071"
```

## Usage

Inspect the selected operator's information and policy:

```sh
akcli info
akcli info --operator http://localhost:7071
akcli --json info
```

`info` reads `/v1/info` and reports version, network, public signer, amount limits,
fees, and scheduling/delay values. Fees are shown verbatim, without evaluation or
assumed units; timing fields labeled `raw` have no inferred units. Missing values
are `-`; maximum `-1` means unlimited, and UTXO maximum `0` means boarding disabled.
These are operator-reported values, not independently verified. JSON serializes
SDK-known fields (including nested fees), not raw response bytes or unknown fields.
HTTP 404 is an error (exit 1), not a missing indexer record.

List indexer VTXOs for an Arkade address:

```sh
akcli address <ark1...|tark1...>
akcli --json address <address>
akcli address <address> --operator http://localhost:7071 --timeout 5
```

`address` validates a v0 Bech32m address locally, checks the operator's signer
and network family, then queries its P2TR lookup script without status filters.
The indexer accepts P2TR scripts as lookup keys; the address's OP_RETURN subdust
script is not an accepted query script. `tark` covers supported test networks,
not one specific network.
All pages are buffered before output; incomplete/invalid pagination, duplicate
outpoints, unusable outpoint identities, and HTTP errors fail with exit 1 and
no partial result. An empty list succeeds (exit 0).

The plain table shows abbreviated outpoints, integer satoshi amounts, status,
and expiry in UTC (with the unix value). Missing values are `-`; missing spent
state is `unknown`, and additional flags appear only when explicitly true.
Use `--json` for an object containing `address`, `scripts` (the single queried
P2TR script), and `vtxos`,
including full identifiers and all SDK-known fields. These are indexer records,
not a spendable balance or an atomic snapshot across pages.

Inspect a single VTXO by outpoint:

```sh
akcli vtxo <txid>:<vout>
akcli vtxo <txid>:<vout> --operator http://localhost:7071
akcli --json vtxo <txid>:<vout>
akcli --color never vtxo <txid>:<vout>
akcli --timeout 5 vtxo <txid>:<vout>
```

Inspect a commitment transaction (the on-chain batch) by txid:

```sh
akcli tx <commitment-txid>
akcli --json tx <commitment-txid>
```

The `commitment:` field shown by `akcli vtxo` is a commitment txid you can feed
straight into `akcli tx`. `tx` shows input/output amounts and VTXO counts, batch
timing, and per-batch expiry/swept state. Resolving virtual/ark transactions
(the `arkTxid`/`spentBy` identifiers) needs proof-of-ownership and transaction
decoding, and is not implemented yet.

Global flags: `--operator <url>`, `--json`, `--color always|never|auto`
(default `auto`: colors only on a TTY and honors `NO_COLOR`), and
`--timeout <seconds>` (default 10). Amounts are shown with thousands separators
and a BTC equivalent (except the address table, which uses sats only), and timestamps as UTC with the raw unix value in
parentheses.

Human-readable output keeps distinct Arkade identifiers clearly labeled. Per the
official SDK semantics, a spent VTXO is described at two layers: `spent by` is
the **checkpoint transaction** that spends it, and `ark txid` is the **Ark
transaction** that spends it (without the checkpoint) — both are empty for an
unspent VTXO. `commitment` lists the ancestor commitment transactions, and
`settled by` is the commitment transaction the VTXO was forfeited onto.
Indexer-reported state is **not** independent cryptographic proof, and the
output says so.

### Exit codes

| Code | Meaning |
| ---- | ------- |
| 0 | Requested information, record, or address list printed (including an empty list) |
| 1 | Error (bad input, missing operator, transport/HTTP failure) |
| 2 | No VTXO or commitment transaction found at the selected operator |

## Status

Implemented: operator information (`info`), address VTXO listing (`address`),
one VTXO (`vtxo`), and one commitment transaction (`tx`).
Virtual/ark transaction decoding and ancestry tracing are follow-on ideas,
not implemented yet.

Built on the official [Arkade Rust SDK](https://github.com/arkade-os/rust-sdk)
(`ark-rest` and `ark-core`, pinned to the same revision).

## License

MIT
