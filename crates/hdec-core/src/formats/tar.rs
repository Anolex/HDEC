//! TAR and compressed TAR (gz, bz2, xz, zst).

use super::stream::{Encoder, decoder};
use crate::fs::{self as hfs, Extractor, OutFile};
use crate::volume::Volume;
use crate::{CountingRead, CreateOptions, Ctx, Entry, ExtractOptions, Format, Meter, Result};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
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
    let mut archive = tar::Archive::new(decoder(format, BufReader::new(Volume::open(path)?))?);
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
            link: match kind {
                EntryType::Symlink => Some(
                    entry
                        .link_name()?
                        .map(|l| l.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
                _ => None,
            },
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
    let file = Volume::open(path)?;
    let consumed = AtomicU64::new(0);
    let mut meter = Meter::streamed(ctx, file.len(), &consumed);
    let input = BufReader::new(CountingRead {
        inner: file,
        count: &consumed,
    });
    crate::read_ahead(decoder(format, input)?, |r| {
        let mut archive = tar::Archive::new(r);
        let mut x = dest
            .map(|d| Extractor::new(d, opts.overwrite, ctx))
            .transpose()?;
        for entry in archive.entries()? {
            ctx.check()?;
            let mut entry = entry?;
            let name = entry_name(&entry)?;
            if !crate::is_selected(&name, opts.only.as_deref()) {
                continue;
            }
            let Some(x) = &mut x else {
                meter.copy(&name, &mut entry, &mut io::sink())?;
                continue;
            };
            match extract_entry(&mut entry, &name, x, &mut meter) {
                Err(crate::Error::UnsafePath(p)) => {
                    x.warnings.push(format!("skipped unsafe path: {p}"))
                }
                r => r?,
            }
        }
        x.map_or(Ok(Vec::new()), Extractor::finish)
    })
}

fn extract_entry(
    entry: &mut tar::Entry<impl io::Read>,
    name: &str,
    x: &mut Extractor,
    meter: &mut Meter,
) -> Result<()> {
    let kind = entry.header().entry_type();
    let link = || -> Result<String> {
        Ok(entry
            .link_name()?
            .map(|l| l.to_string_lossy().into_owned())
            .unwrap_or_default())
    };
    match kind {
        EntryType::Directory => x.dir(name),
        EntryType::Symlink => x.symlink(name, &link()?),
        EntryType::Link => x.hard_link(name, &link()?),
        EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
            let Some(mut out) = x.file(name, entry.size())? else {
                return Ok(());
            };
            meter.copy(name, entry, &mut out)?;
            let header = entry.header();
            x.done(out, mtime(header), header.mode().ok())
        }
        other => {
            x.warnings
                .push(format!("skipped {name}: unsupported entry type {other:?}"));
            Ok(())
        }
    }
}

pub fn create(sources: &[PathBuf], dest: &Path, opts: &CreateOptions, ctx: &Ctx) -> Result<()> {
    let items = hfs::walk(sources)?;
    let meter = Meter::new(
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
                .map_err(|e| ctx.or_cancelled(e))?;
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
