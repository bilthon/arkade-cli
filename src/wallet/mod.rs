//! Encrypted mnemonic storage. No seed derivation or operational wallet interface.

#[cfg(unix)]
mod fs;
#[cfg(unix)]
mod store;
#[cfg(unix)]
pub use store::WalletStore;

/// Owned secret text, erased on drop on a best-effort basis. Never implements Debug.
pub struct Secret(zeroize::Zeroizing<String>);
impl Secret {
    pub fn new(value: String) -> Self {
        Self(zeroize::Zeroizing::new(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// A wallet's validated, public filesystem identifier.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WalletName(String);

impl WalletName {
    pub fn new(name: &str) -> Result<Self, Error> {
        if !(1..=32).contains(&name.len())
            || !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit()
            || !name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        {
            return Err(Error::InvalidName);
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid wallet name")]
    InvalidName,
    #[error("invalid English mnemonic")]
    InvalidMnemonic,
    #[error("password must not be empty")]
    EmptyPassword,
    #[error("unsafe or unavailable wallet storage")]
    Storage,
    #[error("wallet already exists")]
    AlreadyExists,
    #[error("invalid wallet envelope")]
    InvalidEnvelope,
    #[error("unable to unlock wallet")]
    Unlock,
    #[error("cryptographic operation failed")]
    Crypto,
}
