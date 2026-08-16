use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use linyaps_repository::{ErofsCompression, build_erofs_image_with_compression};

const APP_ID: &str = "cn.org.linyaps.builder.utils";

#[derive(Debug, Parser)]
#[command(name = "ll-builder-export")]
struct Cli {
    #[arg(long = "get-header", value_name = "TARGET_PATH")]
    header: Option<PathBuf>,
    #[arg(long = "get-loader", value_name = "TARGET_PATH")]
    loader: Option<PathBuf>,
    #[arg(long = "get-box", value_name = "TARGET_PATH")]
    box_binary: Option<PathBuf>,
    #[arg(long, value_name = "DIR:OUTPUT_PATH")]
    packdir: Option<String>,
    #[arg(short = 'z', value_name = "COMPRESSOR", requires = "packdir")]
    compressor: Option<String>,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run(options: Cli) -> Result<()> {
    println!("{}", env::current_dir()?.display());
    let files = env::var_os("LINGLONG_BUILDER_UTILS_FILES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/opt/apps/{APP_ID}/files")));
    for (source, destination) in [
        (
            files.join("lib/linglong/builder/uab/uab-header"),
            options.header,
        ),
        (
            files.join("lib/linglong/builder/uab/uab-loader"),
            options.loader,
        ),
        (files.join("bin/ll-box"), options.box_binary),
    ] {
        if let Some(destination) = destination {
            copy_file(&source, &destination)?;
        }
    }
    if let Some(packdir) = options.packdir {
        let (directory, output) = packdir
            .split_once(':')
            .context("--packdir requires DIR:OUTPUT_PATH")?;
        if directory.is_empty() || output.is_empty() {
            bail!("--packdir requires DIR:OUTPUT_PATH");
        }
        let directory = Path::new(directory);
        if !directory.is_dir() {
            bail!("{} not exist", directory.display());
        }
        let compression = options
            .compressor
            .as_deref()
            .unwrap_or("none")
            .parse::<ErofsCompression>()?;
        let image = build_erofs_image_with_compression(directory, compression, false)?;
        let output = Path::new(output);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, image)?;
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use linyaps_repository::unpack_erofs_file;
    use tempfile::tempdir;

    #[test]
    fn copies_tools_and_packs_directory() {
        let temporary = tempdir().unwrap();
        let files = temporary.path().join("files");
        fs::create_dir_all(files.join("lib/linglong/builder/uab")).unwrap();
        fs::create_dir_all(files.join("bin")).unwrap();
        fs::write(files.join("lib/linglong/builder/uab/uab-header"), "header").unwrap();
        fs::write(files.join("lib/linglong/builder/uab/uab-loader"), "loader").unwrap();
        fs::write(files.join("bin/ll-box"), "box").unwrap();
        let tree = temporary.path().join("tree");
        fs::create_dir(&tree).unwrap();
        let payload = b"builder export zstd payload\n".repeat(256);
        fs::write(tree.join("payload"), &payload).unwrap();
        let image = temporary.path().join("bundle.erofs");
        unsafe { env::set_var("LINGLONG_BUILDER_UTILS_FILES", &files) };
        run(Cli {
            header: Some(temporary.path().join("out/header")),
            loader: Some(temporary.path().join("out/loader")),
            box_binary: Some(temporary.path().join("out/ll-box")),
            packdir: Some(format!("{}:{}", tree.display(), image.display())),
            compressor: Some("zstd".to_string()),
        })
        .unwrap();
        unsafe { env::remove_var("LINGLONG_BUILDER_UTILS_FILES") };
        assert_eq!(
            fs::read(temporary.path().join("out/header")).unwrap(),
            b"header"
        );
        let extracted = temporary.path().join("extracted");
        let file = fs::File::open(image).unwrap();
        unpack_erofs_file(&file, 0, None, &extracted).unwrap();
        assert_eq!(fs::read(extracted.join("payload")).unwrap(), payload);
    }
}
