use std::{fs, io, path::{Path, PathBuf}};

use fbz::{Error, Result};
use tempfile::TempDir;

fn path_metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)), Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None), Err(error) => Err(error)
    }
}

fn children(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut result = fs::read_dir(path)?.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?;
    result.sort_unstable();
    Ok(result)
}

fn existing_error(path: &Path) -> Error { Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists (use --force)", path.display()))) }

fn directory_collision(path: &Path) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::AlreadyExists, format!("refusing to replace directory {} with an archive entry", path.display())))
}

fn preflight(source: &Path, target: &Path, overwrite: bool) -> Result<()> {
    let Some(target_metadata) = path_metadata(target)? else { return Ok(()); };
    let source_metadata = fs::symlink_metadata(source)?;
    if source_metadata.is_dir() && target_metadata.is_dir() {
        for child in children(source)? { preflight(&child, &target.join(child.file_name().unwrap()), overwrite)?; }
        return Ok(());
    }
    if target_metadata.is_dir() { return Err(directory_collision(target)); }
    if overwrite { Ok(()) } else { Err(existing_error(target)) }
}

#[cfg(unix)]
fn make_directory_mutable(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(metadata.permissions().mode() | 0o700))
}

#[cfg(not(unix))]
fn make_directory_mutable(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

fn commit_entry(source: &Path, target: &Path, overwrite: bool) -> Result<()> {
    let source_metadata = fs::symlink_metadata(source)?;
    let Some(target_metadata) = path_metadata(target)? else { fs::rename(source, target)?; return Ok(()); };
    if source_metadata.is_dir() && target_metadata.is_dir() {
        make_directory_mutable(source, &source_metadata)?;
        for child in children(source)? { commit_entry(&child, &target.join(child.file_name().unwrap()), overwrite)?; }
        fs::remove_dir(source)?;
        return Ok(());
    }
    if target_metadata.is_dir() { return Err(directory_collision(target)); }
    if !overwrite { return Err(existing_error(target)); }
    fs::remove_file(target)?;
    fs::rename(source, target)?;
    Ok(())
}

pub(super) fn staging(destination: &Path) -> Result<TempDir> {
    let parent = match path_metadata(destination)? {
        Some(metadata) if metadata.is_dir() => destination,
        Some(_) => {
            return Err(Error::Io(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", destination.display()))));
        }
        None => destination.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")),
    };
    fs::create_dir_all(parent)?;
    TempDir::new_in(parent).map_err(Error::from)
}

pub(super) fn commit(staging: &Path, destination: &Path, overwrite: bool) -> Result<()> {
    if path_metadata(destination)?.is_none() { fs::create_dir(destination)?; }
    let entries = children(staging)?;
    for source in &entries { preflight(source, &destination.join(source.file_name().unwrap()), overwrite)?; }
    for source in entries { commit_entry(&source, &destination.join(source.file_name().unwrap()), overwrite)?; }
    Ok(())
}
