use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use clap::Args;
use fbz::{Error, Result, zip::PathInput};
use rgapi::FindOptions;

fn invalid(message: impl Into<String>) -> Error { Error::InvalidConfiguration(message.into()) }

#[derive(Args)]
#[group(multiple = true)]
#[command(next_help_heading = "Archive selection")]
pub(super) struct Filters {
    /// Respect .gitignore, .ignore, and .rgignore rules; hidden files remain included.
    #[arg(short = 'i', long)]
    ignore: bool,
    /// Exclude matching names or root-relative paths, pruning directory subtrees; repeatable.
    #[arg(short = 'E', long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Include matching names or root-relative paths; repeatable. Excludes always win.
    #[arg(long, value_name = "GLOB")]
    include: Vec<String>,
    /// Select extensions (e.g. py); repeatable. Must also match any --include filter.
    #[arg(short = 'e', long, value_name = "EXT")]
    extension: Vec<String>,
    /// Print selected archive entry names and a summary without writing an archive.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

impl Filters {
    pub fn is_set(&self) -> bool { self.ignore || self.dry_run || !self.exclude.is_empty() || !self.include.is_empty() || !self.extension.is_empty() }
}

// Resolve parent aliases for output exclusion, but never dereference an entry's own symlink.
fn entry_path(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(parent) = parent.canonicalize() { return Ok(parent.join(name)); }
    }
    Ok(path)
}

pub(super) fn select(inputs: &[String], filters: &Filters, output: &Path) -> Result<Vec<PathInput>> {
    let output = (output != Path::new("-")).then(|| entry_path(output)).transpose()?;
    let mut entries = Vec::new();
    let mut names = HashSet::new();
    for input in inputs {
        if input == "-" { return Err(invalid("stdin cannot be used as an archive entry")); }
        let source = entry_path(Path::new(input))?;
        if output.as_ref() == Some(&source) { return Err(invalid(format!("input and output are both {}", source.display()))); }
        let name = archive_name(Path::new(input))?;
        let directory = fs::symlink_metadata(&source)?.is_dir();
        let opts = FindOptions {
            root: source.clone(), hidden: true, ignore: filters.ignore, dirs: true, special_files: true,
            includes: filters.include.clone(), excludes: filters.exclude.clone(),
            exts: filters.extension.iter().map(|ext| format!("*.{}", ext.trim_start_matches('.'))).collect(),
            ..Default::default()
        };
        let mut paths = BTreeSet::new();
        for path in rgapi::find_iter(&opts).map_err(|e| invalid(e.to_string()))? {
            let path = path.map_err(|e| invalid(e.to_string()))?;
            if directory {
                if output.as_ref() == Some(&source.join(&path)) { continue; }
                paths.extend(path.ancestors().map(Path::to_path_buf));
            } else { paths.insert(PathBuf::new()); }
        }
        // Keep empty input directories unless a positive filter selects only their contents.
        if directory && filters.include.is_empty() && filters.extension.is_empty() { paths.insert(PathBuf::new()); }
        for path in paths {
            let (source, archive_path) = if path.as_os_str().is_empty() { (source.clone(), name.clone()) } else { (source.join(&path), name.join(&path)) };
            let kind = fs::symlink_metadata(&source)?.file_type();
            if !kind.is_file() && !kind.is_dir() && !kind.is_symlink() { return Err(invalid(format!("unsupported filesystem entry {}", source.display()))); }
            if !names.insert(archive_path.clone()) { return Err(invalid(format!("duplicate archive path {}", archive_path.display()))); }
            entries.push(PathInput { source, archive_path });
        }
    }
    if entries.is_empty() { return Err(invalid("no archive entries matched the filters")); }
    Ok(entries)
}

pub(super) fn preview(entries: &[PathInput], output: &Path, quiet: bool) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for entry in entries {
        let suffix = if fs::symlink_metadata(&entry.source)?.is_dir() { "/" } else { "" };
        writeln!(stdout, "{}{suffix}", entry.archive_path.display())?;
    }
    if !quiet { eprintln!("{} entries would be archived to {}", entries.len(), output.display()); }
    Ok(())
}

pub(super) fn archive_name(path: &Path) -> Result<PathBuf> {
    let relative = if path.is_absolute() {
        let current = env::current_dir()?;
        path.strip_prefix(&current).ok().map(Path::to_path_buf).or_else(|| path.file_name().map(PathBuf::from))
    } else { Some(path.to_path_buf()) }
    .ok_or_else(|| invalid(format!("cannot derive an archive name for {}", path.display())))?;
    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => clean.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(format!("refusing unsafe archive input name {}", relative.display())));
            }
        }
    }
    if clean.as_os_str().is_empty() { return Err(invalid(format!("cannot derive an archive name for {}", path.display()))); }
    Ok(clean)
}
