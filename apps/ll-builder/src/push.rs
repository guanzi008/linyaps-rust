use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use linyaps_api::BuilderProject;
use linyaps_core::repository::default_repo;
use linyaps_repository::{LocalRepository, RemoteRepositoryClient};

use crate::project::current_reference;
use crate::source::clear_path;

pub async fn push(
    repository: &LocalRepository,
    project: &BuilderProject,
    current_directory: &Path,
    module: Option<String>,
    repository_url: Option<String>,
    repository_name: Option<String>,
) -> Result<()> {
    let reference = current_reference(project)?;
    let modules = module.map_or_else(
        || {
            let mut modules = vec!["binary".to_string(), "develop".to_string()];
            modules.extend(
                project
                    .modules
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|module| module.name.clone()),
            );
            modules.sort();
            modules.dedup();
            modules
        },
        |module| vec![module],
    );
    let (repository_name, repository_url) = match (repository_name, repository_url) {
        (Some(name), Some(url)) if !name.is_empty() && !url.is_empty() => (name, url),
        _ => {
            let default = default_repo(repository.config())?;
            (default.name.clone(), default.url.clone())
        }
    };
    let username = env::var("LINGLONG_USERNAME").unwrap_or_default();
    let password = env::var("LINGLONG_PASSWORD").unwrap_or_default();
    let client = RemoteRepositoryClient::new(repository_url)?;
    let token = client
        .sign_in(&username, &password)
        .await
        .with_context(|| format!("sign error({username})"))?
        .token;
    let temporary = current_directory.join("linglong/push");
    clear_path(&temporary)?;
    fs::create_dir_all(&temporary)?;
    for module in modules {
        eprintln!("Pushing module: {module}");
        let layer = repository.layer_path(&reference, &module)?;
        let archive = temporary.join(format!("{}-{module}.tgz", reference.id));
        create_tar_gz(&layer, &archive)?;
        let remote_reference = format!(
            "{}/{}/{}/{}/{}",
            reference.channel, reference.id, reference.version, reference.architecture, module
        );
        let task = client
            .new_upload_task(&token, &repository_name, &remote_reference)
            .await
            .context("create task error")?;
        client
            .upload_layer_file(&token, &task.id, &archive)
            .await
            .with_context(|| format!("upload file error({})", task.id))?;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let status = client
                .upload_task_status(&token, &task.id)
                .await
                .with_context(|| format!("get upload info error({})", task.id))?;
            eprintln!("pushing {reference}/{module} status: {}", status.status);
            match status.status.as_str() {
                "complete" => break,
                "failed" => bail!("An error occurred on the remote server({})", task.id),
                _ => {}
            }
        }
        eprintln!("Module {module} pushed successfully.");
    }
    let _ = clear_path(&temporary);
    eprintln!("All modules pushed successfully.");
    Ok(())
}

fn create_tar_gz(source: &Path, output: &Path) -> Result<()> {
    let file = File::create(output)?;
    let mut encoder = GzEncoder::new(file, Compression::default());
    let mut hardlinks = std::collections::HashMap::new();
    write_tar_tree(source, source, &mut encoder, &mut hardlinks)?;
    encoder.write_all(&[0_u8; 1024])?;
    encoder.finish()?.sync_all()?;
    Ok(())
}

fn write_tar_tree(
    root: &Path,
    path: &Path,
    output: &mut impl Write,
    hardlinks: &mut std::collections::HashMap<(u64, u64), String>,
) -> Result<()> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root)?;
        let name = relative
            .to_str()
            .with_context(|| format!("archive path is not UTF-8: {}", relative.display()))?;
        if metadata.is_dir() {
            write_tar_header(output, &format!("{name}/"), &metadata, b'5', 0, None)?;
            write_tar_tree(root, &path, output, hardlinks)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            let target = target
                .to_str()
                .with_context(|| format!("symlink target is not UTF-8: {}", target.display()))?;
            write_tar_header(output, name, &metadata, b'2', 0, Some(target))?;
        } else if metadata.is_file() {
            let key = (metadata.dev(), metadata.ino());
            if metadata.nlink() > 1
                && let Some(target) = hardlinks.get(&key)
            {
                write_tar_header(output, name, &metadata, b'1', 0, Some(target))?;
                continue;
            }
            hardlinks.insert(key, name.to_string());
            write_tar_header(output, name, &metadata, b'0', metadata.len(), None)?;
            let mut file = File::open(&path)?;
            let mut buffer = [0_u8; 128 * 1024];
            let mut remaining = metadata.len();
            while remaining != 0 {
                let length = usize::try_from(remaining.min(buffer.len() as u64))?;
                file.read_exact(&mut buffer[..length])?;
                output.write_all(&buffer[..length])?;
                remaining -= length as u64;
            }
            write_padding(output, metadata.len())?;
        }
    }
    Ok(())
}

fn write_tar_header(
    output: &mut impl Write,
    name: &str,
    metadata: &fs::Metadata,
    entry_type: u8,
    size: u64,
    link_name: Option<&str>,
) -> Result<()> {
    let mut header = [0_u8; 512];
    write_tar_path(&mut header, name)?;
    write_octal(&mut header[100..108], u64::from(metadata.mode() & 0o7777))?;
    write_octal(&mut header[108..116], u64::from(metadata.uid()))?;
    write_octal(&mut header[116..124], u64::from(metadata.gid()))?;
    write_octal(&mut header[124..136], size)?;
    write_octal(&mut header[136..148], metadata.mtime().max(0) as u64)?;
    header[148..156].fill(b' ');
    header[156] = entry_type;
    if let Some(link_name) = link_name {
        if link_name.len() > 100 {
            bail!("tar link target is too long: {link_name}");
        }
        header[157..157 + link_name.len()].copy_from_slice(link_name.as_bytes());
    }
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
    let checksum = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum.as_bytes());
    output.write_all(&header)?;
    if size == 0 {
        return Ok(());
    }
    Ok(())
}

fn write_tar_path(header: &mut [u8; 512], path: &str) -> Result<()> {
    let bytes = path.as_bytes();
    if bytes.len() <= 100 {
        header[..bytes.len()].copy_from_slice(bytes);
        return Ok(());
    }
    let split = path
        .char_indices()
        .filter(|(_, character)| *character == '/')
        .map(|(index, _)| index)
        .find(|index| *index <= 155 && bytes.len() - index - 1 <= 100)
        .context("tar path is too long")?;
    header[..bytes.len() - split - 1].copy_from_slice(&bytes[split + 1..]);
    header[345..345 + split].copy_from_slice(&bytes[..split]);
    Ok(())
}

fn write_octal(destination: &mut [u8], value: u64) -> Result<()> {
    let digits = destination.len() - 1;
    let value = format!("{value:0digits$o}", digits = digits);
    if value.len() > digits {
        bail!("tar numeric field overflow");
    }
    destination[..digits].copy_from_slice(value.as_bytes());
    destination[digits] = 0;
    Ok(())
}

fn write_padding(output: &mut impl Write, size: u64) -> Result<()> {
    let padding = (512 - size % 512) % 512;
    if padding != 0 {
        output.write_all(&vec![0_u8; padding as usize])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use flate2::read::GzDecoder;
    use tempfile::tempdir;

    use super::*;
    use crate::source::extract_tar;

    #[test]
    fn tar_gz_round_trips_files_symlinks_and_hardlinks() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("files/bin")).unwrap();
        fs::write(source.join("files/bin/demo"), "payload").unwrap();
        std::os::unix::fs::symlink("demo", source.join("files/bin/demo-link")).unwrap();
        fs::hard_link(
            source.join("files/bin/demo"),
            source.join("files/bin/demo-hard"),
        )
        .unwrap();
        let archive = temporary.path().join("layer.tgz");
        create_tar_gz(&source, &archive).unwrap();
        let mut tar = Vec::new();
        GzDecoder::new(File::open(archive).unwrap())
            .read_to_end(&mut tar)
            .unwrap();
        let extracted = temporary.path().join("extracted");
        fs::create_dir_all(&extracted).unwrap();
        extract_tar(&tar, &extracted).unwrap();
        assert_eq!(
            fs::read(extracted.join("files/bin/demo")).unwrap(),
            b"payload"
        );
        assert_eq!(
            fs::read_link(extracted.join("files/bin/demo-link")).unwrap(),
            Path::new("demo")
        );
        assert_eq!(
            fs::metadata(extracted.join("files/bin/demo"))
                .unwrap()
                .ino(),
            fs::metadata(extracted.join("files/bin/demo-hard"))
                .unwrap()
                .ino()
        );
    }
}
