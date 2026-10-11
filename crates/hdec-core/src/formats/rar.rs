//! RAR 1.3–7 (read-only) via `rars`.

use crate::fs::{self as hfs, OutFile};
use crate::volume::{self, Volume};
use crate::{Ctx, Entry, Error, ExtractOptions, ProgressFn, Result};
use rars::{
    Archive, ArchiveMember, ArchiveMemberDetail, ArchiveReadOptions, ArchiveReader, AttrSource,
    ErrorKind,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const S_IFMT: u64 = 0o170000;
const S_IFLNK: u64 = 0o120000;

fn map_err(e: rars::Error, ctx: &Ctx) -> Error {
    match e.kind() {
        ErrorKind::PasswordRequired => Error::PasswordRequired,
        ErrorKind::BadPassword => Error::WrongPassword,
        // RAR4 has no password check value: a wrong password surfaces as a bad checksum.
        ErrorKind::ChecksumMismatch if ctx.password.is_some() => Error::WrongPassword,
        ErrorKind::Cancelled => Error::Cancelled,
        ErrorKind::UnsupportedFormat | ErrorKind::UnsupportedFeature => {
            Error::Unsupported(e.to_string())
        }
        ErrorKind::Io | ErrorKind::WriterFailure => {
            ctx.or_cancelled(io::Error::other(e.to_string()))
        }
        _ => Error::Corrupt(e.to_string()),
    }
}

fn options<'a>(ctx: &'a Ctx) -> ArchiveReadOptions<'a> {
    ArchiveReadOptions::with_optional_password(ctx.password.map(str::as_bytes))
}

/// `x.part1.rar, x.part2.rar, …` or `x.rar, x.r00, x.r01, …`; otherwise just `path`.
fn volume_paths(path: &Path) -> Vec<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let lower = name.to_ascii_lowercase();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some((base, num)) = lower
        .strip_suffix(".rar")
        .and_then(|s| s.rsplit_once(".part"))
        && digits(num)
    {
        let (base, ext) = (&name[..base.len()], &name[name.len() - 3..]);
        let width = num.len();
        let parts: Vec<_> = (1..)
            .map(|n| path.with_file_name(format!("{base}.part{n:0width$}.{ext}")))
            .take_while(|p| p.is_file())
            .collect();
        if !parts.is_empty() {
            return parts;
        }
    }
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    if ext == "rar" || (ext.len() == 3 && ext.starts_with('r') && digits(&ext[1..])) {
        let stem = &name[..name.len() - 4];
        let first = path.with_file_name(format!("{stem}.rar"));
        let rest = (0..)
            .map(|n| path.with_file_name(format!("{stem}.r{n:02}")))
            .take_while(|p| p.is_file());
        if first.is_file() {
            return std::iter::once(first).chain(rest).collect();
        }
    }
    vec![path.to_path_buf()]
}

fn open(path: &Path, ctx: &Ctx) -> Result<Vec<Archive>> {
    if volume::base_path(path).is_some() {
        let archive = ArchiveReader::read_reader_with_options(Volume::open(path)?, options(ctx));
        return Ok(vec![archive.map_err(|e| map_err(e, ctx))?]);
    }
    volume_paths(path)
        .iter()
        .map(|p| {
            ArchiveReader::read_path_with_options(p, options(ctx)).map_err(|e| map_err(e, ctx))
        })
        .collect()
}

fn members(archives: &[Archive], ctx: &Ctx) -> Result<Vec<ArchiveMember>> {
    rars::volume_members(archives).map_err(|e| map_err(e, ctx))
}

fn clean(name: &[u8]) -> String {
    String::from_utf8_lossy(name)
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned()
}

fn member_name(m: &ArchiveMember) -> String {
    clean(
        &m.decoded_name(None)
            .unwrap_or(std::borrow::Cow::Borrowed(&m.meta.name)),
    )
}

/// RAR5 symlink target, if this member is one (Unix or Windows symlink redirection).
fn symlink_target(m: &ArchiveMember) -> Option<String> {
    match &m.detail {
        ArchiveMemberDetail::Rar50Plus {
            redirection: Some(r),
            ..
        } if matches!(r.redirection_type, 1 | 2) => {
            Some(String::from_utf8_lossy(&r.target_name).replace('\\', "/"))
        }
        _ => None,
    }
}

pub fn list(path: &Path, ctx: &Ctx) -> Result<Vec<Entry>> {
    let archives = open(path, ctx)?;
    Ok(members(&archives, ctx)?
        .iter()
        .map(|m| Entry {
            path: member_name(m),
            size: m.meta.unpacked_size,
            packed: Some(m.meta.packed_size),
            modified: m.meta.modification_time(),
            is_dir: m.meta.is_directory,
            encrypted: m.meta.is_encrypted,
        })
        .collect())
}

/// Where one entry's bytes go, and what to do once it is complete.
enum Pending {
    File {
        out: Rc<RefCell<Option<OutFile>>>,
        modified: Option<SystemTime>,
        mode: Option<u32>,
    },
    Link {
        target: Rc<RefCell<Vec<u8>>>,
        path: PathBuf,
        name: String,
    },
}

/// The `'static` writer handed to `rars`: counts progress, honours cancel.
struct Sink {
    to: Target,
    done: Rc<Cell<u64>>,
    total: u64,
    name: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<ProgressFn>,
}

enum Target {
    File(Rc<RefCell<Option<OutFile>>>),
    Buf(Rc<RefCell<Vec<u8>>>),
    Null,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        let n = match &self.to {
            Target::File(out) => out
                .borrow_mut()
                .as_mut()
                .ok_or_else(|| io::Error::other("entry closed"))?
                .write(buf)?,
            Target::Buf(b) if b.borrow().len() + buf.len() > 4096 => {
                return Err(io::Error::other("symlink target too long"));
            }
            Target::Buf(b) => {
                b.borrow_mut().extend_from_slice(buf);
                buf.len()
            }
            Target::Null => buf.len(),
        };
        self.done.set(self.done.get() + n as u64);
        (*self.progress)(self.done.get(), self.total, &self.name);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        match &self.to {
            Target::File(out) => out.borrow_mut().as_mut().map_or(Ok(()), |f| f.flush()),
            _ => Ok(()),
        }
    }
}

fn finish(pending: Option<Pending>, dest: &Path, warnings: &mut Vec<String>) -> Result<()> {
    match pending {
        None => Ok(()),
        Some(Pending::File {
            out,
            modified,
            mode,
        }) => {
            let Some(out) = out.borrow_mut().take() else {
                return Ok(());
            };
            if let Some(t) = modified {
                out.file().set_modified(t)?;
            }
            hfs::set_mode(out.file(), mode)?;
            out.commit()
        }
        Some(Pending::Link { target, path, name }) => {
            let target = String::from_utf8_lossy(&target.borrow()).into_owned();
            if !hfs::symlink(dest, &path, &target)? {
                warnings.push(format!("skipped symlink {name} -> {target}"));
            }
            Ok(())
        }
    }
}

/// `dest = None` → test only.
pub fn extract(
    path: &Path,
    dest: Option<&Path>,
    opts: &ExtractOptions,
    ctx: &Ctx,
) -> Result<Vec<String>> {
    let archives = open(path, ctx)?;
    let members = members(&archives, ctx)?;
    let only = opts.only.as_deref();
    let modified: HashMap<String, Option<SystemTime>> = members
        .iter()
        .map(|m| (member_name(m), m.meta.modification_time()))
        .collect();
    let total = members
        .iter()
        .filter(|m| crate::is_selected(&member_name(m), only))
        .map(|m| m.meta.unpacked_size)
        .sum();
    let done = Rc::new(Cell::new(0));
    let mut warnings = Vec::new();
    let mut pending = None;
    let mut failure = None;

    let result = rars::extract_volumes_to_with_options(&archives, options(ctx), |meta| {
        let step = (|| -> Result<Target> {
            let dest_dir = dest.unwrap_or(Path::new(""));
            finish(pending.take(), dest_dir, &mut warnings)?;
            let name = clean(&meta.name);
            let Some(dest) = dest.filter(|_| crate::is_selected(&name, only)) else {
                return Ok(Target::Null);
            };
            if meta.is_directory {
                hfs::make_dirs(dest, &crate::safe_path::join(dest, &name)?)?;
                return Ok(Target::Null);
            }
            let Some(out_path) = hfs::output_path(dest, &name, opts.overwrite)? else {
                return Ok(Target::Null);
            };
            let unix = meta.attr_source == AttrSource::Unix;
            if unix && meta.file_attr & S_IFMT == S_IFLNK {
                let target = Rc::new(RefCell::new(Vec::new()));
                pending = Some(Pending::Link {
                    target: target.clone(),
                    path: out_path,
                    name,
                });
                return Ok(Target::Buf(target));
            }
            let out = Rc::new(RefCell::new(Some(OutFile::create(out_path)?)));
            let mode = unix.then_some(meta.file_attr as u32 & 0o7777);
            pending = Some(Pending::File {
                out: out.clone(),
                modified: modified.get(&name).copied().flatten(),
                mode,
            });
            Ok(Target::File(out))
        })();
        let to = match step {
            Ok(to) => to,
            Err(Error::UnsafePath(p)) => {
                warnings.push(format!("skipped unsafe path: {p}"));
                Target::Null
            }
            Err(e) => {
                failure = Some(e);
                return Err(io::Error::other("hdec: entry setup failed").into());
            }
        };
        Ok(Box::new(Sink {
            to,
            done: done.clone(),
            total,
            name: clean(&meta.name),
            cancel: ctx.cancel.clone(),
            progress: ctx.progress.clone(),
        }) as Box<dyn Write>)
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(|e| map_err(e, ctx))?;
    let Some(dest) = dest else {
        return Ok(warnings);
    };
    finish(pending.take(), dest, &mut warnings)?;

    // `extract_to` skips RAR5 redirections; recreate symlinks from the metadata.
    for m in &members {
        let name = member_name(m);
        if !crate::is_selected(&name, only) {
            continue;
        }
        match (symlink_target(m), &m.detail) {
            (Some(target), _) => match hfs::output_path(dest, &name, opts.overwrite) {
                Ok(Some(link)) if hfs::symlink(dest, &link, &target)? => {}
                Ok(None) => {}
                Ok(Some(_)) | Err(Error::UnsafePath(_)) => {
                    warnings.push(format!("skipped symlink {name} -> {target}"))
                }
                Err(e) => return Err(e),
            },
            (
                None,
                ArchiveMemberDetail::Rar50Plus {
                    redirection: Some(_),
                    ..
                },
            ) => {
                warnings.push(format!(
                    "skipped {name}: hard links, junctions and file copies aren't supported"
                ));
            }
            _ => {}
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_volume_sets() {
        let tmp = tempfile::tempdir().unwrap();
        let touch = |n: &str| std::fs::write(tmp.path().join(n), b"").unwrap();
        for n in [
            "a.part01.rar",
            "a.part02.rar",
            "a.part03.rar",
            "b.rar",
            "b.r00",
            "b.r01",
            "c.rar",
        ] {
            touch(n);
        }
        let names = |p: &str| -> Vec<String> {
            volume_paths(&tmp.path().join(p))
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            names("a.part02.rar"),
            ["a.part01.rar", "a.part02.rar", "a.part03.rar"]
        );
        assert_eq!(names("b.r01"), ["b.rar", "b.r00", "b.r01"]);
        assert_eq!(names("b.rar"), ["b.rar", "b.r00", "b.r01"]);
        assert_eq!(names("c.rar"), ["c.rar"]);
    }
}
