use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};

use tokio_util::sync::CancellationToken;

use super::ChunkStoreError;

pub(super) fn read_chunk(
    reader: &mut impl Read,
    buffer: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<usize, ChunkStoreError> {
    let mut filled = 0;
    while filled < buffer.len() {
        ensure_not_cancelled(cancellation)?;
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(filled)
}

pub(super) fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum: u64,
    cancellation: &CancellationToken,
) -> Result<u64, ChunkStoreError> {
    let mut limited = reader.take(maximum);
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        ensure_not_cancelled(cancellation)?;
        let read = limited.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        total += u64::try_from(read).map_err(|_| ChunkStoreError::SizeOverflow)?;
    }
    Ok(total)
}

pub(super) fn ensure_not_cancelled(
    cancellation: &CancellationToken,
) -> Result<(), ChunkStoreError> {
    if cancellation.is_cancelled() {
        Err(ChunkStoreError::Cancelled)
    } else {
        Ok(())
    }
}
pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(super) fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
