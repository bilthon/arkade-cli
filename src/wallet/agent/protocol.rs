use super::{Error, Status};
use crate::wallet::{balance::Balance, Secret, WalletName};
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
pub(super) const BALANCE: u8 = 6;

pub(super) enum Request {
    Status,
    Unlock(WalletName, Secret),
    Lock,
    Stop,
    Receive,
    Balance,
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
        Some(&BALANCE) if bytes.len() == 2 => Ok(Request::Balance),
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
// encodings are unchanged.
pub(super) fn receive_response(value: Result<String, Error>) -> Vec<u8> {
    match value {
        Ok(address) if !address.is_empty() && address.len() <= 512 && address.is_ascii() => {
            let mut bytes = vec![VERSION, 5];
            bytes.extend_from_slice(address.as_bytes());
            bytes
        }
        Err(Error::ReceiveMissingOperator) => vec![VERSION, 6],
        Err(Error::ReceiveConfig) => vec![VERSION, 7],
        Err(Error::ReceiveConnection) => vec![VERSION, 8],
        Err(Error::ReceiveHttp) => vec![VERSION, 9],
        Err(Error::ReceiveInfo) => vec![VERSION, 10],
        Err(Error::ReceiveStorage) => vec![VERSION, 11],
        _ => response(Err(Error::Receive)),
    }
}
pub(super) fn parse_receive_response(bytes: &[u8]) -> Result<String, Error> {
    match bytes {
        [VERSION, 4] => return Err(Error::Receive),
        [VERSION, 6] => return Err(Error::ReceiveMissingOperator),
        [VERSION, 7] => return Err(Error::ReceiveConfig),
        [VERSION, 8] => return Err(Error::ReceiveConnection),
        [VERSION, 9] => return Err(Error::ReceiveHttp),
        [VERSION, 10] => return Err(Error::ReceiveInfo),
        [VERSION, 11] => return Err(Error::ReceiveStorage),
        _ => (),
    }
    if !(3..=514).contains(&bytes.len()) || bytes[..2] != [VERSION, RECEIVE] {
        return Err(Error::Protocol);
    }
    let address = &bytes[2..];
    if !address.iter().all(u8::is_ascii_graphic) {
        return Err(Error::Protocol);
    }
    let address = std::str::from_utf8(address).map_err(|_| Error::Protocol)?;
    ark_core::ArkAddress::decode(address).map_err(|_| Error::Protocol)?;
    Ok(address.to_owned())
}

// Separate response tags preserve all existing status/receive frames. Five
// big-endian u64s, in Balance field order; no textual numbers or partial totals.
pub(super) fn balance_response(value: Result<Balance, Error>) -> Vec<u8> {
    let tag = match value {
        Ok(balance) => {
            let mut bytes = Vec::with_capacity(42);
            bytes.extend_from_slice(&[VERSION, 12]);
            for sats in [
                balance.offchain_balance_sats,
                balance.confirmed_sats,
                balance.preconfirmed_sats,
                balance.recoverable_sats,
                balance.pending_recovery_sats,
            ] {
                bytes.extend_from_slice(&sats.to_be_bytes());
            }
            return bytes;
        }
        Err(Error::BalanceSession) => 13,
        Err(Error::BalanceMissingOperator) => 14,
        Err(Error::BalanceConfig) => 15,
        Err(Error::BalanceMissingState) => 16,
        Err(Error::BalanceStorage) => 17,
        Err(Error::BalanceOperator) => 18,
        Err(Error::BalanceQuery) => 19,
        Err(Error::BalanceInvalidResponse) => 20,
        Err(Error::BalanceClock) => 21,
        Err(_) => 4,
    };
    vec![VERSION, tag]
}

pub(super) fn parse_balance_response(bytes: &[u8]) -> Result<Balance, Error> {
    if bytes.len() == 42 && bytes[..2] == [VERSION, 12] {
        let mut fields = bytes[2..].chunks_exact(8).map(|field| {
            u64::from_be_bytes(field.try_into().expect("fixed-width balance field"))
        });
        return Ok(Balance {
            offchain_balance_sats: fields.next().unwrap(),
            confirmed_sats: fields.next().unwrap(),
            preconfirmed_sats: fields.next().unwrap(),
            recoverable_sats: fields.next().unwrap(),
            pending_recovery_sats: fields.next().unwrap(),
        });
    }
    Err(match bytes {
        [VERSION, 13] => Error::BalanceSession,
        [VERSION, 14] => Error::BalanceMissingOperator,
        [VERSION, 15] => Error::BalanceConfig,
        [VERSION, 16] => Error::BalanceMissingState,
        [VERSION, 17] => Error::BalanceStorage,
        [VERSION, 18] => Error::BalanceOperator,
        [VERSION, 19] => Error::BalanceQuery,
        [VERSION, 20] => Error::BalanceInvalidResponse,
        [VERSION, 21] => Error::BalanceClock,
        _ => Error::Protocol,
    })
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
    fn balance_is_parameterless_and_fixed_width() {
        use crate::wallet::balance::Balance;
        assert!(matches!(parse(&[1, 6]), Ok(Request::Balance)));
        assert!(parse(&[1, 6, 0]).is_err());
        let balance = Balance {
            offchain_balance_sats: u64::MAX,
            confirmed_sats: 1,
            preconfirmed_sats: 2,
            recoverable_sats: 3,
            pending_recovery_sats: 4,
        };
        let bytes = balance_response(Ok(balance));
        assert_eq!(bytes.len(), 42);
        assert_eq!(&bytes[..10], &[1, 12, 255, 255, 255, 255, 255, 255, 255, 255]);
        assert_eq!(parse_balance_response(&bytes).unwrap(), balance);
        for length in 0..42 {
            assert!(matches!(parse_balance_response(&bytes[..length]), Err(Error::Protocol)));
        }
        let mut oversized = bytes.clone();
        oversized.push(0);
        assert!(matches!(parse_balance_response(&oversized), Err(Error::Protocol)));
        let mut wrong_version = bytes.clone();
        wrong_version[0] = 2;
        assert!(parse_balance_response(&wrong_version).is_err());
        assert!(parse_response(&bytes).is_err());
        assert!(parse_receive_response(&bytes).is_err());
    }

    #[test]
    fn balance_errors_are_exact_and_do_not_accept_other_response_kinds() {
        for (tag, error) in [
            (13, Error::BalanceSession),
            (14, Error::BalanceMissingOperator),
            (15, Error::BalanceConfig),
            (16, Error::BalanceMissingState),
            (17, Error::BalanceStorage),
            (18, Error::BalanceOperator),
            (19, Error::BalanceQuery),
            (20, Error::BalanceInvalidResponse),
            (21, Error::BalanceClock),
        ] {
            let message = error.to_string();
            assert_eq!(balance_response(Err(error)), [1, tag]);
            assert_eq!(parse_balance_response(&[1, tag]).unwrap_err().to_string(), message);
            assert!(matches!(parse_balance_response(&[1, tag, 0]), Err(Error::Protocol)));
            assert!(parse_response(&[1, tag]).is_err());
            assert!(parse_receive_response(&[1, tag]).is_err());
        }
        for tag in (0..=12).chain(22..=255) {
            assert!(matches!(parse_balance_response(&[1, tag]), Err(Error::Protocol)));
        }
    }

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
    fn receive_errors_have_exact_static_frames_and_reject_wrong_kinds() {
        for (tag, error) in [
            (6, Error::ReceiveMissingOperator),
            (7, Error::ReceiveConfig),
            (8, Error::ReceiveConnection),
            (9, Error::ReceiveHttp),
            (10, Error::ReceiveInfo),
            (11, Error::ReceiveStorage),
        ] {
            let message = error.to_string();
            assert_eq!(receive_response(Err(error)), [1, tag]);
            assert_eq!(
                parse_receive_response(&[1, tag]).unwrap_err().to_string(),
                message
            );
            assert!(matches!(parse_response(&[1, tag]), Err(Error::Protocol)));
            assert!(matches!(
                parse_receive_response(&[1, tag, 0]),
                Err(Error::Protocol)
            ));
        }
        for tag in 12..=255 {
            assert!(matches!(
                parse_receive_response(&[1, tag]),
                Err(Error::Protocol)
            ));
        }
    }
    #[test]
    fn receive_decoder_accepts_only_bounded_sdk_addresses() {
        let address = "tark1qpumuen7l8wthtz45p3ftn58pvrs9xlumvkuu2xet8egzkcklqte3egkjh5ls9mvgks7uczg0xv5s4pgkqt6ufmrt3qx0cfym0ja4y7t4pgslm";
        let valid = receive_response(Ok(address.into()));
        assert_eq!(parse_receive_response(&valid).unwrap(), address);
        assert!(parse_response(&valid).is_err());
        assert!(matches!(
            parse_receive_response(&[1, 4]),
            Err(Error::Receive)
        ));
        for bytes in [
            vec![],
            vec![1],
            vec![1, 5],
            vec![2, 5],
            vec![1, 0],
            b"\x01\x01alice".to_vec(),
            vec![1, 2],
            vec![1, 3],
            vec![1, 4, 0],
            b"\x01\x05not-an-address".to_vec(),
            [b"\x01\x05".as_slice(), &vec![b'x'; 513]].concat(),
            [valid.as_slice(), b"\n"].concat(),
            [valid.as_slice(), b"\x1b[31m"].concat(),
            [valid.as_slice(), b"\0"].concat(),
            [valid.as_slice(), b" "].concat(),
            [valid.as_slice(), "é".as_bytes()].concat(),
            valid[..valid.len() - 1].to_vec(),
        ] {
            assert!(matches!(
                parse_receive_response(&bytes),
                Err(Error::Protocol)
            ));
        }
    }

    #[tokio::test]
    async fn response_frames_reject_oversized_lengths_and_truncation() {
        for frame in [
            u32::MAX.to_be_bytes().to_vec(),
            ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
            vec![0, 0, 0, 1, 1],
            vec![0, 0, 0, 5, 1, 5, b't'],
            [vec![0, 0, 0, 42, 1, 12], vec![0; 39]].concat(),
            vec![0, 0],
        ] {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            writer.write_all(&frame).await.unwrap();
            writer.shutdown().await.unwrap();
            assert!(read(&mut reader).await.is_err());
        }
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
