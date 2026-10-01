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
```

Human-readable output keeps distinct Arkade identifiers clearly labeled:
`spent by` (the spending/checkpoint transaction reported by the operator) is
shown separately from `ark txid` (the Arkade transaction). Indexer-reported
state is **not** independent cryptographic proof, and the output says so.

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
