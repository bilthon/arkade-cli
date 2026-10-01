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

Inspect a single VTXO by outpoint:

```sh
akcli vtxo <txid>:<vout>
akcli vtxo <txid>:<vout> --operator http://localhost:7071
akcli --json vtxo <txid>:<vout>
akcli --color never vtxo <txid>:<vout>
akcli --timeout 5 vtxo <txid>:<vout>
```

Global flags: `--operator <url>`, `--json`, `--color always|never|auto`
(default `auto`: colors only on a TTY and honors `NO_COLOR`), and
`--timeout <seconds>` (default 10). Amounts are shown with thousands separators
and a BTC equivalent, and timestamps as UTC with the raw unix value in
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
| 0 | VTXO found and printed |
| 1 | Error (bad input, missing operator, transport/HTTP failure) |
| 2 | No VTXO found for the outpoint at the selected operator |

## Status

First milestone: inspect one VTXO and show where it went. Full transaction
decoding, address-to-script conversion, and ancestry tracing are follow-on
ideas, not implemented yet.

Built on the official [Arkade Rust SDK](https://github.com/arkade-os/rust-sdk)
(`ark-rest`).

## License

MIT
