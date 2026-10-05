# Encrypted mnemonic store (library only)

W1 provides a local encrypted mnemonic store, not a usable wallet CLI. There is no
agent, wallet command, seed derivation, address generation, or funds support yet.
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
existing CLI is unchanged. Other Unix platforms are not a tested support commitment.

## Storage and encryption

Files live at `<absolute-home>/.akcli/wallets/<name>.json`, with a 16 KiB read bound.
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
is accepted. `wallets` must be owned and 0700; wallet files must be owned regular files
with exactly 0600 permissions. Unsafe existing objects are rejected, never chmodded.
Nonblocking file opens avoid hanging on special objects before type validation.

Creation writes **only encrypted JSON** to an exclusive 0600 same-directory random
`.pending-` file. It syncs the file, publishes with an atomic no-overwrite hard link,
then removes the temporary name and syncs the directory. Duplicate and racing creates
cannot replace an existing destination. An error after publication may leave a valid
wallet visible; callers should inspect rather than assume no file was created.
A crash may leave an encrypted `.pending-` file, which listing ignores; no automatic
cleanup or plaintext recovery is attempted. Filesystems must support ordinary Unix
permissions, hard links, and directory fsync; network filesystems are not validated.

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
