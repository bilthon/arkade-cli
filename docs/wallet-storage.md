# Wallet storage

The encrypted mnemonic store backs the [local wallet CLI](wallet-cli.md) and agent.
The private agent receive operation (R2) derives an address and persists its public
contract through the pinned SDK. There is no public receive CLI yet (R3), and no
balance, send, sync, swap, or restore support.
An encrypted mnemonic backup is **not a guarantee of complete wallet recovery**:
contract, key-index, and swap state may also be required by future integrations.

## Interface

On macOS and Linux, use `arkade_cli::wallet`:

- `WalletName::new(&str)` validates ASCII `[a-z0-9][a-z0-9_-]{0,31}` without normalization.
- `Secret::new(String)` takes ownership of secret text. It has no `Debug` or serialization
  implementation; `expose()` deliberately borrows the plaintext. Avoid caller-side copies.
- `WalletStore::open(&Path)` requires an absolute, trusted home directory and opens or
  creates `.akcli/wallets` within it. No environment lookup or relative fallback occurs.
- `create(&WalletName, mnemonic: Secret, password: Secret)` validates and encrypts an
  imported English BIP39 mnemonic. All valid BIP39 lengths are accepted. ASCII whitespace
  is canonicalized to single spaces; non-ASCII input and invalid checksums are rejected.
- `list()` returns sorted public `WalletName` values after bounded structural validation,
  without decrypting. It cannot establish ciphertext authenticity without a password.
- `unlock(&WalletName, password: Secret)` returns an owned `Secret` containing the canonical
  mnemonic. Every unlock failure has the same safe message, without input or parser details.

Passwords must be nonempty; there is no additional policy. A file password is **not**
a BIP39 passphrase. This interface never derives a seed. Store operations are synchronous;
callers must bound concurrent KDF work. The store is absent from non-Unix builds; the
existing wallet CLI behavior is unchanged. Other Unix platforms are not a tested support commitment.

## Storage and encryption

Files live at `<absolute-home>/.akcli/wallets/<name>/<name>.json`, with a 16 KiB read bound.
Listing validates each named directory and its expected JSON, returning sorted complete
wallets. Safe directories missing that JSON are skipped; unsafe objects, unreadable files,
and malformed or name-mismatched envelopes are errors. Reads never create wallet children.
Unrelated colocated files (such as future `contracts.db` or `metadata.json`) are ignored;
creation does not produce placeholders for them.

Legacy top-level `*.json` entries are ignored, never opened, unlocked, migrated, moved,
or deleted. A new same-name directory may coexist with an untouched legacy flat file.
There is no migration or backward compatibility.
The JSON object has exactly `version`, `name`, `profile`, `salt`, `nonce`, and `ciphertext`.
Duplicate/unknown fields, unsupported versions/profiles, filename/name mismatch, invalid
base64, and invalid decoded lengths are rejected before running the KDF. No arbitrary
KDF parameter fields are accepted.

| Property | v1 value |
| --- | --- |
| Profile | `argon2id-v19-m65536-t3-p1-xchacha20poly1305` |
| KDF | Argon2id v19, 65536 KiB, 3 iterations, parallelism 1, 32-byte key |
| Salt / nonce | OS-random 16 bytes / 24 bytes, standard padded base64 |
| Encryption | XChaCha20Poly1305, ciphertext includes its authentication tag |
| AAD | JSON tuple `["akcli-wallet", version, name, profile]` |
| Plaintext | Canonical English BIP39 mnemonic only |

The same profile is used in tests; no weakened test KDF exists. Salt/nonce/ciphertext
changes fail authentication; renaming a file and its metadata cannot rebind the ciphertext.
Names, sizes, timestamps, and the number of wallets remain public metadata.

## Filesystem guarantees and limits

Managed path operations are descriptor-relative using `rustix`, with no-follow opens.
The supplied home is a trusted anchor (its ancestors are not a sandbox). `.akcli` must
be owned by the effective user and not group/world writable; an existing 0755 directory
is accepted. `wallets` and each wallet child must be owned and 0700;
wallet files must be owned regular files
with exactly 0600 permissions. Unsafe existing objects are rejected, never chmodded.
Nonblocking file opens avoid hanging on special objects before type validation.

Creation encrypts before filesystem publication, exclusively creates the named 0700
child, then syncs the `wallets` parent. Any existing object at that name blocks creation,
including an empty or incomplete directory; it is never reused, repaired, or removed.
Inside the child, creation writes **only encrypted JSON** to an exclusive 0600 random
`.pending-` file. It syncs the file, publishes with an atomic no-overwrite hard link,
syncs the child, removes the temporary name, and syncs the child again. Duplicate and
racing creates cannot replace an existing destination.

An error after child creation can leave an incomplete directory that reserves the name;
listing skips it only if it is safe and its expected JSON is genuinely absent. An error
after publication may leave a valid wallet visible; callers should inspect rather than
assume nothing was created. A crash may leave an encrypted `.pending-` file. Listing
ignores unrelated child files and legacy top-level `.pending-` entries. No recursive
cleanup, automatic directory removal/reuse, or plaintext recovery is attempted. Filesystems must support ordinary Unix
permissions, hard links, and directory fsync; network filesystems are not validated.

## Receive storage (R1/R2)

`ReceiveStore::open_existing(home, name)` retains the validated wallet-directory
descriptor and exposes `database_path()` for SDK integration. Opening never
creates missing roots or wallet children. `check_binding(&ReceiveSettings)` reads
without writes. `initialize(&ReceiveSettings)` rechecks, then writes only receive
state; callers must validate the operator first. Keep the store alive for the
SDK database lifetime. The agent retains it while `SqliteContractStore` and
`ContractManager` are alive. `wallet receive` calls this through the agent.

`metadata.json` is strict JSON, bounded to 16 KiB, with exactly these fields:

| Field | Value |
| --- | --- |
| `version` | `2` |
| `environment` | SDK-validated raw REST environment, including distinct `signet`/`mutinynet` |
| `operator_rest_url` | Normalized configured HTTP/HTTPS authority |
| `derivation_profile` | `sdk65b383e-default-empty-passphrase-index0` |

Unknown/duplicate fields, unsupported versions/profiles, and conflicting bindings
are rejected without changes, including legacy version-1 gRPC bindings. There is
no automatic migration or reset. The environment and typed SDK info come from
one REST response; binding validation precedes derivation and initialization.
There is no redundant wallet name, custom derivation,
or signer binding. The profile fixes the SDK's default `m/83696968'/11811'/0`
derivation with normal owner index 0 and empty BIP39 passphrase. The encryption
password is unrelated. Operator signer/policy changes may still change an address.
The SDK network family alone cannot distinguish Mutinynet from Signet.

Metadata uses the existing synced, atomic no-overwrite publication helper.
Only afterward is `contracts.db` exclusively precreated as an empty 0600 file,
synced along with its parent. Both state files are unencrypted public state;
private permissions still apply. Existing metadata, DB, `contracts.db-journal`,
`contracts.db-wal`, and `contracts.db-shm` must be owned regular 0600 files;
symlinks, special objects and unsafe modes are rejected, never repaired.
Database/auxiliary files without metadata are not adopted; auxiliaries without
a database are also rejected. Matching metadata without a DB is a retryable
partial initialization. Existing state and mnemonic envelopes are never rewritten
or automatically cleaned up. Publication/sync errors can leave persistent files.

The agent validates operator network and session before initialization, rechecks
all SQLite paths immediately before and after SDK open, then inserts the default
VTXO contract with `Active` state and `Some(0)` key index. Only a successful
`insert_or_get` permits address construction. It revalidates storage and session
before responding. Errors can leave initialized files, but never return an address
without successful persistence. The encrypted mnemonic envelope is untouched.

Managed operations remain descriptor-relative, but the SDK pathname open is
**not** race-immune against hostile same-user mutation. The daemon sets umask 0077.
Tests exercise real SQLite persistence/reopen, WAL/SHM owner-only modes and unsafe
auxiliary rejection, failed inserts, and no-write operator/binding mismatches,
in addition to R1's bounded parsing, partial retries, no-clobber races, unsafe
files/directories, and descriptor retention across a directory rename.
Power-loss durability, cross-UID fixtures, and physical erasure remain unproven.

## Secret handling is best effort

Owned passwords, canonical mnemonics, derived keys, plaintext byte buffers, and Argon2
scratch memory use zeroizing storage. The BIP39 dependency enables zeroization. Buffer
capacity is reserved before building canonical plaintext. No secret type implements
`Debug`, and errors do not include inputs. This is not physical-erasure proof: compiler,
allocator, dependency internals, caller copies, registers, swap, crash dumps, and
compromised same-user processes remain outside the guarantee. This does not use memory
locking or an OS keyring and cannot defend against a malicious process running as the
same user or privileged filesystem mutation.

Tests use temporary homes and synthetic mnemonics only. They cover authenticated
round trips, all English word counts, safe errors, envelope bounds, malformed fields,
renaming, permissions, symlinks, special objects, duplicates, and racing publication.
Cross-UID ownership rejection is implemented but requires a privileged fixture to
exercise directly; power-loss durability and physical memory erasure are not proven
by these tests.
