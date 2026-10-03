# Changelog

## v0.1.0 — Initial release

Source-only release of `arkade-cli`, providing the `akcli` read-only explorer.
No precompiled binaries or crates.io package are provided.

### Features

- `info`: inspect operator information and policy.
- `address`: list indexer VTXOs for an Arkade address.
- `script`: decode script bytecode locally and look up VTXOs for P2TR scripts.
- `vtxo`: inspect one VTXO by outpoint.
- `tx`: inspect a commitment transaction only, not an arbitrary transaction.
- `virtual-tx`: inspect an operator-provided virtual transaction PSBT.
- Human-readable reports and machine-readable JSON (`--json`).
- Operator configuration in `~/.akcli/config.toml`, a `--operator` override,
  request timeouts (`--timeout`, default 10 seconds), and color controls
  (`--color auto|always|never`, with `NO_COLOR` support).

### Scope and limitations

- Read-only: no wallet, private-key management, signing, or sending transactions.
- Structural decoding is not script execution or signature verification.
  Operator-reported records are not independent proof of ownership or
  spendability.
- Operator-backed queries require compatible APIs and response formats. Tested
  with arkd v0.9.4; compatibility with every operator or version is not guaranteed.
- Access depends on operator policy: anonymous queries may be restricted and
  virtual transaction PSBTs may omit signatures. Authentication integration is
  not implemented.
