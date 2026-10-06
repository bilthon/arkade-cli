use super::{Error, Status};
use crate::wallet::{Secret, WalletName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};
use zeroize::Zeroizing;

pub(super) const MAX_PASSWORD: usize = 4096;
const MAX_FRAME: usize = 2 + 1 + 32 + 2 + MAX_PASSWORD;
const VERSION: u8 = 1;
pub(super) const STATUS: u8 = 1;
pub(super) const UNLOCK: u8 = 2;
pub(super) const LOCK: u8 = 3;
pub(super) const STOP: u8 = 4;
pub(super) const RECEIVE: u8 = 5;

pub(super) enum Request {
    Status,
    Unlock(WalletName, Secret),
    Lock,
    Stop,
    Receive,
}

pub(super) fn uid_matches(peer: u32, expected: u32) -> Result<(), Error> {
    if peer == expected {
        Ok(())
    } else {
        Err(Error::Protocol)
    }
}
pub(super) fn authenticate(stream: &UnixStream) -> Result<(), Error> {
    uid_matches(
        stream.peer_cred().map_err(|_| Error::Protocol)?.uid(),
        rustix::process::geteuid().as_raw(),
    )
}

// Exact allocation before any secret enters the buffer. Neither writer nor reader
// grows it. The guard is established before reads and survives cancellation/errors.
pub(super) fn request(
    op: u8,
    unlock: Option<(&WalletName, &Secret)>,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let size = match unlock {
        Some((name, password)) => {
            if password.expose().is_empty() || password.expose().len() > MAX_PASSWORD {
                return Err(Error::Protocol);
            }
            5 + name.as_str().len() + password.expose().len()
        }
        None => 2,
    };
    let mut bytes = Zeroizing::new(vec![0; size]);
    bytes[0] = VERSION;
    bytes[1] = op;
    if let Some((name, password)) = unlock {
        let n = name.as_str().len();
        bytes[2] = n as u8;
        bytes[3..3 + n].copy_from_slice(name.as_str().as_bytes());
        bytes[3 + n..5 + n].copy_from_slice(&(password.expose().len() as u16).to_be_bytes());
        bytes[5 + n..].copy_from_slice(password.expose().as_bytes());
    }
    Ok(bytes)
}

pub(super) async fn read(stream: &mut UnixStream) -> Result<Zeroizing<Vec<u8>>, Error> {
    let size = stream.read_u32().await.map_err(|_| Error::Protocol)? as usize;
    if !(2..=MAX_FRAME).contains(&size) {
        return Err(Error::Protocol);
    }
    let mut bytes = Zeroizing::new(vec![0; size]);
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|_| Error::Protocol)?;
    Ok(bytes)
}
pub(super) async fn write(stream: &mut UnixStream, bytes: &[u8]) -> Result<(), Error> {
    stream
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| Error::Protocol)?;
    stream.write_all(bytes).await.map_err(|_| Error::Protocol)
}

pub(super) fn parse(bytes: &[u8]) -> Result<Request, Error> {
    if bytes.first() != Some(&VERSION) {
        return Err(Error::Protocol);
    }
    match bytes.get(1) {
        Some(&STATUS) if bytes.len() == 2 => Ok(Request::Status),
        Some(&LOCK) if bytes.len() == 2 => Ok(Request::Lock),
        Some(&STOP) if bytes.len() == 2 => Ok(Request::Stop),
        Some(&RECEIVE) if bytes.len() == 2 => Ok(Request::Receive),
        Some(&UNLOCK) => {
            let n = *bytes.get(2).ok_or(Error::Protocol)? as usize;
            if !(1..=32).contains(&n) || bytes.len() < 5 + n {
                return Err(Error::Protocol);
            }
            let name = std::str::from_utf8(&bytes[3..3 + n]).map_err(|_| Error::Protocol)?;
            let name = WalletName::new(name).map_err(|_| Error::Protocol)?;
            let p = u16::from_be_bytes([bytes[3 + n], bytes[4 + n]]) as usize;
            if !(1..=MAX_PASSWORD).contains(&p) || bytes.len() != 5 + n + p {
                return Err(Error::Protocol);
            }
            let password = std::str::from_utf8(&bytes[5 + n..]).map_err(|_| Error::Protocol)?;
            // This owned copy is immediately guarded; UTF-8 errors never own input.
            Ok(Request::Unlock(name, Secret::new(password.to_owned())))
        }
        _ => Err(Error::Protocol),
    }
}

pub(super) fn response(value: Result<Status, Error>) -> Vec<u8> {
    match value {
        Ok(Status::Locked) => vec![VERSION, 0],
        Ok(Status::Unlocked(name)) => {
            let mut bytes = vec![VERSION, 1];
            bytes.extend_from_slice(name.as_str().as_bytes());
            bytes
        }
        Err(Error::Busy) => vec![VERSION, 2],
        Err(Error::Unlock) => vec![VERSION, 3],
        Err(_) => vec![VERSION, 4],
    }
}
// Receive success is address-only, with its own tag. Existing status/error
// encodings are unchanged. R3 will add the client-side address decoder.
pub(super) fn receive_response(value: Result<String, Error>) -> Vec<u8> {
    match value {
        Ok(address) if !address.is_empty() && address.len() <= 512 && address.is_ascii() => {
            let mut bytes = vec![VERSION, 5];
            bytes.extend_from_slice(address.as_bytes());
            bytes
        }
        _ => response(Err(Error::Receive)),
    }
}
pub(super) fn parse_response(bytes: &[u8]) -> Result<Status, Error> {
    if bytes.first() != Some(&VERSION) {
        return Err(Error::Protocol);
    }
    match bytes.get(1) {
        Some(0) if bytes.len() == 2 => Ok(Status::Locked),
        Some(1) => {
            let name = std::str::from_utf8(&bytes[2..]).map_err(|_| Error::Protocol)?;
            Ok(Status::Unlocked(
                WalletName::new(name).map_err(|_| Error::Protocol)?,
            ))
        }
        Some(2) if bytes.len() == 2 => Err(Error::Busy),
        Some(3) if bytes.len() == 2 => Err(Error::Unlock),
        _ => Err(Error::Protocol),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receive_is_parameterless_and_bounded() {
        assert!(parse(&[1, 5]).is_ok());
        assert!(parse(&[1, 5, 0]).is_err());
        assert_eq!(
            receive_response(Ok("tark1fixture".into())),
            b"\x01\x05tark1fixture"
        );
        for invalid in [String::new(), "x".repeat(513), "é".into()] {
            assert_eq!(receive_response(Ok(invalid)), [1, 4]);
        }
        assert_eq!(receive_response(Err(Error::Receive)), [1, 4]);
    }
    #[test]
    fn malformed_payloads_and_uid_policy() {
        for bytes in [
            vec![],
            vec![1],
            vec![2, 1],
            vec![1, 99],
            vec![1, 1, 0],
            vec![1, 2, 0, 0, 0],
            vec![1, 2, 1, b'A', 0, 1, b'x'],
            vec![1, 2, 1, b'a', 0, 2, b'x'],
        ] {
            assert!(parse(&bytes).is_err());
        }
        assert!(uid_matches(42, 42).is_ok());
        assert!(uid_matches(43, 42).is_err());
    }
    #[test]
    fn roundtrip_password_bounds_and_public_responses() {
        let name = WalletName::new("alice").unwrap();
        for length in [1, MAX_PASSWORD] {
            let secret = Secret::new("x".repeat(length));
            let bytes = request(UNLOCK, Some((&name, &secret))).unwrap();
            assert!(matches!(parse(&bytes), Ok(Request::Unlock(_, _))));
            let mut trailing = bytes;
            trailing[3 + name.as_str().len()] = 0;
            trailing[4 + name.as_str().len()] = 0;
            assert!(parse(&trailing).is_err());
        }
        for length in [0, MAX_PASSWORD + 1] {
            assert!(request(UNLOCK, Some((&name, &Secret::new("x".repeat(length))))).is_err());
        }
        assert_eq!(response(Err(Error::Unlock)), [1, 3]);
        assert_eq!(response(Ok(Status::Unlocked(name))), b"\x01\x01alice");
    }
}
