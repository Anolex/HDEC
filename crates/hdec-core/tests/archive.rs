use hdec_core::{CreateOptions, Ctx, Error, ExtractOptions, Format, Level, Overwrite};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

fn noop(_: u64, _: u64, _: &str) {}

fn ctx<'a>(password: Option<&'a str>, cancel: &'a AtomicBool) -> Ctx<'a> {
    Ctx {
        password,
        cancel,
        progress: &noop,
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Builds `<dir>/tree` with the awkward cases every format must survive.
fn sample_tree(dir: &Path) -> PathBuf {
    let root = dir.join("tree");
    fs::create_dir_all(root.join("sub/deeper")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::write(root.join("a.txt"), "hello").unwrap();
    fs::write(root.join("zero.bin"), "").unwrap();
    fs::write(root.join("sub/ünïcødé ñame.txt"), "unicode").unwrap();
    let big: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(root.join("sub/deeper/big.bin"), big).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::options()
        .write(true)
        .open(root.join("a.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    root
}

/// Relative path → file contents, "<dir>" or "-> target".
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let meta = fs::symlink_metadata(&p).unwrap();
            let value = if meta.is_symlink() {
                format!("-> {}", fs::read_link(&p).unwrap().display()).into_bytes()
            } else if meta.is_dir() {
                stack.push(p);
                b"<dir>".to_vec()
            } else {
                fs::read(&p).unwrap()
            };
            out.insert(rel, value);
        }
    }
    out
}

fn create(
    format: Format,
    level: Level,
    zip_crypto: bool,
    password: Option<&str>,
) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = sample_tree(tmp.path());
    let archive = tmp.path().join("out.archive");
    let cancel = AtomicBool::new(false);
    let opts = CreateOptions {
        format,
        level,
        zip_crypto,
    };
    hdec_core::create(
        std::slice::from_ref(&src),
        &archive,
        &opts,
        &ctx(password, &cancel),
    )
    .unwrap();
    (tmp, src, archive)
}

fn extract(archive: &Path, dest: &Path, password: Option<&str>) -> hdec_core::Result<Vec<String>> {
    let cancel = AtomicBool::new(false);
    hdec_core::extract(
        archive,
        dest,
        &ExtractOptions::default(),
        &ctx(password, &cancel),
    )
}

#[test]
fn zip_round_trips_every_level() {
    for level in [Level::Store, Level::Fast, Level::Normal, Level::Max] {
        let (tmp, src, archive) = create(Format::Zip, level, false, None);
        let out = tmp.path().join("x");
        assert!(extract(&archive, &out, None).unwrap().is_empty());
        assert_eq!(snapshot(&out.join("tree")), snapshot(&src), "{level:?}");

        let cancel = AtomicBool::new(false);
        let names: Vec<_> = hdec_core::list(&archive, &ctx(None, &cancel))
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert!(
            names.contains(&"tree/sub/ünïcødé ñame.txt".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"tree/empty".to_string()));
        hdec_core::test(&archive, &ctx(None, &cancel)).unwrap();
    }
}

#[test]
fn zip_keeps_modification_time() {
    let (tmp, _, archive) = create(Format::Zip, Level::Normal, false, None);
    let out = tmp.path().join("x");
    extract(&archive, &out, None).unwrap();
    let got = fs::metadata(out.join("tree/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let want = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    let diff = got.duration_since(want).unwrap_or_else(|e| e.duration());
    assert!(diff <= Duration::from_secs(2), "{diff:?}"); // DOS time has 2 s resolution
}

#[test]
fn zip_passwords() {
    for zip_crypto in [false, true] {
        let (tmp, src, archive) = create(Format::Zip, Level::Normal, zip_crypto, Some("pässword"));
        let out = tmp.path().join("x");
        assert!(matches!(
            extract(&archive, &out, None),
            Err(Error::PasswordRequired)
        ));
        assert!(matches!(
            extract(&archive, &out, Some("nope")),
            Err(Error::WrongPassword)
        ));
        let out = tmp.path().join("y");
        extract(&archive, &out, Some("pässword")).unwrap();
        assert_eq!(
            snapshot(&out.join("tree")),
            snapshot(&src),
            "zip_crypto={zip_crypto}"
        );
        let cancel = AtomicBool::new(false);
        let entries = hdec_core::list(&archive, &ctx(None, &cancel)).unwrap();
        assert!(entries.iter().filter(|e| !e.is_dir).all(|e| e.encrypted));
    }
}

#[test]
fn reads_info_zip_archives() {
    let tmp = tempfile::tempdir().unwrap();
    extract(&fixture("infozip.zip"), tmp.path(), None).unwrap();
    assert_eq!(fs::read(tmp.path().join("tree/a.txt")).unwrap(), b"hello");
    assert_eq!(
        fs::read(tmp.path().join("tree/sub/b.txt")).unwrap(),
        b"world"
    );
    assert!(tmp.path().join("tree/empty").is_dir());

    let enc = fixture("zipcrypto_infozip.zip");
    let fresh = tmp.path().join("fresh");
    assert!(matches!(
        extract(&enc, &fresh, None),
        Err(Error::PasswordRequired)
    ));
    assert!(matches!(
        extract(&enc, &fresh, Some("wrong")),
        Err(Error::WrongPassword)
    ));
    let out = tmp.path().join("enc");
    extract(&enc, &out, Some("secret")).unwrap();
    assert_eq!(fs::read(out.join("tree/sub/b.txt")).unwrap(), b"world");
}

#[test]
fn skips_path_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("inner");
    let warnings = extract(&fixture("slip.zip"), &dest, None).unwrap();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert_eq!(fs::read(dest.join("ok.txt")).unwrap(), b"ok");
    assert!(!tmp.path().join("evil.txt").exists());
    assert!(!dest.join("abs.txt").exists());
}

#[test]
fn skips_escaping_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("a/b");
    let warnings = extract(&fixture("symlink_escape.zip"), &dest, None).unwrap();
    assert!(warnings.iter().any(|w| w.contains("link")), "{warnings:?}");
    assert!(!fs::symlink_metadata(dest.join("link")).is_ok_and(|m| m.is_symlink()));
    assert!(!tmp.path().join("outside").exists());
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(dest.join("good")).unwrap(),
        Path::new("ok.txt")
    );
}

#[test]
fn overwrite_policies() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = fixture("infozip.zip");
    let cancel = AtomicBool::new(false);
    let c = ctx(None, &cancel);
    let a = tmp.path().join("tree/a.txt");
    let run = |overwrite| {
        hdec_core::extract(
            &archive,
            tmp.path(),
            &ExtractOptions {
                only: None,
                overwrite,
            },
            &c,
        )
        .unwrap();
    };
    run(Overwrite::Skip);
    fs::write(&a, "changed").unwrap();
    run(Overwrite::Skip);
    assert_eq!(fs::read(&a).unwrap(), b"changed");
    run(Overwrite::Rename);
    assert_eq!(fs::read(&a).unwrap(), b"changed");
    assert_eq!(
        fs::read(tmp.path().join("tree/a (1).txt")).unwrap(),
        b"hello"
    );
    run(Overwrite::Replace);
    assert_eq!(fs::read(&a).unwrap(), b"hello");
}

#[test]
fn extracts_selection_only() {
    let tmp = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let opts = ExtractOptions {
        only: Some(vec!["tree/sub".into()]),
        ..Default::default()
    };
    hdec_core::extract(
        &fixture("infozip.zip"),
        tmp.path(),
        &opts,
        &ctx(None, &cancel),
    )
    .unwrap();
    assert!(tmp.path().join("tree/sub/b.txt").exists());
    assert!(!tmp.path().join("tree/a.txt").exists());
}

#[test]
fn cancel_leaves_nothing_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let src = sample_tree(tmp.path());
    let archive = tmp.path().join("out.zip");
    let cancel = AtomicBool::new(true);
    let opts = CreateOptions {
        format: Format::Zip,
        level: Level::Normal,
        zip_crypto: false,
    };
    let r = hdec_core::create(&[src], &archive, &opts, &ctx(None, &cancel));
    assert!(matches!(r, Err(Error::Cancelled)));
    let names: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["tree"]);
}

#[test]
fn test_detects_corruption() {
    let (tmp, _, archive) = create(Format::Zip, Level::Store, false, None);
    let mut bytes = fs::read(&archive).unwrap();
    let at = bytes.windows(5).position(|w| w == b"hello").unwrap();
    bytes[at] = b'j';
    let bad = tmp.path().join("bad.zip");
    fs::write(&bad, bytes).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(hdec_core::test(&bad, &ctx(None, &cancel)).is_err());
}
