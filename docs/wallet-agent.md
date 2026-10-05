# Private wallet agent (W2 foundation)

The local agent retains **one** unlocked mnemonic in memory. It does not derive
keys, sign, access an operator, or return mnemonic material. Interactive wallet
commands and TOML configuration are W3 work, not implemented here.

## Interface and lease

`wallet::agent::Agent::new(absolute_home, options)` accepts an explicit home;
there is no HOME, explorer-config, or operator lookup. `Options::default()` uses
600 seconds. `Options::new(Duration)` accepts whole seconds from 1 through 86,400.

The async interface is `ensure_running`, `unlock(&WalletName, Secret)`, `status`,
`lock`, and `stop`. Call `ensure_running` before unlocking. It launches the current
executable, which must dispatch `agent::internal_entry()` before normal startup.
An existing agent keeps its original options; changing the lease requires restart.
`status` and `unlock` return only `Locked` or `Unlocked(public_wallet_name)`.
Passwords are nonempty UTF-8, at most 4096 bytes. W1's password semantics and
fixed Argon2id profile remain unchanged.

A successful unlock replaces the retained mnemonic and establishes a fresh
monotonic deadline. A failed unlock preserves the previous wallet **without
renewing its deadline**. Status does not renew it either. An independent active
maintenance future drops retained material at expiry, even without client calls
and while a blocking KDF runs. Lock and stop are global. Restart always begins
locked. No key-use operation exists yet.

## Process and filesystem lifecycle

The binary checks its hidden internal bootstrap before constructing the explorer
runtime or loading config. `ensure` is a noninteractive bootstrap launcher;
`daemon` is its child mode. Arguments carry only mode, absolute home, and lease
seconds. No password or mnemonic is accepted through arguments or environment.
The child uses the current executable, an empty environment, `/` as its working
directory, and null stdin/stdout/stderr. It calls safe `setsid` **after exec and
before creating Tokio threads**. There are no unsafe `pre_exec` hooks, TCP
listeners, system services, or shell launchers.

All runtime paths live beneath the explicit home:

| Path | Policy |
| --- | --- |
| `.akcli` | Owned directory; no group/world write; safe existing 0755 permitted |
| `.akcli/run` | Owned directory, exactly 0700 |
| `.akcli/run/agent.lock` | Owned regular single-link file, exactly 0600; never unlinked |
| `.akcli/run/agent.sock` | Owned Unix socket, exactly 0600, including at creation |

Managed opens/stat/unlink operations are descriptor-relative and nofollow.
Directories, lock files, and sockets are checked for owner, type, and mode.
Symlinks, FIFOs, wrong types, unsafe modes, relative homes, parent-directory
components, and socket paths of 104 bytes or more are rejected. The final home
component is opened nofollow, as in W1. Ancestor resolution and malicious
same-user directory replacement are not hardened beyond W1's trust model.
Unix socket bind/connect require pathname APIs, not an `*at` interface.

The daemon owns the exclusive advisory flock for its entire lifetime, including
shutdown's wait for a running KDF. Concurrent starters share the stable inode.
Exclusive creation followed by no-create opening of an existing lock also avoids
an observed macOS concurrent `O_CREAT | O_NOFOLLOW` open failure.

Only definite connection refusal or absence permits stale-socket cleanup, and
only under the exclusive lock. Successful connections (even incompatible ones),
protocol failures, authentication failures, permission errors, and timeouts do
not authorize removal. Shutdown checks device/inode identity before unlinking
its own socket; a replacement socket is preserved.

Readiness is bounded to five seconds after spawning; an initial live probe can
consume another two seconds. Startup children are reaped. Failure/cancellation
kills and reaps only the child spawned by that caller. A stale socket left by a
killed startup is deliberately left for the next exclusive lock holder, rather
than removed speculatively. A detached daemon survives its launcher's exit.

## Protocol and concurrency

Both peers verify `UnixStream::peer_cred().uid()` against effective UID. The client
checks **before constructing or sending the password frame**, and the server
checks before reading a request. Same-UID processes are within the trust domain;
this is not authentication against malware running as the wallet owner.

The private v1 protocol uses a four-byte big-endian payload length, then version
and opcode bytes. Status/lock/stop have no further payload. Unlock carries a
one-byte name length, validated ASCII name, two-byte password length, and UTF-8
password. Maximum payload size is 4133 bytes. Frames are allocated at their exact
validated size under `Zeroizing<Vec<u8>>` before receiving secret bytes and never
grow afterward. Password ownership moves into W1's `Secret`. Parsing errors own
no input; fixed error responses contain no secret or filesystem details.
Responses contain only version, a fixed result code, and optionally a validated
public wallet name. This is not JSON and is not a stable external protocol.

There is one request per connection, no pipelining. Payload length must match its
schema exactly. Trailing bytes already available are rejected; any further data
or EOF during unlock cancels that request. Bytes sent only after a completed
control response cannot retroactively invalidate that completed request.

Limits are 32 accepted request tasks, a two-second whole-frame read deadline,
a 25-second whole-server-request deadline, and a 30-second client budget. Slow
bytes do not reset deadlines. One blocking unlock is admitted at a time; excess
unlocks fail busy rather than queueing passwords. The semaphore permit belongs
to the **actual blocking worker**, not its cancellable request task.

The coordinator's generation invalidates results after lock/stop. A timed-out,
cancelled, or disconnected request cannot install its abandoned result. Stop
invalidates first, rejects further work, cancels request tasks, then waits for
the one possible worker to finish and drop its result before releasing the lock.
Control requests and expiry do not wait for the KDF.

## Security limits and verification

Zeroization is best effort for retained mnemonics, passwords, frames, decrypted
payloads, derived keys, and W1's Argon2 scratch allocation. Running Tokio blocking
work **cannot be cancelled**: lock, stop, timeout, or disconnect does not promise
instant erasure of worker-held passwords/scratch. Fixed KDF parameters bound
work and memory, not elapsed wall-clock time. There is no protection against
same-user malware, a compromised kernel, privileged inspection, kernel socket
copies, swap, core dumps, or all compiler/runtime copies.

Tests use only synthetic fixtures in short temporary homes. Coverage includes
fake-clock exact/active expiry, failed/replacement unlock semantics, deferred
results, actual-worker permit retention after cancellation/timeout, real KDF
control responsiveness, bounded/malformed/slow protocol input, UID policy,
private filesystem modes, symlinks/FIFOs, stale and occupied sockets, concurrent
starters, stable lock identity, detached launcher survival, config independence,
and stop/restart. Captured launcher pipes reaching EOF while the agent remains
alive exercise non-inherited stdio. Synthetic poisoned HOME/operator environment
values cannot redirect the explicit-home agent; empty child environment and `/`
cwd are also explicit spawn invariants, not an OS-level environment inspection.

macOS execution and real same-UID credentials are tested here. Linux execution,
actual cross-UID rejection, and non-macOS/non-Linux builds remain unverified;
other platforms receive an unsupported error from agent construction. The shared
UID predicate has an injected mismatch test; this is not a cross-user OS test.
W1 storage and existing explorer behavior remain separate and unchanged.
