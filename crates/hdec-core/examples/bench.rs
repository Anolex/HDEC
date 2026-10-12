//! Times create / test / extract for each format on a folder you choose:
//! `cargo run --release -p hdec-core --example bench -- <folder> [normal|fast|max|store] [zip 7z ...]`

use hdec_core::{CreateOptions, Ctx, ExtractOptions, Format, Level};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() -> hdec_core::Result<()> {
    let mut args = std::env::args().skip(1);
    let src = PathBuf::from(
        args.next()
            .expect("usage: bench <folder> [level] [formats]"),
    );
    let level_name = args.next().unwrap_or_else(|| "normal".into());
    let level = match level_name.as_str() {
        "store" => Level::Store,
        "fast" => Level::Fast,
        "max" => Level::Max,
        _ => Level::Normal,
    };
    let all = [
        Format::Zip,
        Format::SevenZ,
        Format::Rar,
        Format::Tar,
        Format::TarGz,
        Format::TarXz,
        Format::TarBz2,
    ];
    let wanted: Vec<String> = args.collect();
    let ctx = Ctx {
        password: None,
        cancel: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(|_, _, _| {}),
    };
    println!(
        "{:8} {:>9} {:>9} {:>9} {:>9}",
        "format", "create", "test", "extract", "size MB"
    );
    for format in all {
        if !wanted.is_empty() && !wanted.iter().any(|w| w == format.extension()) {
            continue;
        }
        let tmp = tempfile::tempdir()?;
        let archive = tmp.path().join(format!("a.{}", format.extension()));
        let opts = CreateOptions {
            format,
            level,
            ..Default::default()
        };
        let t = Instant::now();
        hdec_core::create(std::slice::from_ref(&src), &archive, &opts, &ctx)?;
        let create = t.elapsed().as_secs_f64();
        let t = Instant::now();
        hdec_core::test(&archive, &ctx)?;
        let test = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let out = tmp.path().join("out");
        hdec_core::extract(&archive, &out, &ExtractOptions::default(), &ctx)?;
        let extract = t.elapsed().as_secs_f64();
        let size = std::fs::metadata(&archive)?.len() as f64 / (1 << 20) as f64;
        println!(
            "{:8} {create:8.2}s {test:8.2}s {extract:8.2}s {size:9.1}",
            format.name()
        );
    }
    Ok(())
}
