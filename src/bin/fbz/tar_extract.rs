use std::{io, path::Path, thread};

use fbz::{Error, PipeWriter, Result, output_pipe};

use super::archive_extract;

fn broken_pipe(error: &Error) -> bool { matches!(error, Error::Io(source) if source.kind() == io::ErrorKind::BrokenPipe) }

pub(super) fn unpack<F>(destination: &Path, overwrite: bool, decode: F) -> Result<()> where F: FnOnce(&mut PipeWriter) -> Result<()> + Send {
    let staging = archive_extract::staging(destination)?;
    thread::scope(|scope| {
        let (mut writer, mut reader) = output_pipe();
        let decoder = scope.spawn(move || decode(&mut writer));
        let extracted = {
            let mut archive = tar::Archive::new(&mut reader);
            archive.set_overwrite(true);
            archive.unpack(staging.path()).map_err(Error::from)
        };

        let drained = if extracted.is_ok() { io::copy(&mut reader, &mut io::sink()).map(|_| ()).map_err(Error::from) } else { Ok(()) };
        drop(reader);

        let decoded = decoder.join().map_err(|_| Error::InvalidConfiguration("decoder worker panicked while extracting tar".into()))?;
        match (decoded, extracted, drained) {
            (Ok(()), Ok(()), Ok(())) => archive_extract::commit(staging.path(), destination, overwrite),
            (Err(error), Err(archive_error), _) if broken_pipe(&error) => Err(archive_error), (Err(error), _, _) => Err(error),
            (Ok(()), Err(error), _) | (Ok(()), Ok(()), Err(error)) => Err(error)
        }
    })
}
