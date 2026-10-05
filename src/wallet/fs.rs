use super::{Error, WalletName};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    path::Path,
};

pub(super) const LIMIT: usize = 16 * 1024;
pub(super) struct Directory(OwnedFd);
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn check(fd: &OwnedFd, directory: bool, parent: bool) -> Result<(), Error> {
    let s = fs::fstat(fd).map_err(|_| Error::Storage)?;
    let kind = FileType::from_raw_mode(s.st_mode);
    let valid_mode = if parent {
        s.st_mode & 0o022 == 0
    } else {
        s.st_mode & 0o7777 == if directory { 0o700 } else { 0o600 }
    };
    if s.st_uid != rustix::process::geteuid().as_raw()
        || !valid_mode
        || kind
            != if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            }
    {
        return Err(Error::Storage);
    }
    Ok(())
}

impl Directory {
    pub(super) fn open(home: &Path) -> Result<Self, Error> {
        if !home.is_absolute() {
            return Err(Error::Storage);
        }
        let home = fs::open(home, DIRECTORY, Mode::empty()).map_err(|_| Error::Storage)?;
        let mut current = home;
        for (name, parent) in [(".akcli", true), ("wallets", false)] {
            match fs::mkdirat(&current, name, Mode::from_raw_mode(0o700)) {
                Ok(()) => {
                    fs::fsync(&current).map_err(|_| Error::Storage)?;
                }
                Err(rustix::io::Errno::EXIST) => (),
                Err(_) => return Err(Error::Storage),
            }
            let next =
                fs::openat(&current, name, DIRECTORY, Mode::empty()).map_err(|_| Error::Storage)?;
            check(&next, true, parent)?;
            current = next;
        }
        Ok(Self(current))
    }

    pub(super) fn read(&self, name: &WalletName) -> Result<Vec<u8>, Error> {
        let fd = fs::openat(
            &self.0,
            format!("{}.json", name.as_str()),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Error::Storage)?;
        check(&fd, false, false)?;
        let mut bytes = Vec::new();
        File::from(fd)
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        if bytes.len() > LIMIT {
            return Err(Error::InvalidEnvelope);
        }
        Ok(bytes)
    }

    pub(super) fn names(&self) -> Result<Vec<WalletName>, Error> {
        let entries = fs::Dir::read_from(&self.0).map_err(|_| Error::Storage)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| Error::Storage)?;
            let name = entry.file_name().to_str().map_err(|_| Error::Storage)?;
            if name == "." || name == ".." || name.starts_with(".pending-") {
                continue;
            }
            let name = name.strip_suffix(".json").ok_or(Error::InvalidEnvelope)?;
            names.push(WalletName::new(name).map_err(|_| Error::InvalidEnvelope)?);
        }
        names.sort();
        Ok(names)
    }

    pub(super) fn publish(&self, name: &WalletName, bytes: &[u8]) -> Result<(), Error> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Crypto)?;
        let temporary = format!(".pending-{:032x}", u128::from_le_bytes(random));
        let fd = fs::openat(
            &self.0,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| Error::Storage)?;
        let result = (|| {
            check(&fd, false, false)?;
            let mut file = File::from(fd);
            file.write_all(bytes).map_err(|_| Error::Storage)?;
            file.sync_all().map_err(|_| Error::Storage)?;
            fs::linkat(
                &self.0,
                &temporary,
                &self.0,
                format!("{}.json", name.as_str()),
                AtFlags::empty(),
            )
            .map_err(|e| {
                if e == rustix::io::Errno::EXIST {
                    Error::AlreadyExists
                } else {
                    Error::Storage
                }
            })?;
            fs::fsync(&self.0).map_err(|_| Error::Storage)
        })();
        let cleanup =
            fs::unlinkat(&self.0, &temporary, AtFlags::empty()).map_err(|_| Error::Storage);
        let sync = fs::fsync(&self.0).map_err(|_| Error::Storage);
        result.and(cleanup).and(sync)
    }
}
