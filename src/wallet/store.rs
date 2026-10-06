use super::{
    fs::{Directory, LIMIT},
    Error, Secret, WalletName,
};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use zeroize::Zeroizing;

const PROFILE: &str = "argon2id-v19-m65536-t3-p1-xchacha20poly1305";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    name: String,
    profile: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}
struct Decoded {
    salt: [u8; 16],
    nonce: [u8; 24],
    ciphertext: Vec<u8>,
}
impl Envelope {
    fn decode(bytes: &[u8], name: &WalletName) -> Result<(Self, Decoded), Error> {
        if bytes.len() > LIMIT {
            return Err(Error::InvalidEnvelope);
        }
        let e: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidEnvelope)?;
        if e.version != 1 || e.profile != PROFILE || e.name != name.as_str() {
            return Err(Error::InvalidEnvelope);
        }
        let salt = STANDARD
            .decode(&e.salt)
            .map_err(|_| Error::InvalidEnvelope)?
            .try_into()
            .map_err(|_| Error::InvalidEnvelope)?;
        let nonce = STANDARD
            .decode(&e.nonce)
            .map_err(|_| Error::InvalidEnvelope)?
            .try_into()
            .map_err(|_| Error::InvalidEnvelope)?;
        let ciphertext = STANDARD
            .decode(&e.ciphertext)
            .map_err(|_| Error::InvalidEnvelope)?;
        // English BIP39 phrases are bounded; reject malformed lengths before the KDF.
        if !(16..=512).contains(&ciphertext.len()) {
            return Err(Error::InvalidEnvelope);
        }
        Ok((
            e,
            Decoded {
                salt,
                nonce,
                ciphertext,
            },
        ))
    }
    fn aad(&self) -> Vec<u8> {
        // A JSON tuple is unambiguous and domain-separated from the payload.
        serde_json::to_vec(&("akcli-wallet", self.version, &self.name, &self.profile))
            .expect("public strings serialize")
    }
}
fn key(password: &Secret, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>, Error> {
    let mut key = Zeroizing::new([0u8; 32]);
    let params = Params::new(65536, 3, 1, Some(32)).map_err(|_| Error::Crypto)?;
    // The convenience method allocates scratch memory without erasing it on drop.
    let mut memory = Zeroizing::new(vec![argon2::Block::default(); params.block_count()]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password.expose().as_bytes(), salt, &mut *key, &mut *memory)
        .map_err(|_| Error::Crypto)?;
    Ok(key)
}
fn canonical(mnemonic: &Secret) -> Result<Secret, Error> {
    // English words are ASCII: do not allocate unguarded Unicode-normalization buffers.
    if !mnemonic.expose().is_ascii() {
        return Err(Error::InvalidMnemonic);
    }
    let parsed = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, mnemonic.expose())
        .map_err(|_| Error::InvalidMnemonic)?;
    let mut text = Zeroizing::new(String::with_capacity(256));
    for (index, word) in parsed.words().enumerate() {
        if index != 0 {
            text.push(' ');
        }
        text.push_str(word);
    }
    Ok(Secret(text))
}

/// Encrypted mnemonic storage rooted at an explicitly supplied absolute home.
/// No seed derivation, agent, or CLI integration is provided.
pub struct WalletStore {
    directory: Directory,
}
impl WalletStore {
    pub fn open(home: &Path) -> Result<Self, Error> {
        Ok(Self {
            directory: Directory::open(home)?,
        })
    }

    pub fn create(
        &self,
        name: &WalletName,
        mnemonic: Secret,
        password: Secret,
    ) -> Result<(), Error> {
        if password.expose().is_empty() {
            return Err(Error::EmptyPassword);
        }
        let mnemonic = canonical(&mnemonic)?;
        let mut salt = [0u8; 16];
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut salt).map_err(|_| Error::Crypto)?;
        getrandom::fill(&mut nonce).map_err(|_| Error::Crypto)?;
        let key = key(&password, &salt)?;
        let mut e = Envelope {
            version: 1,
            name: name.as_str().to_owned(),
            profile: PROFILE.to_owned(),
            salt: STANDARD.encode(salt),
            nonce: STANDARD.encode(nonce),
            ciphertext: String::new(),
        };
        let mut payload = Zeroizing::new(Vec::with_capacity(mnemonic.expose().len() + 16));
        payload.extend_from_slice(mnemonic.expose().as_bytes());
        XChaCha20Poly1305::new((&*key).into())
            .encrypt_in_place(XNonce::from_slice(&nonce), &e.aad(), &mut *payload)
            .map_err(|_| Error::Crypto)?;
        e.ciphertext = STANDARD.encode(&*payload);
        let bytes = serde_json::to_vec(&e).map_err(|_| Error::Crypto)?;
        self.directory.publish(name, &bytes)
    }

    pub fn list(&self) -> Result<Vec<WalletName>, Error> {
        let mut complete = Vec::new();
        for name in self.directory.names()? {
            if let Some(bytes) = self.directory.read(&name)? {
                Envelope::decode(&bytes, &name)?;
                complete.push(name);
            }
        }
        Ok(complete)
    }

    pub fn unlock(&self, name: &WalletName, password: Secret) -> Result<Secret, Error> {
        self.unlock_inner(name, password).map_err(|_| Error::Unlock)
    }
    fn unlock_inner(&self, name: &WalletName, password: Secret) -> Result<Secret, Error> {
        if password.expose().is_empty() {
            return Err(Error::EmptyPassword);
        }
        let bytes = self.directory.read(name)?.ok_or(Error::Storage)?;
        let (e, decoded) = Envelope::decode(&bytes, name)?;
        let key = key(&password, &decoded.salt)?;
        let mut payload = Zeroizing::new(decoded.ciphertext);
        XChaCha20Poly1305::new((&*key).into())
            .decrypt_in_place(XNonce::from_slice(&decoded.nonce), &e.aad(), &mut *payload)
            .map_err(|_| Error::Unlock)?;
        let text = std::str::from_utf8(&payload).map_err(|_| Error::Unlock)?;
        let mnemonic = Secret::new(text.to_owned());
        let canonical = canonical(&mnemonic)?;
        if canonical.expose() != mnemonic.expose() {
            return Err(Error::Unlock);
        }
        Ok(canonical)
    }
}
