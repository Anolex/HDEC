//! TAR and compressed TAR (gz, bz2, xz, zst).

use super::stream::{Encoder, decoder};
use crate::fs::{self as hfs, OutFile};
use crate::{
    CountingRead, CreateOptions, Ctx, Entry, ExtractOptions, Format, Meter, Result, safe_path,
};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tar::EntryType;

fn entry_name(entry: &tar::Entry<impl io::Read>) -> Result<String> {
    Ok(entry
        .path()?
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned())
}

fn mtime(header: &tar::Header) -> Option<SystemTime> {
    header
        .mtime()
        .ok()
        .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

pub fn list(path: &Path, format: Format) -> Result<Vec<Entry>> {
    let mut archive = tar::Archive::new(decoder(format, BufReader::new(File::open(path)?))?);
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let kind = entry.header().entry_type();
        if !matches!(
            kind,
            EntryType::Regular
                | EntryType::Continuous
                | EntryType::GNUSparse
                | EntryType::Directory
                | EntryType::Symlink
                | EntryType::Link
        ) {
            continue;
        }
        out.push(Entry {
            path: entry_name(&entry)?,
            size: entry.size(),
            packed: None,
            modified: mtime(entry.header()),
            is_dir: kind == EntryType::Directory,
            encrypted: false,
        });
    }
    Ok(out)
}

/// `dest = None` → test only.
pub fn extract(
    path: &Path,
    format: Format,
    dest: Option<&Path>,
    opts: &ExtractOptions,
    ctx: &Ctx,
) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let consumed = Cell::new(0);
    let mut meter = Meter::streamed(ctx, file.metadata()?.len(), &consumed);
    let input = BufReader::new(CountingRead {
        inner: file,
        count: &consumed,
    });
    let mut archive = tar::Archive::new(decoder(format, input)?);
    let mut warnings = Vec::new();
    for entry in archive.entries()? {
        ctx.check()?;
        let mut entry = entry?;
        let name = entry_name(&entry)?;
        if !crate::is_selected(&name, opts.only.as_deref()) {
            continue;
        }
        let Some(dest) = dest else {
            meter.copy(&name, &mut entry, &mut io::sink())?;
            continue;
        };
        match extract_entry(&mut entry, &name, dest, opts, &mut meter, &mut warnings) {
            Err(crate::Error::UnsafePath(p)) => warnings.push(format!("skipped unsafe path: {p}")),
            r => r?,
        }
    }
    Ok(warnings)
}

fn extract_entry(
    entry: &mut tar::Entry<impl io::Read>,
    name: &str,
    dest: &Path,
    opts: &ExtractOptions,
    meter: &mut Meter,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let kind = entry.header().entry_type();
    match kind {
        EntryType::Directory => return hfs::make_dirs(dest, &safe_path::join(dest, name)?),
        EntryType::Regular
        | EntryType::Continuous
        | EntryType::GNUSparse
        | EntryType::Symlink
        | EntryType::Link => {}
        other => {
            warnings.push(format!("skipped {name}: unsupported entry type {other:?}"));
            return Ok(());
        }
    }
    let Some(out_path) = hfs::output_path(dest, name, opts.overwrite)? else {
        return Ok(());
    };
    let link = entry
        .link_name()?
        .map(|l| l.to_string_lossy().into_owned())
        .unwrap_or_default();
    if kind == EntryType::Symlink {
        if !hfs::symlink(dest, &out_path, &link)? {
            warnings.push(format!("skipped symlink {name} -> {link}"));
        }
        return Ok(());
    }
    if kind == EntryType::Link {
        let target = safe_path::join(dest, &link)?;
        if fs::symlink_metadata(&out_path).is_ok() {
            fs::remove_file(&out_path)?;
        }
        if fs::hard_link(&target, &out_path).is_err() {
            warnings.push(format!("skipped hard link {name} -> {link}"));
        }
        return Ok(());
    }
    let mut out = OutFile::create(out_path)?;
    meter.copy(name, entry, &mut out)?;
    if let Some(t) = mtime(entry.header()) {
        out.file().set_modified(t)?;
    }
    hfs::set_mode(out.file(), entry.header().mode().ok())?;
    out.commit()
}

pub fn create(sources: &[PathBuf], dest: &Path, opts: &CreateOptions, ctx: &Ctx) -> Result<()> {
    let items = hfs::walk(sources)?;
    let mut meter = Meter::new(
        ctx,
        items
            .iter()
            .filter(|s| s.meta.is_file())
            .map(|s| s.meta.len())
            .sum(),
    );
    let out = BufWriter::new(OutFile::create(dest.to_path_buf())?);
    let mut builder = tar::Builder::new(Encoder::new(opts.format, out, opts.level)?);
    for item in &items {
        ctx.check()?;
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&item.meta);
        if item.meta.is_dir() {
            header.set_size(0);
            builder.append_data(&mut header, &item.name, io::empty())?;
        } else if item.meta.is_symlink() {
            header.set_size(0);
            builder.append_link(&mut header, &item.name, fs::read_link(&item.path)?)?;
        } else {
            let file = File::open(&item.path)?;
            builder
                .append_data(&mut header, &item.name, meter.reader(&item.name, file))
                .map_err(|e| ctx.io_error(e))?;
        }
    }
    let out = builder
        .into_inner()?
        .finish()?
        .into_inner()
        .map_err(|e| e.into_error())?;
    out.file().sync_all()?;
    out.commit()
}
