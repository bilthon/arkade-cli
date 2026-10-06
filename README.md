# arkade-cli (`akcli`)

A small, read-only explorer for Arkade virtual transactions and outputs — think
of the exploratory convenience of `bitcoin-cli`, but for Arkade VTXOs.

Explorer commands are **read-only**. Local `wallet` commands additionally store
encrypted BIP39 mnemonics and manage a private, on-demand memory agent. Receive
addresses and operator-reported offchain balance are available through the pinned
SDK; send, on-chain wallet, sync, and complete recovery are not implemented.

## Install

v0.1.0 is a source-only release: no precompiled binaries or crates.io package.
Prerequisites: a recent Rust toolchain with Cargo, and Git. Tested with Rust
1.95.0; this is not a declared minimum supported Rust version (MSRV).

Once the v0.1.0 release is published, install the tagged source:

```sh
cargo install --git https://github.com/bilthon/arkade-cli --tag v0.1.0 --locked arkade-cli
```

Alternatively, install from the root of a local source checkout:

```sh
cargo install --locked --path .
```

The package is named `arkade-cli`; the installed binary is `akcli`.
Ensure Cargo's bin directory is on your `PATH`.

## Configuration

For explorer commands, the operator base URL is resolved with this precedence:

1. `--operator <url>` flag
2. `operator` in `~/.akcli/config.toml`
3. otherwise, an error

Example `~/.akcli/config.toml`:

```toml
operator = "http://localhost:7071"
```

## Local encrypted wallets

```sh
akcli wallet create personal            # 12 English words (default)
akcli wallet create long-backup --words 24
akcli wallet import existing
akcli wallet list
akcli wallet unlock personal
akcli wallet status
akcli wallet receive                    # requires receive configuration below
akcli wallet balance                    # unlocked session, initialized receive state
akcli wallet lock
akcli wallet agent stop
```

Create, import, and unlock require a controlling terminal; passwords and imported
phrases are hidden. Never put secrets in arguments or environment variables.
Creation defaults to 12 English words; use `--words 24` for 24 words (creation
only). It saves the encrypted file **before** explicitly displaying the new
phrase on the terminal. Cancelling after publication retains that file,
even without backup acknowledgement. No command exports a stored phrase.

Wallet commands require an absolute `HOME`. Receive and balance use the existing
top-level REST `operator` configuration; other wallet commands do not require it.
`--json` contains only public results, never passwords or phrases.
The file password is **not** a BIP39 passphrase, and a mnemonic alone is not a
guaranteed complete recovery backup. See [wallet usage and limits](docs/wallet-cli.md).

Optional configuration in `~/.akcli/config.toml`:

```toml
[wallet]
idle_timeout_seconds = 600
```

Only unlock reads these settings (integer 1–86400, default 600). An existing agent
keeps its original timeout: explicitly stop it, then unlock to apply changes.

Receive uses `operator` from the config example above, before any TOML table
headers. No `[wallet.receive]`, environment, or gRPC endpoint is required.
The first validated `GET /v1/info` response establishes the network; metadata v2
binds its environment and normalized REST URL. Changes and legacy v1 bindings
fail closed without migration. Receive rejects `--operator`; edit persistent
configuration instead. Errors are sanitized and distinguish configuration,
connection/timeout, HTTP, invalid info, and storage binding failures.

After unlocking, run
`akcli wallet receive` (or `cargo run -- wallet receive` from a source checkout).
It prints `Address: <address>`, or `{"address":"<address>"}` with `--json`.
There are no receive arguments or password prompts, automatic unlock/start, or
lease renewal. Older daemons may require manual `akcli wallet agent stop` and
re-unlock with the current binary; receive never silently restarts them.

Receive uses an **empty BIP39 passphrase**, not the file-encryption password, and
reuses SDK owner index 0 across requests/restarts. An operator signer/policy change
can change the address; this is not rotation. Public `contracts.db` and
`metadata.json` are persisted beside the encrypted wallet with owner-only
permissions; they are not encrypted. A mnemonic alone is not a full recovery backup.

### Offchain balance

`akcli wallet balance` takes no parameters and requires a currently unlocked agent.
It queries all persisted default receive contracts, including historical signer and
policy variants, without deriving addresses, inserting contracts, discovering funds,
syncing, or moving funds. Missing/partial receive state is an error: initialize it
explicitly with `wallet receive`; a valid initialized store with no matching VTXOs
reports zero. Balance rejects `--operator`; the daemon reloads the persisted
REST configuration per request. After upgrading an old daemon binary, stop it and
unlock again; balance never autostarts or restarts it or renews its lease.

The headline is `Offchain balance (operator-reported)`, with Confirmed,
Preconfirmed, Recoverable and Pending recovery buckets in sats and an availability
warning. These SDK classifications are not Bitcoin confirmations, proof of funds,
immediate spendability, or guaranteed/complete recovery. No onchain/assets are
included. Pagination is not an atomic snapshot; expiry depends on the local clock.
SQLite opening is initialization-capable, not strictly read-only storage.
`--json` returns exactly five decimal-string values:

```json
{"offchain_balance_sats":"100","confirmed_sats":"10","preconfirmed_sats":"20","recoverable_sats":"30","pending_recovery_sats":"40"}
```

Failures print a sanitized diagnostic to stderr, with no partial totals on stdout.
See [balance details](docs/wallet-cli.md#read-offchain-balance).

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

Decode script bytecode and look up P2TR VTXOs:

```sh
akcli script 512025a43cecfa0e1b1a4f72d64ad15f4cfa7a84d0723e8511c969aa543638ea9967
akcli --json script 6A
akcli script ''
```

`script` reports canonical lowercase hex, standard script type (or `unknown`),
ASM, optional witness version, and the full P2TR output key bytes. Uppercase hex,
empty bytecode and nonminimal pushes are accepted; malformed hex and truncated
push instructions fail before configuration loading, HTTP or result output.
This is structural decoding, **not** script execution, signature verification,
curve-key validation, ownership or spendability proof. No internal key, script
tree or network is inferred.

Only P2TR scripts trigger lookup: one canonical script filter, no status filter,
no `/v1/info` identity check. All pages are buffered using the same checks and
status table as `address`; failures produce no partial result. Non-P2TR scripts
succeed locally without loading configuration or making HTTP requests, even
when operator configuration is absent or malformed. No `--decode-only` flag is
needed or supported.

JSON contains `script` (`hex`, `type`, `asm`, `witness_version`,
`p2tr_output_key`), `lookup_status`, and `vtxos`, retaining full identifiers and
SDK-known records. Unsupported lookup is `"lookup_status": "unsupported"` with
`"vtxos": null`, not an empty result. Completed P2TR lookup is
`"lookup_status": "complete"` with a VTXO array (possibly `[]`). Both succeed
with exit 0; the human report explicitly distinguishes unsupported from empty.

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
timing, and per-batch expiry/swept state. `tx` remains commitment-only.

Inspect a virtual transaction (for example an `arkTxid` or `spentBy` identifier):

```sh
akcli virtual-tx <txid>
akcli --json virtual-tx <txid>
akcli virtual-tx <txid> --operator http://localhost:7071 --timeout 5
```

`virtual-tx` validates a 64-character lowercase hex ID before HTTP and makes one
anonymous request for the operator-provided PSBT. It checks the unsigned
transaction's computed ID, then displays version, consensus locktime, counts,
output total, and INPUTS/OUTPUTS tables with full outpoints and script hex.
Output table amounts are integer sats. No fields are colored, even with
`--color always`. JSON contains the same decoded fields with integer satoshi
amounts and a `psbt` field retaining the exact original base64 PSBT.

Access depends on operator policy: some operators allow anonymous lookup,
others may omit signatures or restrict access. No authentication integration
is implemented yet. Signatures and spendability are **not verified**; decoding
is not proof of executability, and no input amounts or fees are inferred.
An explicit HTTP 200 `txs: []` means no record (exit 2, human message or JSON
`null`). HTTP 404, like 403 and other HTTP failures, is an error (exit 1), unlike
`tx`'s commitment-404 handling. Missing/null `txs`, multiple records (including
duplicates), malformed PSBTs, mismatched IDs, and output-total overflow are
errors with no partial result.

Global flags: `--operator <url>`, `--json`, `--color always|never|auto`
(default `auto`: colors only on a TTY and honors `NO_COLOR`), and
`--timeout <seconds>` (default 10). Amounts are shown with thousands separators
and a BTC equivalent (except address, script and virtual transaction output tables, which use sats only), and timestamps as UTC with the raw unix value in
parentheses. `--color always` overrides `NO_COLOR`; `--color never` disables color.
JSON output is never colored.

In `address`, `script` and `vtxo` human views, explicit `spent` makes the entire status
text red, including all flags and separators, but never padding, labels, or
other columns. Otherwise, `unspent` is green; when also `preconfirmed`, both
words are blue, and `swept` is independently red. Missing spent state is
`unknown`; without the spent override, `unknown`, `unrolled`, and `preconfirmed`
without explicit unspent stay neutral, as do separators. All true flags remain
visible, including contradictory combinations.

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
| 0 | Successful command, including wallet operations, empty lists and unsupported local script lookup |
| 1 | Error (bad input, missing operator, transport/HTTP failure) |
| 2 | Command-line syntax error, or no VTXO, commitment transaction, or virtual transaction found at the selected operator |

## Status

Implemented: operator information (`info`), address VTXO listing (`address`),
one VTXO (`vtxo`), one commitment transaction (`tx`), and virtual transaction
PSBT inspection (`virtual-tx`), and script inspection (`script`). Ancestry tracing
is not implemented yet. Encrypted named mnemonic storage and interactive local
wallet-agent commands and receive addresses are also available on macOS/Linux;
they provide operator-reported offchain balance, but not spending or complete recovery.

Built on the official [Arkade Rust SDK](https://github.com/arkade-os/rust-sdk)
(`ark-rest` and `ark-core`, pinned to the same revision).

## License

MIT
