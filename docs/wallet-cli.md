# Local encrypted wallets

`akcli wallet` stores named encrypted English BIP39 mnemonics and keeps at most
one unlocked mnemonic in a private local agent. `wallet receive` asks that agent
to derive and persist a receive address through the pinned SDK. Only receive
contacts the configured REST operator. No balance, send, on-chain wallet, sync,
or complete recovery operation is provided.

## Commands

| Command | Effect |
| --- | --- |
| `akcli wallet create <name> [--words 12\|24]` | Generate 12 English words by default (optionally 24) from OS entropy, encrypt, then explicitly display on the controlling terminal |
| `akcli wallet import <name>` | Validate and encrypt an existing English phrase, without redisplaying it |
| `akcli wallet list` | List validated public wallet names without decryption |
| `akcli wallet unlock <name>` | Prompt for a password, start the agent if absent, and replace its retained wallet on success |
| `akcli wallet status` | Show running/stopped and locked/unlocked status, plus the public unlocked name |
| `akcli wallet receive` | Show a receive address for the currently unlocked wallet; reuse index 0, not rotation |
| `akcli wallet lock` | Forget the agent's retained mnemonic, without stopping the agent |
| `akcli wallet agent stop` | Stop the agent and forget retained material |

Names must match `[a-z0-9][a-z0-9_-]{0,31}`; names are rejected, not normalized.
Files live at `$HOME/.akcli/wallets/<name>/<name>.json`. An absolute `HOME` is required;
there is no current-directory fallback. Existing named objects, including empty or
incomplete directories, block creation without being modified. Legacy flat `*.json`
files are ignored, not unlocked or migrated; a new same-name directory can coexist
with an untouched legacy file. Create/import do not create SDK database or metadata
placeholders; receive initializes public state only after validation.

Create/import/unlock require `/dev/tty`; piped stdin is not a secret-input
interface. Name and terminal validation precede secret entry and publication or
agent startup. Global flags work before or after subcommands. Wallet commands
ignore `--color` and the explorer HTTP `--timeout` setting. Receive rejects
`--operator`: set the persistent top-level `operator` instead. Other wallet commands
continue to ignore `--operator`.
`--json` affects only the public result, not interactive terminal prompts.

## Create and back up

`akcli wallet create personal` defaults to 12 words. Use
`akcli wallet create personal --words 24` for 24 words. `--words 12` is also
accepted; the option works before or after the name and is create-only.

1. Choose a file password and confirm it. It must be nonempty UTF-8, at most
   4096 bytes (not characters).
2. Read the retention warning and type `yes` to permit terminal display.
3. The CLI generates 16 bytes of OS entropy for 12 words, or 32 bytes for 24 words,
   and saves the encrypted wallet using the no-clobber store **before displaying**
   the resulting English BIP39 phrase.
4. Record the phrase privately and type `yes` to acknowledge your backup.

Cancelling at a prompt before saving leaves no new wallet file. Once saving
starts, cancellation is deferred until it finishes. **After publication the file
is retained**, even if display fails or backup acknowledgement is cancelled.
A failed filesystem sync can leave a published file; errors warn that any such
file is retained. A storage failure can also leave an incomplete named directory;
it remains reserved and cannot be retried under the same name. List skips safe
incomplete directories, but reports unsafe objects or malformed expected JSON.
There is no automatic cleanup or reuse. The CLI never deletes a published wallet
to undo cancellation.
It does not automatically unlock it or offer a stored-phrase export/reveal command.

The phrase is intentionally written only to the controlling terminal, including
when stdout is redirected or `--json` is used. Keep that terminal private: scrollback,
terminal recording, screenshots and observers can retain what was displayed.

**The file password is not a BIP39 passphrase.** Receive uses an empty BIP39
passphrase independently of this encryption password. A mnemonic alone is not a guaranteed complete Arkade recovery backup: contract,
key-index, swap and other state may also matter.

## Import and unlock

Import accepts valid 12-, 15-, 18-, 21-, or 24-word English BIP39 phrases with a
valid checksum. Input is hidden and bounded to 512 UTF-8 bytes. The validated
phrase is canonicalized, encrypted with a confirmed file password, and never
redisplayed. Import does not unlock or launch the agent.

Unlock prompts only for the file password. Successful unlock replaces the one
previously retained wallet. A wrong password or failed unlock preserves the
previous state without extending its expiry. Status, list, and receive never renew
expiry. Explicit lock and stop apply globally, not to a named wallet.

Status on a proven absent agent returns stopped/locked. Lock and stop then succeed
idempotently. Unsafe permissions, symlinks, wrong file types, authentication errors,
protocol failures and timeouts remain errors. These commands never autostart and
never create runtime directories just to discover absence. An accepted stop asks
the server to shut down; a currently running KDF can delay final process exit.

## Idle timeout

Only unlock loads wallet settings from `$HOME/.akcli/config.toml`:

```toml
[wallet]
idle_timeout_seconds = 600
```

The value must be an integer from 1 to 86400; the default is 600 seconds. Unrelated
operator fields are ignored by this loader. Receive separately loads its own
settings in the daemon; other wallet commands do not load configuration, so a
malformed explorer configuration does not prevent locking.
Invalid wallet settings fail unlock with a sanitized diagnostic.

An already running agent keeps its original lease, even if unlock reads changed
settings. To apply a change, run `akcli wallet agent stop`, wait for shutdown, then
unlock again. There is no automatic restart. Restarted agents begin locked.

## Receive an address

Set the existing top-level REST `operator` in `$HOME/.akcli/config.toml`
(before any TOML table headers). For a local operator, for example:

```toml
operator = "http://localhost:7071"
```

No `[wallet.receive]`, environment, or gRPC endpoint is required. On first
initialization, `GET /v1/info` establishes the network; no network is assumed.

Unlock an existing wallet, then request its address:

```sh
akcli wallet unlock personal
akcli wallet receive
# From a source checkout, after unlocking:
cargo run -- wallet receive
```

Success prints `Address: <exact SDK address>`; `--json` returns
`{"address":"<exact SDK address>"}`. Receive takes no name, amount, rotation,
passphrase, or password argument. It never prompts, starts an agent, automatically
unlocks, or renews the lease. A missing/locked agent fails without an address or
implicit wallet creation. Invalid configuration, remote network mismatch, and
persistence failures also return no address.

An already unlocked older daemon may not support receive. On a protocol error,
manually run `akcli wallet agent stop`, wait for shutdown, then unlock again with
the current binary. There is no hidden restart that discards an unlocked session.

The operator URL is an HTTP/HTTPS authority (at most 2048 bytes), optionally
ending in `/`, without credentials, query, fragment, controls, or other path.
Normalization removes the root slash and normalizes host/default port. The daemon
reloads the file from its absolute home for each request, with a 64 KiB bound.
The REST request has a 10-second timeout within the 25-second server budget.
Missing operator, invalid config, connection/timeout, HTTP failure, invalid operator
info, and unsafe/incompatible storage produce static diagnostics without supplied
values, remote bodies, or URLs. Other wallet settings and explorer flag precedence
are unchanged; no per-request URL is sent over IPC.

The fixed receive profile uses the pinned SDK's default derivation with an empty
BIP39 passphrase and owner index 0 (reuse, not rotation). The validated raw
environment from that same REST response remains bound in metadata, preserving
Mutinynet versus Signet even though typed SDK info collapses them. Repeated requests and restarts reuse index 0; the address can change if
the operator's signer or policy changes. Neither signer pinning nor complete
imported-wallet recovery is promised.

The daemon writes unencrypted public `metadata.json` and `contracts.db` beside
the encrypted wallet file, with owner-only permissions. Metadata binds the
operator-reported environment, normalized REST URL, and derivation profile; the database
holds SDK contract state. The contract is persisted before an address is returned.
Version-1 bindings, changed URLs/environments, and malformed or unsafe storage
are rejected without migration or rewriting. Do not treat the mnemonic alone as a full recovery backup. See
[receive storage](wallet-storage.md#receive-storage-preparation-r1).

## Output and cancellation

Human output uses plain public labels. JSON contains only public data:

```json
{"agent":"stopped","status":"locked","name":null}
```

List returns `{"names":[]}` or an array of public names. Create/import/unlock
return `result` and `name`. No response contains a password or mnemonic.
Help and parse errors use standard Clap output. Parse failures exit with code 2
and empty stdout; diagnostics can echo invalid arguments. Secret-input flags are
not supported: enter passwords and mnemonics only at the protected terminal
prompts, never through argv, environment, stdin redirection or JSON.

Terminal input is noncanonical and no-echo, with fixed-capacity zeroizing buffers.
Backspace erases removed bytes. Overflow, invalid UTF-8, EOF, Ctrl-C and external
SIGINT cancel input without printing its contents. Normal returns and errors
flush unread input before restoring original terminal settings. Tokio handles
external SIGINT; there is no custom unsafe signal handler. SIGKILL, abort, machine
failure or a disconnected terminal can prevent restoration; use your terminal's
reset facility if necessary. Saving/KDF work is not instantly abortable.

Zeroization is best effort, not protection against privileged inspection,
same-user malware, swap, core dumps, terminal/kernel copies or all compiler copies.
See [storage](wallet-storage.md) and [agent](wallet-agent.md) for their contracts.

## Verification boundaries

Tests use synthetic temporary homes and real controlling PTY sessions, capturing
stdout/stderr separately without printing secret transcripts on assertion failure.
They cover default/explicit 12-word and explicit 24-word generation with English
checksums and private display, standard help and parse errors, hidden import/password
confirmation, all valid import phrase lengths, bounded
UTF-8 input/editing, duplicate publication, creation acknowledgement/retention,
cancellation, SIGINT and terminal restoration, no-TTY rejection, independent CLI
agent calls, replacement/failure, expiry, locking, stopping and restart. Receive
adds local-REST/SDK binary tests for labeled and JSON output, repeat/restart reuse,
locked/missing agents, invalid configuration, remote network mismatch, and
malformed/incompatible response rejection without automatic restart. Explicit
fixture daemons are stopped, waited and reaped, including failure paths.

Execution was exercised on macOS. Linux execution, cross-UID tests and unsupported
platform builds remain separate verification work. Most PTY lifecycle tests start
owned daemon children to guarantee cleanup. A separate interactive test starts
with no daemon, unlocks through a real PTY, and verifies retention after that CLI
exits using separate status, lock and stop commands. Its bounded cleanup verifies
socket removal and lifetime-lock release, not OS reaping of the detached daemon;
failure-path cleanup for this guard has not been fault-injected.
