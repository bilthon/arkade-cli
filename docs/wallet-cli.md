# Local encrypted wallets

`akcli wallet` stores named encrypted English BIP39 mnemonics and keeps at most
one unlocked mnemonic in a private local agent. It does **not** derive keys,
query funds, sign, produce addresses, or integrate an operational SDK wallet.
No wallet command contacts an operator.

## Commands

| Command | Effect |
| --- | --- |
| `akcli wallet create <name> [--words 12\|24]` | Generate 12 English words by default (optionally 24) from OS entropy, encrypt, then explicitly display on the controlling terminal |
| `akcli wallet import <name>` | Validate and encrypt an existing English phrase, without redisplaying it |
| `akcli wallet list` | List validated public wallet names without decryption |
| `akcli wallet unlock <name>` | Prompt for a password, start the agent if absent, and replace its retained wallet on success |
| `akcli wallet status` | Show running/stopped and locked/unlocked status, plus the public unlocked name |
| `akcli wallet lock` | Forget the agent's retained mnemonic, without stopping the agent |
| `akcli wallet agent stop` | Stop the agent and forget retained material |

Names must match `[a-z0-9][a-z0-9_-]{0,31}`; names are rejected, not normalized.
Files live at `$HOME/.akcli/wallets/<name>.json`. An absolute `HOME` is required;
there is no current-directory fallback. Existing names are never overwritten.

Create/import/unlock require `/dev/tty`; piped stdin is not a secret-input
interface. Name and terminal validation precede secret entry and publication or
agent startup. Global flags work before or after subcommands. Wallet commands
ignore `--operator`, `--color`, and the explorer HTTP `--timeout` setting.
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
file is retained. The CLI never deletes a published wallet to undo cancellation.
It does not automatically unlock it or offer a stored-phrase export/reveal command.

The phrase is intentionally written only to the controlling terminal, including
when stdout is redirected or `--json` is used. Keep that terminal private: scrollback,
terminal recording, screenshots and observers can retain what was displayed.

**The file password is not a BIP39 passphrase.** No seed derivation occurs here;
a future operational wallet must decide passphrase semantics explicitly. A
mnemonic alone is not a guaranteed complete Arkade recovery backup: contract,
key-index, swap and other state may also matter.

## Import and unlock

Import accepts valid 12-, 15-, 18-, 21-, or 24-word English BIP39 phrases with a
valid checksum. Input is hidden and bounded to 512 UTF-8 bytes. The validated
phrase is canonicalized, encrypted with a confirmed file password, and never
redisplayed. Import does not unlock or launch the agent.

Unlock prompts only for the file password. Successful unlock replaces the one
previously retained wallet. A wrong password or failed unlock preserves the
previous state without extending its expiry. Status and list also never renew
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
operator fields are ignored by this loader. Other wallet commands do not load
configuration, so a malformed explorer configuration does not prevent locking.
Invalid wallet settings fail unlock with a sanitized diagnostic.

An already running agent keeps its original lease, even if unlock reads changed
settings. To apply a change, run `akcli wallet agent stop`, wait for shutdown, then
unlock again. There is no automatic restart. Restarted agents begin locked.

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
agent calls, replacement/failure, expiry, locking, stopping and restart. Explicit
fixture daemons are stopped, waited and reaped, including failure paths.

Execution was exercised on macOS. Linux execution, cross-UID tests and unsupported
platform builds remain separate verification work. Most PTY lifecycle tests start
owned daemon children to guarantee cleanup. A separate interactive test starts
with no daemon, unlocks through a real PTY, and verifies retention after that CLI
exits using separate status, lock and stop commands. Its bounded cleanup verifies
socket removal and lifetime-lock release, not OS reaping of the detached daemon;
failure-path cleanup for this guard has not been fault-injected.
