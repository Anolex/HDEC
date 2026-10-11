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
    let archive = tmp.path().join(format!("out.{}", format.extension()));
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

const TAR_FORMATS: [Format; 4] = [Format::Tar, Format::TarGz, Format::TarXz, Format::TarBz2];

#[test]
fn tar_round_trips() {
    for format in TAR_FORMATS {
        for level in [Level::Store, Level::Max] {
            let (tmp, src, archive) = create(format, level, false, None);
            let out = tmp.path().join("x");
            let cancel = AtomicBool::new(false);
            let opts = ExtractOptions::default();
            assert!(
                hdec_core::extract(&archive, &out, &opts, &ctx(None, &cancel))
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                snapshot(&out.join("tree")),
                snapshot(&src),
                "{format:?} {level:?}"
            );
            hdec_core::test(&archive, &ctx(None, &cancel)).unwrap();
            let names: Vec<_> = hdec_core::list(&archive, &ctx(None, &cancel))
                .unwrap()
                .into_iter()
                .map(|e| e.path)
                .collect();
            assert!(
                names.contains(&"tree/sub/ünïcødé ñame.txt".to_string()),
                "{names:?}"
            );
        }
    }
}

#[test]
fn reads_system_tar_archives() {
    let long = format!("tree/{}/ünï.txt", "d".repeat(120));
    for name in [
        "bsdtar_pax.tar.gz",
        "gnutar.tar.zst",
        "bsdtar.tar.xz",
        "bsdtar.tar.bz2",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            extract(&fixture(name), tmp.path(), None)
                .unwrap()
                .is_empty(),
            "{name}"
        );
        assert_eq!(
            fs::read(tmp.path().join("tree/a.txt")).unwrap(),
            b"hello",
            "{name}"
        );
        assert_eq!(
            fs::read(tmp.path().join("tree/sub/b.txt")).unwrap(),
            b"world",
            "{name}"
        );
        assert_eq!(fs::read(tmp.path().join(&long)).unwrap(), b"long", "{name}");
        assert!(tmp.path().join("tree/empty").is_dir(), "{name}");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(tmp.path().join("tree/link")).unwrap(),
            Path::new("a.txt"),
            "{name}"
        );
    }
}

#[test]
fn tar_skips_malicious_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("a/b");
    let warnings = extract(&fixture("evil.tar"), &dest, None).unwrap();
    assert_eq!(fs::read(dest.join("ok.txt")).unwrap(), b"ok");
    assert!(!tmp.path().join("a/evil.txt").exists());
    assert!(!dest.join("abs.txt").exists());
    assert!(!tmp.path().join("outside").exists());
    assert!(!fs::symlink_metadata(dest.join("link")).is_ok_and(|m| m.is_symlink()));
    assert!(!dest.join("hard").exists());
    assert!(!dest.join("fifo").exists());
    assert_eq!(warnings.len(), 5, "{warnings:?}"); // ../evil, /abs, link, hard, fifo
}

#[test]
fn reads_compressed_streams() {
    for (name, inner, want) in [
        ("note.txt.gz", "note.txt", "hello stream"),
        ("note.txt.xz", "note.txt", "hello stream"),
        ("note.txt.bz2", "note.txt", "hello stream"),
        ("note.txt.lzma", "note.txt", "hello stream"),
        ("multi_member.gz", "multi_member", "part1 part2"),
        ("multi_frame.zst", "multi_frame", "hello stream more"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        extract(&fixture(name), tmp.path(), None).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join(inner)).unwrap(),
            want,
            "{name}"
        );
        let cancel = AtomicBool::new(false);
        hdec_core::test(&fixture(name), &ctx(None, &cancel)).unwrap();
    }
}

#[test]
fn stream_round_trips_single_file() {
    for format in [Format::Gz, Format::Xz, Format::Bz2] {
        let tmp = tempfile::tempdir().unwrap();
        let src = sample_tree(tmp.path()).join("sub/deeper/big.bin");
        let archive = tmp.path().join(format!("big.bin.{}", format.extension()));
        let cancel = AtomicBool::new(false);
        let opts = CreateOptions {
            format,
            level: Level::Normal,
            zip_crypto: false,
        };
        hdec_core::create(
            std::slice::from_ref(&src),
            &archive,
            &opts,
            &ctx(None, &cancel),
        )
        .unwrap();
        let out = tmp.path().join("x");
        extract(&archive, &out, None).unwrap();
        assert_eq!(
            fs::read(out.join("big.bin")).unwrap(),
            fs::read(&src).unwrap(),
            "{format:?}"
        );

        let dir = tmp.path().join("tree");
        let r = hdec_core::create(&[dir], &tmp.path().join("d.gz"), &opts, &ctx(None, &cancel));
        assert!(matches!(r, Err(Error::Unsupported(_))), "{format:?}");
    }
}

#[test]
fn refuses_password_for_unencryptable_formats() {
    let tmp = tempfile::tempdir().unwrap();
    let src = sample_tree(tmp.path());
    let cancel = AtomicBool::new(false);
    let opts = CreateOptions {
        format: Format::TarGz,
        level: Level::Normal,
        zip_crypto: false,
    };
    let r = hdec_core::create(
        &[src],
        &tmp.path().join("x.tar.gz"),
        &opts,
        &ctx(Some("pw"), &cancel),
    );
    assert!(matches!(r, Err(Error::Unsupported(_))));
}

#[test]
fn detects_corrupt_streams() {
    let tmp = tempfile::tempdir().unwrap();
    for name in [
        "note.txt.gz",
        "note.txt.xz",
        "note.txt.bz2",
        "multi_frame.zst",
    ] {
        let mut bytes = fs::read(fixture(name)).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x55;
        let bad = tmp.path().join(name);
        fs::write(&bad, bytes).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(
            hdec_core::test(&bad, &ctx(None, &cancel)).is_err(),
            "{name}"
        );
    }
}

#[test]
fn sevenz_round_trips() {
    for level in [Level::Store, Level::Fast, Level::Normal, Level::Max] {
        let (tmp, src, archive) = create(Format::SevenZ, level, false, None);
        let out = tmp.path().join("x");
        assert!(extract(&archive, &out, None).unwrap().is_empty());
        assert_eq!(snapshot(&out.join("tree")), snapshot(&src), "{level:?}");
        let cancel = AtomicBool::new(false);
        hdec_core::test(&archive, &ctx(None, &cancel)).unwrap();
        let entries = hdec_core::list(&archive, &ctx(None, &cancel)).unwrap();
        assert!(entries.iter().any(|e| e.path == "tree/empty" && e.is_dir));
        assert!(entries.iter().all(|e| !e.encrypted));
    }
}

#[test]
fn sevenz_keeps_modification_time() {
    let (tmp, _, archive) = create(Format::SevenZ, Level::Normal, false, None);
    let out = tmp.path().join("x");
    extract(&archive, &out, None).unwrap();
    let got = fs::metadata(out.join("tree/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        got,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000)
    );
}

#[test]
fn sevenz_passwords_encrypt_names_too() {
    let (tmp, src, archive) = create(Format::SevenZ, Level::Normal, false, Some("pässword"));
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        hdec_core::list(&archive, &ctx(None, &cancel)),
        Err(Error::PasswordRequired)
    ));
    assert!(matches!(
        hdec_core::list(&archive, &ctx(Some("nope"), &cancel)),
        Err(Error::WrongPassword)
    ));
    assert!(matches!(
        extract(&archive, &tmp.path().join("x"), Some("nope")),
        Err(Error::WrongPassword)
    ));
    let entries = hdec_core::list(&archive, &ctx(Some("pässword"), &cancel)).unwrap();
    assert!(
        entries
            .iter()
            .filter(|e| !e.is_dir && e.size > 0)
            .all(|e| e.encrypted)
    );
    let out = tmp.path().join("y");
    extract(&archive, &out, Some("pässword")).unwrap();
    assert_eq!(snapshot(&out.join("tree")), snapshot(&src));
    assert!(
        !fs::read(&archive)
            .unwrap()
            .windows(5)
            .any(|w| w == b"a.txt")
    );
}

#[test]
fn sevenz_extracts_selection_from_solid_block() {
    let (tmp, _, archive) = create(Format::SevenZ, Level::Normal, false, None);
    let cancel = AtomicBool::new(false);
    let opts = ExtractOptions {
        only: Some(vec!["tree/sub/deeper".into()]),
        ..Default::default()
    };
    let out = tmp.path().join("x");
    hdec_core::extract(&archive, &out, &opts, &ctx(None, &cancel)).unwrap();
    assert_eq!(
        fs::read(out.join("tree/sub/deeper/big.bin")).unwrap(),
        fs::read(tmp.path().join("tree/sub/deeper/big.bin")).unwrap()
    );
    assert!(!out.join("tree/a.txt").exists());
}

#[test]
fn reads_bsdtar_7z_codecs() {
    for codec in ["lzma1", "lzma2", "bzip2", "deflate", "ppmd", "store"] {
        let tmp = tempfile::tempdir().unwrap();
        let archive = fixture(&format!("bsdtar_{codec}.7z"));
        assert!(
            extract(&archive, tmp.path(), None).unwrap().is_empty(),
            "{codec}"
        );
        assert_eq!(
            fs::read(tmp.path().join("tree/a.txt")).unwrap(),
            b"hello",
            "{codec}"
        );
        assert_eq!(
            fs::read(tmp.path().join("tree/sub/b.txt")).unwrap(),
            b"world",
            "{codec}"
        );
        assert!(tmp.path().join("tree/empty").is_dir(), "{codec}");
    }
}
