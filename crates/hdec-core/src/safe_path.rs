//! Maps archive entry names to paths that are guaranteed to stay inside the
//! extraction directory. Purely lexical: the caller must still refuse to write
//! through existing symlinks.

use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub fn join(dest: &Path, name: &str) -> Result<PathBuf> {
    join_for(dest, name, cfg!(windows))
}

fn join_for(dest: &Path, name: &str, windows: bool) -> Result<PathBuf> {
    let unsafe_path = || Error::UnsafePath(name.to_owned());
    let normalized = name.replace('\\', "/");
    // Leading '/' also catches UNC (`\\server`) and `\\?\` prefixes after normalization.
    if normalized.starts_with('/') || normalized.contains('\0') {
        return Err(unsafe_path());
    }
    let mut out = dest.to_path_buf();
    let mut pushed = false;
    for c in normalized.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err(unsafe_path()),
            c if windows => {
                out.push(sanitize_windows(c));
                pushed = true;
            }
            c => {
                out.push(c);
                pushed = true;
            }
        }
    }
    if pushed { Ok(out) } else { Err(unsafe_path()) }
}

/// True if a symlink at `link` (relative to the extraction dir) pointing to
/// `target` stays inside the extraction dir. `..` is allowed only at the
/// start: it then climbs from the link's own (real) folder, so the check holds
/// on disk too. After a name it could climb out of another symlink
/// (`s -> .` then `t -> s/../outside`), which a lexical check can't see.
pub fn link_stays_inside(link: &Path, target: &str) -> bool {
    let target = target.replace('\\', "/");
    if target.starts_with('/') || target.contains('\0') || target.contains(':') {
        return false;
    }
    let mut depth = link.parent().map_or(0, |p| p.components().count());
    let mut named = false;
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." if depth == 0 || named => return false,
            ".." => depth -= 1,
            _ => named = true,
        }
    }
    true
}

/// Makes one path component legal and harmless on Windows: no drive letters or
/// alternate data streams (`:`), no reserved device names, no trailing dots or
/// spaces (Windows strips those, which would turn `.. ` into `..`).
fn sanitize_windows(c: &str) -> String {
    let mut s: String = c
        .chars()
        .map(|ch| {
            if ch < ' ' || "<>:\"|?*".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    s.truncate(s.trim_end_matches(['.', ' ']).len());
    let stem = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if s.is_empty() || reserved {
        s.insert(0, '_');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str, windows: bool) -> PathBuf {
        join_for(Path::new("/dest"), name, windows).unwrap()
    }

    #[test]
    fn joins_normal_names() {
        for windows in [false, true] {
            assert_eq!(ok("a/b/c.txt", windows), Path::new("/dest/a/b/c.txt"));
            assert_eq!(ok("a\\b", windows), Path::new("/dest/a/b"));
            assert_eq!(ok("./a//b/", windows), Path::new("/dest/a/b"));
            assert_eq!(ok("dir/", windows), Path::new("/dest/dir"));
        }
    }

    #[test]
    fn rejects_escapes() {
        for name in [
            "../x",
            "a/../../x",
            "a/..",
            "..\\x",
            "/etc/passwd",
            "\\\\server\\share\\x",
            "\\\\?\\C:\\x",
            "a\0b",
            "",
            "./",
            "/",
        ] {
            for windows in [false, true] {
                assert!(
                    matches!(
                        join_for(Path::new("/dest"), name, windows),
                        Err(Error::UnsafePath(_))
                    ),
                    "{name:?} windows={windows}"
                );
            }
        }
    }

    #[test]
    fn sanitizes_for_windows() {
        assert_eq!(ok("C:/x", true), Path::new("/dest/C_/x"));
        assert_eq!(
            ok("file.txt:stream", true),
            Path::new("/dest/file.txt_stream")
        );
        assert_eq!(ok("a/CON.txt", true), Path::new("/dest/a/_CON.txt"));
        assert_eq!(ok("nul", true), Path::new("/dest/_nul"));
        assert_eq!(ok("com1", true), Path::new("/dest/_com1"));
        assert_eq!(ok("con .txt", true), Path::new("/dest/_con .txt"));
        assert_eq!(ok("com0", true), Path::new("/dest/com0"));
        assert_eq!(ok("console", true), Path::new("/dest/console"));
        assert_eq!(ok("x. . ", true), Path::new("/dest/x"));
        assert_eq!(ok(".. ", true), Path::new("/dest/_"));
        assert_eq!(ok("...", true), Path::new("/dest/_"));
        assert_eq!(ok("a<b>?", true), Path::new("/dest/a_b__"));
    }

    #[test]
    fn checks_symlink_targets() {
        let inside = |link: &str, target: &str| link_stays_inside(Path::new(link), target);
        assert!(inside("a/link", "b.txt"));
        assert!(inside("a/link", "../b.txt"));
        assert!(inside("a/b/link", "../../c"));
        assert!(!inside("a/link", "x/../../y"));
        assert!(!inside("a/link", "s/../b.txt"));
        assert!(!inside("link", "../x"));
        assert!(!inside("a/link", "../../x"));
        assert!(!inside("a/link", "x/../../../y"));
        assert!(!inside("link", "/etc/passwd"));
        assert!(!inside("link", "C:\\Windows"));
        assert!(!inside("link", "..\\x"));
    }

    #[test]
    fn keeps_unix_names() {
        assert_eq!(ok("a:b", false), Path::new("/dest/a:b"));
        assert_eq!(ok("con", false), Path::new("/dest/con"));
        assert_eq!(ok("...", false), Path::new("/dest/..."));
    }
}
