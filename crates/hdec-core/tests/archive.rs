use hdec_core::{CreateOptions, Ctx, Error, ExtractOptions, Format, Level, Overwrite};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

fn noop(_: u64, _: u64, _: &str) {}

/// `cancel` is copied: set it before building the context.
fn ctx<'a>(password: Option<&'a str>, cancel: &AtomicBool) -> Ctx<'a> {
    Ctx {
        password,
        cancel: std::sync::Arc::new(AtomicBool::new(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
        )),
        progress: std::sync::Arc::new(noop),
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
        split: None,
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
        split: None,
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
        // Windows skips symlinks (with a warning); nothing else may warn.
        assert!(
            extract(&fixture(name), tmp.path(), None)
                .unwrap()
                .iter()
                .all(|w| !cfg!(unix) && w.contains("symlink")),
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
            split: None,
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
        split: None,
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

#[test]
fn split_volumes_round_trip() {
    for format in [Format::Zip, Format::SevenZ, Format::TarGz] {
        let tmp = tempfile::tempdir().unwrap();
        let src = sample_tree(tmp.path());
        let archive = tmp.path().join(format!("out.{}", format.extension()));
        let cancel = AtomicBool::new(false);
        let opts = CreateOptions {
            format,
            level: Level::Store,
            split: Some(1_000_000),
            ..Default::default()
        };
        hdec_core::create(
            std::slice::from_ref(&src),
            &archive,
            &opts,
            &ctx(None, &cancel),
        )
        .unwrap();
        assert!(!archive.exists());
        let part = |n: u32| {
            tmp.path()
                .join(format!("out.{}.{n:03}", format.extension()))
        };
        assert!(part(3).exists() && part(4).exists(), "{format:?}");
        assert!(fs::metadata(part(1)).unwrap().len() == 1_000_000);
        // Any part opens the whole set.
        let out = tmp.path().join("x");
        extract(&part(2), &out, None).unwrap();
        assert_eq!(snapshot(&out.join("tree")), snapshot(&src), "{format:?}");
        assert!(!hdec_core::modifiable(&part(1)));
    }
}

#[test]
fn modify_add_delete_rename() {
    for format in [
        Format::Zip,
        Format::SevenZ,
        Format::Rar,
        Format::Tar,
        Format::TarXz,
    ] {
        let (tmp, _, archive) = create(format, Level::Normal, false, None);
        assert!(hdec_core::modifiable(&archive));
        let cancel = AtomicBool::new(false);
        let c = ctx(None, &cancel);
        let new = tmp.path().join("new.txt");
        fs::write(&new, "new").unwrap();
        let names = || -> Vec<String> {
            hdec_core::list(&archive, &c)
                .unwrap()
                .into_iter()
                .map(|e| e.path)
                .collect()
        };

        hdec_core::modify(
            &archive,
            &hdec_core::Edit::Add {
                sources: vec![new.clone()],
                into: "tree/sub".into(),
            },
            &c,
        )
        .unwrap();
        assert!(
            names().contains(&"tree/sub/new.txt".to_string()),
            "{format:?}"
        );
        fs::write(&new, "newer").unwrap();
        hdec_core::modify(
            &archive,
            &hdec_core::Edit::Add {
                sources: vec![new.clone()],
                into: "tree/sub".into(),
            },
            &c,
        )
        .unwrap();
        assert_eq!(
            names().iter().filter(|n| n.ends_with("new.txt")).count(),
            1,
            "{format:?}"
        );

        hdec_core::modify(
            &archive,
            &hdec_core::Edit::Delete(vec!["tree/sub/deeper".into(), "tree/a.txt".into()]),
            &c,
        )
        .unwrap();
        let after = names();
        assert!(
            !after
                .iter()
                .any(|n| n.starts_with("tree/sub/deeper") || n == "tree/a.txt"),
            "{format:?} {after:?}"
        );

        hdec_core::modify(
            &archive,
            &hdec_core::Edit::Rename(vec![("tree/sub".into(), "tree/moved".into())]),
            &c,
        )
        .unwrap();
        let out = tmp.path().join("x");
        extract(&archive, &out, None).unwrap();
        assert_eq!(
            fs::read(out.join("tree/moved/new.txt")).unwrap(),
            b"newer",
            "{format:?}"
        );
        assert_eq!(
            fs::read(out.join("tree/moved/ünïcødé ñame.txt")).unwrap(),
            b"unicode",
            "{format:?}"
        );
        assert!(!out.join("tree/sub").exists());
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("hdec"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
fn zip_modify_keeps_encrypted_entries_raw() {
    let (tmp, _, archive) = create(Format::Zip, Level::Normal, false, Some("pw"));
    let cancel = AtomicBool::new(false);
    // No password needed to delete: entries are copied without decrypting.
    hdec_core::modify(
        &archive,
        &hdec_core::Edit::Delete(vec!["tree/zero.bin".into()]),
        &ctx(None, &cancel),
    )
    .unwrap();
    let out = tmp.path().join("x");
    extract(&archive, &out, Some("pw")).unwrap();
    assert_eq!(fs::read(out.join("tree/a.txt")).unwrap(), b"hello");
    assert!(!out.join("tree/zero.bin").exists());
}

#[test]
fn sevenz_modify_keeps_password() {
    let (tmp, _, archive) = create(Format::SevenZ, Level::Normal, false, Some("pw"));
    let cancel = AtomicBool::new(false);
    hdec_core::modify(
        &archive,
        &hdec_core::Edit::Delete(vec!["tree/zero.bin".into()]),
        &ctx(Some("pw"), &cancel),
    )
    .unwrap();
    assert!(matches!(
        hdec_core::list(&archive, &ctx(None, &cancel)),
        Err(Error::PasswordRequired)
    ));
    let out = tmp.path().join("x");
    extract(&archive, &out, Some("pw")).unwrap();
    assert!(out.join("tree/a.txt").exists() && !out.join("tree/zero.bin").exists());
}

fn big_pattern() -> Vec<u8> {
    (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn check_rar_tree(root: &Path, name: &str) {
    assert_eq!(
        fs::read(root.join("tree/a.txt")).unwrap(),
        b"hello",
        "{name}"
    );
    assert_eq!(
        fs::read(root.join("tree/sub/b.txt")).unwrap(),
        b"world",
        "{name}"
    );
    assert_eq!(
        fs::read(root.join("tree/sub/ünï.txt")).unwrap(),
        b"unicode",
        "{name}"
    );
    assert_eq!(
        fs::read(root.join("tree/big.bin")).unwrap(),
        big_pattern(),
        "{name}"
    );
    assert!(root.join("tree/empty").is_dir(), "{name}");
}

#[test]
fn reads_rar5_with_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("d");
    let warnings = extract(&fixture("rar5.rar"), &dest, None).unwrap();
    check_rar_tree(&dest, "rar5");
    // tree/evil escapes; on Windows tree/link is skipped too.
    assert_eq!(
        warnings.len(),
        if cfg!(unix) { 1 } else { 2 },
        "{warnings:?}"
    );
    assert!(!tmp.path().join("outside").exists());
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(dest.join("tree/link")).unwrap(),
        Path::new("a.txt")
    );
    let got = fs::metadata(dest.join("tree/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        got,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000)
    );
    let cancel = AtomicBool::new(false);
    let entries = hdec_core::list(&fixture("rar5.rar"), &ctx(None, &cancel)).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.path == "tree/sub/ünï.txt" && e.size == 7)
    );
    hdec_core::test(&fixture("rar5.rar"), &ctx(None, &cancel)).unwrap();
}

#[test]
fn reads_rar4() {
    let tmp = tempfile::tempdir().unwrap();
    extract(&fixture("rar4.rar"), tmp.path(), None).unwrap();
    check_rar_tree(tmp.path(), "rar4");
}

#[test]
fn rar_passwords() {
    for name in [
        "rar5_solid_pw.rar",
        "rar5_headers_pw.rar",
        "rar4_solid_pw.rar",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let archive = fixture(name);
        let r = extract(&archive, &tmp.path().join("a"), None);
        assert!(matches!(r, Err(Error::PasswordRequired)), "{name}: {r:?}");
        let r = extract(&archive, &tmp.path().join("b"), Some("wrong"));
        assert!(matches!(r, Err(Error::WrongPassword)), "{name}: {r:?}");
        extract(&archive, &tmp.path().join("c"), Some("secret")).unwrap();
        check_rar_tree(&tmp.path().join("c"), name);
        if tmp.path().join("b").exists() {
            let leftovers = snapshot(&tmp.path().join("b"));
            assert!(
                leftovers.keys().all(|k| !k.contains("hdec-tmp")),
                "{name}: {leftovers:?}"
            );
        }
    }
    let cancel = AtomicBool::new(false);
    let r = hdec_core::list(&fixture("rar5_headers_pw.rar"), &ctx(None, &cancel));
    assert!(matches!(r, Err(Error::PasswordRequired)), "{r:?}");
}

#[test]
fn reads_rar_volume_sets() {
    let tmp = tempfile::tempdir().unwrap();
    let want: Vec<u8> = (0..30_000u32).map(|i| (i % 251) as u8).collect();
    for part in ["rar5_vol.part1.rar", "rar5_vol.part3.rar"] {
        let out = tmp.path().join(part);
        extract(&fixture(part), &out, None).unwrap();
        assert_eq!(fs::read(out.join("spanned.bin")).unwrap(), want, "{part}");
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"hello", "{part}");
    }
    let cancel = AtomicBool::new(false);
    let entries = hdec_core::list(&fixture("rar5_vol.part2.rar"), &ctx(None, &cancel)).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["a.txt", "spanned.bin"]
    );
}

#[test]
fn rar_selection_and_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let opts = ExtractOptions {
        only: Some(vec!["tree/sub".into()]),
        ..Default::default()
    };
    hdec_core::extract(
        &fixture("rar5_solid_pw.rar"),
        tmp.path(),
        &opts,
        &ctx(Some("secret"), &cancel),
    )
    .unwrap();
    assert!(tmp.path().join("tree/sub/b.txt").exists());
    assert!(!tmp.path().join("tree/a.txt").exists());

    let mut bytes = fs::read(fixture("rar4.rar")).unwrap();
    let n = bytes.len();
    bytes[n / 2] ^= 0xFF;
    let bad = tmp.path().join("bad.rar");
    fs::write(&bad, bytes).unwrap();
    assert!(hdec_core::test(&bad, &ctx(None, &cancel)).is_err());
}

/// A fake Windows stub: "MZ", junk containing a bare 7z signature (no valid header), then the archive.
fn fake_sfx(archive: &[u8]) -> Vec<u8> {
    let mut exe = b"MZ".to_vec();
    exe.extend((0..50_000u32).map(|i| (i * 7) as u8));
    exe.extend_from_slice(b"7z\xBC\xAF\x27\x1C\x00\x04junkjunkjunkjunkjunkjunk");
    exe.extend((0..1000u32).map(|i| (i * 13) as u8));
    exe.extend_from_slice(archive);
    exe
}

#[test]
fn reads_self_extracting_exes() {
    let tmp = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let mut cases = Vec::new();
    for format in [Format::SevenZ, Format::Zip] {
        let (_t, _, archive) = create(format, Level::Normal, false, None);
        cases.push((format, fs::read(archive).unwrap()));
    }
    cases.push((Format::Rar, fs::read(fixture("rar5.rar")).unwrap()));
    for (format, bytes) in cases {
        let exe = tmp.path().join(format!("setup_{}.exe", format.extension()));
        fs::write(&exe, fake_sfx(&bytes)).unwrap();
        assert_eq!(hdec_core::detect(&exe).unwrap(), format);
        assert!(!hdec_core::modifiable(&exe));
        let out = tmp.path().join(format.extension());
        extract(&exe, &out, None).unwrap();
        assert_eq!(
            fs::read(out.join("tree/a.txt")).unwrap(),
            b"hello",
            "{format:?}"
        );
        hdec_core::test(&exe, &ctx(None, &cancel)).unwrap();
    }
    let plain = tmp.path().join("plain.exe");
    fs::write(&plain, b"MZ just a program").unwrap();
    assert!(hdec_core::detect(&plain).is_err());
}

#[test]
fn reads_lzip() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let lz = |data: &[u8]| {
        let mut w =
            lzma_rust2::LzipWriter::new(Vec::new(), lzma_rust2::LzipOptions::with_preset(6));
        w.write_all(data).unwrap();
        w.finish().unwrap()
    };
    fs::write(tmp.path().join("note.txt.lz"), lz(b"hello lzip")).unwrap();
    let (_t, _, tar) = create(Format::Tar, Level::Normal, false, None);
    fs::write(tmp.path().join("t.tar.lz"), lz(&fs::read(tar).unwrap())).unwrap();

    assert_eq!(
        hdec_core::detect(&tmp.path().join("t.tar.lz")).unwrap(),
        Format::TarLz
    );
    extract(&tmp.path().join("note.txt.lz"), &tmp.path().join("a"), None).unwrap();
    assert_eq!(
        fs::read(tmp.path().join("a/note.txt")).unwrap(),
        b"hello lzip"
    );
    extract(&tmp.path().join("t.tar.lz"), &tmp.path().join("b"), None).unwrap();
    assert_eq!(fs::read(tmp.path().join("b/tree/a.txt")).unwrap(), b"hello");
}

#[test]
fn lists_symlinks() {
    let cancel = AtomicBool::new(false);
    for format in [Format::Zip, Format::Tar, Format::SevenZ] {
        let (_tmp, _, archive) = create(format, Level::Normal, false, None);
        let entries = hdec_core::list(&archive, &ctx(None, &cancel)).unwrap();
        let link = entries.iter().find(|e| e.path == "tree/link");
        let file = entries.iter().find(|e| e.path == "tree/a.txt").unwrap();
        assert_eq!(file.link, None, "{format:?}");
        if cfg!(unix) {
            // 7z keeps the target in solid compressed data, so only the flag is known.
            let want = if format == Format::SevenZ {
                ""
            } else {
                "a.txt"
            };
            assert_eq!(link.unwrap().link.as_deref(), Some(want), "{format:?}");
        }
    }
    let entries = hdec_core::list(&fixture("rar5.rar"), &ctx(None, &cancel)).unwrap();
    let link = entries.iter().find(|e| e.path == "tree/link").unwrap();
    assert_eq!(link.link.as_deref(), Some("a.txt"));
}

#[test]
fn modify_new_folders_and_moves() {
    use hdec_core::Edit;
    for format in [Format::Zip, Format::SevenZ, Format::TarGz] {
        let (tmp, _, archive) = create(format, Level::Normal, false, None);
        let cancel = AtomicBool::new(false);
        let c = ctx(None, &cancel);
        let names = || -> Vec<String> {
            hdec_core::list(&archive, &c)
                .unwrap()
                .into_iter()
                .map(|e| e.path)
                .collect()
        };

        hdec_core::modify(&archive, &Edit::CreateDir("tree/new dir".into()), &c).unwrap();
        assert!(names().contains(&"tree/new dir".to_string()), "{format:?}");

        let moves = vec![
            ("tree/a.txt".into(), "tree/new dir/a.txt".into()),
            ("tree/sub".into(), "tree/new dir/sub".into()),
        ];
        hdec_core::modify(&archive, &Edit::Rename(moves), &c).unwrap();
        let out = tmp.path().join("x");
        extract(&archive, &out, None).unwrap();
        assert_eq!(
            fs::read(out.join("tree/new dir/a.txt")).unwrap(),
            b"hello",
            "{format:?}"
        );
        assert_eq!(
            fs::read(out.join("tree/new dir/sub/ünïcødé ñame.txt")).unwrap(),
            b"unicode",
            "{format:?}"
        );
        assert!(
            !out.join("tree/a.txt").exists() && !out.join("tree/sub").exists(),
            "{format:?}"
        );

        let refused = [
            Edit::CreateDir("tree/new dir".into()),
            Edit::CreateDir("../escape".into()),
            Edit::Rename(vec![("tree/zero.bin".into(), "tree/empty".into())]),
            Edit::Rename(vec![("tree/new dir".into(), "tree/new dir/inner".into())]),
            Edit::Rename(vec![("tree/missing".into(), "tree/x".into())]),
        ];
        for edit in refused {
            let r = hdec_core::modify(&archive, &edit, &c);
            assert!(
                matches!(r, Err(Error::Unsupported(_))),
                "{format:?} {edit:?}: {r:?}"
            );
        }
    }
}

#[test]
fn modifies_info_zip_archives() {
    // Info-ZIP writes Unix extra fields (0x7875) that must survive raw copying.
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("t.zip");
    fs::copy(fixture("infozip.zip"), &archive).unwrap();
    let cancel = AtomicBool::new(false);
    let edit = hdec_core::Edit::Rename(vec![("tree/a.txt".into(), "tree/sub/a.txt".into())]);
    hdec_core::modify(&archive, &edit, &ctx(None, &cancel)).unwrap();
    extract(&archive, &tmp.path().join("x"), None).unwrap();
    assert_eq!(
        fs::read(tmp.path().join("x/tree/sub/a.txt")).unwrap(),
        b"hello"
    );
}

#[test]
fn modify_adds_folder_trees() {
    for format in [
        Format::Zip,
        Format::SevenZ,
        Format::Rar,
        Format::Tar,
        Format::TarXz,
    ] {
        let (tmp, _, archive) = create(format, Level::Normal, false, None);
        let extra = tmp.path().join("extra");
        fs::create_dir_all(extra.join("one/two/empty")).unwrap();
        fs::write(extra.join("top.txt"), "top").unwrap();
        fs::write(extra.join("one/two/deep.txt"), "deep").unwrap();
        let cancel = AtomicBool::new(false);
        let c = ctx(None, &cancel);
        hdec_core::modify(
            &archive,
            &hdec_core::Edit::Add {
                sources: vec![extra.clone()],
                into: "tree/sub".into(),
            },
            &c,
        )
        .unwrap();
        let out = tmp.path().join("x");
        let warnings = extract(&archive, &out, None).unwrap();
        assert!(
            warnings.iter().all(|w| w.contains("link")),
            "{format:?}: {warnings:?}"
        );
        let got = out.join("tree/sub/extra");
        assert_eq!(fs::read(got.join("top.txt")).unwrap(), b"top", "{format:?}");
        assert_eq!(fs::read(got.join("one/two/deep.txt")).unwrap(), b"deep");
        assert!(got.join("one/two/empty").is_dir(), "{format:?}");
        assert!(out.join("tree/sub/deeper/big.bin").is_file(), "{format:?}");
    }
}

#[test]
fn rar_round_trips() {
    for level in [Level::Store, Level::Fast, Level::Normal, Level::Max] {
        let (tmp, src, archive) = create(Format::Rar, level, false, None);
        let out = tmp.path().join("x");
        assert!(extract(&archive, &out, None).unwrap().is_empty());
        assert_eq!(snapshot(&out.join("tree")), snapshot(&src), "{level:?}");
        let cancel = AtomicBool::new(false);
        hdec_core::test(&archive, &ctx(None, &cancel)).unwrap();
        let entries = hdec_core::list(&archive, &ctx(None, &cancel)).unwrap();
        assert!(entries.iter().any(|e| e.path == "tree/empty" && e.is_dir));
        assert!(hdec_core::modifiable(&archive));
    }
}

#[test]
fn rar_keeps_modification_time() {
    let (tmp, _, archive) = create(Format::Rar, Level::Normal, false, None);
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
fn rar_passwords_encrypt_names_too() {
    let (tmp, src, archive) = create(Format::Rar, Level::Normal, false, Some("pässword"));
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        hdec_core::list(&archive, &ctx(None, &cancel)),
        Err(Error::PasswordRequired)
    ));
    assert!(matches!(
        hdec_core::list(&archive, &ctx(Some("nope"), &cancel)),
        Err(Error::WrongPassword)
    ));
    let out = tmp.path().join("y");
    extract(&archive, &out, Some("pässword")).unwrap();
    assert_eq!(snapshot(&out.join("tree")), snapshot(&src));
    assert!(
        !fs::read(&archive)
            .unwrap()
            .windows(5)
            .any(|w| w == b"a.txt")
    );

    // Edits keep the password.
    let c = ctx(Some("pässword"), &cancel);
    hdec_core::modify(
        &archive,
        &hdec_core::Edit::Delete(vec!["tree/a.txt".into()]),
        &c,
    )
    .unwrap();
    assert!(matches!(
        hdec_core::list(&archive, &ctx(None, &cancel)),
        Err(Error::PasswordRequired)
    ));
    let names: Vec<_> = hdec_core::list(&archive, &c)
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert!(!names.contains(&"tree/a.txt".to_string()) && names.contains(&"tree/sub".to_string()));
}

#[test]
fn edits_existing_rar_files() {
    let cancel = AtomicBool::new(false);
    for (name, password) in [
        ("rar4.rar", None),
        ("rar5_solid_pw.rar", Some("secret")),
        ("rar5_headers_pw.rar", Some("secret")),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join(name);
        fs::copy(fixture(name), &archive).unwrap();
        assert!(hdec_core::modifiable(&archive), "{name}");
        let c = ctx(password, &cancel);
        hdec_core::modify(
            &archive,
            &hdec_core::Edit::Rename(vec![("tree/a.txt".into(), "tree/renamed.txt".into())]),
            &c,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let out = tmp.path().join("x");
        extract(&archive, &out, password).unwrap();
        assert_eq!(
            fs::read(out.join("tree/renamed.txt")).unwrap(),
            b"hello",
            "{name}"
        );
        assert_eq!(
            fs::read(out.join("tree/sub/b.txt")).unwrap(),
            b"world",
            "{name}"
        );
        assert!(!out.join("tree/a.txt").exists(), "{name}");
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("hdec"))
            .collect();
        assert!(leftovers.is_empty(), "{name}: {leftovers:?}");
    }
    assert!(!hdec_core::modifiable(&fixture("rar5_vol.part1.rar")));

    // rar5.rar holds a symlink escaping the archive: edits are refused, file untouched.
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("rar5.rar");
    fs::copy(fixture("rar5.rar"), &archive).unwrap();
    let r = hdec_core::modify(
        &archive,
        &hdec_core::Edit::Delete(vec!["tree/a.txt".into()]),
        &ctx(None, &cancel),
    );
    assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
    assert_eq!(
        fs::read(&archive).unwrap(),
        fs::read(fixture("rar5.rar")).unwrap()
    );
}

#[test]
fn duplicate_names_follow_the_overwrite_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("dup.tar");
    let mut b = tar::Builder::new(fs::File::create(&archive).unwrap());
    let mut add = |name: &str, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, name, data).unwrap();
    };
    add("d/x.txt", b"one");
    // Enough files in between that the writer threads are busy.
    for i in 0..200 {
        add(&format!("d/f{i}.txt"), b"filler");
    }
    add("d/x.txt", b"two");
    b.into_inner().unwrap();

    let cancel = AtomicBool::new(false);
    for (overwrite, x, renamed) in [
        (Overwrite::Skip, "one", None),
        (Overwrite::Replace, "two", None),
        (Overwrite::Rename, "one", Some("two")),
    ] {
        let out = tmp.path().join(format!("{overwrite:?}"));
        let opts = ExtractOptions {
            only: None,
            overwrite,
        };
        hdec_core::extract(&archive, &out, &opts, &ctx(None, &cancel)).unwrap();
        assert_eq!(
            fs::read(out.join("d/x.txt")).unwrap(),
            x.as_bytes(),
            "{overwrite:?}"
        );
        let second = fs::read(out.join("d/x (1).txt")).ok();
        assert_eq!(
            second.as_deref(),
            renamed.map(str::as_bytes),
            "{overwrite:?}"
        );
        assert_eq!(
            fs::read_dir(out.join("d")).unwrap().count(),
            201 + renamed.iter().count()
        );
    }
}

#[test]
fn zip_big_files_deflate_in_parallel() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big");
    fs::create_dir(&src).unwrap();
    // Over the parallel-entry limit, compressible, and not chunk-aligned.
    let data: Vec<u8> = (0..20_000_003u32)
        .map(|i| ((i % 1009) ^ (i >> 11)) as u8)
        .collect();
    fs::write(src.join("data.bin"), &data).unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_400_000_000);
    let f = fs::File::options()
        .write(true)
        .open(src.join("data.bin"))
        .unwrap();
    f.set_modified(old).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o750))
            .unwrap();
    }
    drop(f);
    let cancel = AtomicBool::new(false);
    let c = ctx(None, &cancel);
    for level in [Level::Fast, Level::Normal] {
        let archive = tmp.path().join(format!("{level:?}.zip"));
        let opts = CreateOptions {
            format: Format::Zip,
            level,
            ..Default::default()
        };
        hdec_core::create(std::slice::from_ref(&src), &archive, &opts, &c).unwrap();
        assert!(fs::metadata(&archive).unwrap().len() < data.len() as u64 / 2);
        hdec_core::test(&archive, &c).unwrap();
        let out = tmp.path().join(format!("out-{level:?}"));
        extract(&archive, &out, None).unwrap();
        let got = out.join("big/data.bin");
        assert!(fs::read(&got).unwrap() == data);
        let meta = fs::metadata(&got).unwrap();
        let drift = meta
            .modified()
            .unwrap()
            .duration_since(old)
            .unwrap_or_default();
        assert!(drift < Duration::from_secs(3));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o750);
        }
    }
}

#[test]
fn bzip2_streams_round_trip_in_parallel() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("data.bin");
    // Several 900 kB streams, plus a run that expands well past its packed size.
    let mut data: Vec<u8> = (0..3_000_000u32)
        .map(|i| ((i % 251) ^ (i >> 9)) as u8)
        .collect();
    data.extend(std::iter::repeat_n(7u8, 2_000_000));
    fs::write(&src, &data).unwrap();
    let cancel = AtomicBool::new(false);
    let c = ctx(None, &cancel);
    let archive = tmp.path().join("data.bin.bz2");
    let opts = CreateOptions {
        format: Format::Bz2,
        level: Level::Max,
        ..Default::default()
    };
    hdec_core::create(std::slice::from_ref(&src), &archive, &opts, &c).unwrap();
    let packed = fs::read(&archive).unwrap();
    assert!(packed.windows(4).filter(|w| w == b"BZh9").count() >= 5);
    let out = tmp.path().join("out");
    extract(&archive, &out, None).unwrap();
    assert!(fs::read(out.join("data.bin")).unwrap() == data);

    // A damaged stream in the middle fails the whole extraction.
    let mut bad = packed.clone();
    let second = bad.windows(4).rposition(|w| w == b"BZh9").unwrap();
    bad[second + 20] ^= 0xFF;
    let bad_archive = tmp.path().join("bad.bin.bz2");
    fs::write(&bad_archive, bad).unwrap();
    assert!(hdec_core::test(&bad_archive, &c).is_err());
}

#[test]
fn colliding_names_dont_abort_extraction() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("collide.tar");
    let mut b = tar::Builder::new(fs::File::create(&archive).unwrap());
    let mut add = |name: &str, data: &[u8], kind: tar::EntryType| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_size(data.len() as u64);
        h.set_mode(0o755);
        b.append_data(&mut h, name, data).unwrap();
    };
    // Same name on case-insensitive file systems (macOS, Windows).
    add("d/Makefile", b"upper", tar::EntryType::Regular);
    add("d/makefile", b"lower", tar::EntryType::Regular);
    // A file named like a folder made earlier.
    add("d/sub/", b"", tar::EntryType::Directory);
    add("d/sub", b"file", tar::EntryType::Regular);
    b.into_inner().unwrap();
    let cancel = AtomicBool::new(false);
    for overwrite in [Overwrite::Skip, Overwrite::Replace, Overwrite::Rename] {
        let out = tmp.path().join(format!("{overwrite:?}"));
        let opts = ExtractOptions {
            only: None,
            overwrite,
        };
        let result = hdec_core::extract(&archive, &out, &opts, &ctx(None, &cancel));
        assert!(result.is_ok(), "{overwrite:?}: {result:?}");
        assert!(out.join("d/sub").is_dir(), "{overwrite:?}");
        assert!(fs::read(out.join("d/Makefile")).is_ok(), "{overwrite:?}");
    }
}

#[test]
fn empty_inputs_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("empty.txt");
    fs::write(&file, "").unwrap();
    let dir = tmp.path().join("nothing");
    fs::create_dir(&dir).unwrap();
    let cancel = AtomicBool::new(false);
    let c = ctx(None, &cancel);
    let cases = [
        (Format::Gz, &file),
        (Format::Bz2, &file),
        (Format::Xz, &file),
        (Format::Zip, &dir),
        (Format::SevenZ, &dir),
        (Format::TarGz, &dir),
        (Format::TarBz2, &dir),
        (Format::TarXz, &dir),
        (Format::Rar, &dir),
    ];
    for (format, src) in cases {
        for level in [Level::Store, Level::Normal] {
            // Single-file streams extract to the archive's name minus the extension.
            let folder = tmp.path().join(format!("{level:?}"));
            fs::create_dir_all(&folder).unwrap();
            let stem = src.file_name().unwrap().to_string_lossy();
            let archive = folder.join(format!("{stem}.{}", format.extension()));
            let opts = CreateOptions {
                format,
                level,
                ..Default::default()
            };
            hdec_core::create(std::slice::from_ref(src), &archive, &opts, &c).unwrap();
            hdec_core::test(&archive, &c).unwrap();
            let out = tmp
                .path()
                .join(format!("out-{level:?}-{}", format.extension()));
            extract(&archive, &out, None).unwrap();
            let name = src.file_name().unwrap();
            let got = fs::symlink_metadata(out.join(name));
            assert!(got.is_ok(), "{format:?} {level:?}");
            if format.extension().contains("tar") || src == &dir {
                assert!(got.unwrap().is_dir(), "{format:?} {level:?}");
            } else {
                assert_eq!(
                    fs::read(out.join(name)).unwrap(),
                    b"",
                    "{format:?} {level:?}"
                );
            }
        }
    }
}
