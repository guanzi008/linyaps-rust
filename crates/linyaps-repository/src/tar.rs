use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

const BLOCK_SIZE: usize = 512;

#[derive(Debug, Error)]
pub enum TarError {
    #[error("failed to extract tar archive: {0}")]
    Io(#[from] std::io::Error),
    #[error("tar archive is truncated")]
    Truncated,
    #[error("tar entry is too large")]
    TooLarge,
    #[error("invalid tar header checksum")]
    InvalidChecksum,
    #[error("invalid tar numeric field")]
    InvalidNumber,
    #[error("invalid pax record")]
    InvalidPax,
    #[error("archive contains unsafe path: {0}")]
    UnsafePath(String),
}

pub fn extract_tar(archive: &[u8], destination: impl AsRef<Path>) -> Result<(), TarError> {
    let destination = destination.as_ref();
    fs::create_dir_all(destination)?;
    let mut offset = 0_usize;
    let mut long_name = None;
    let mut long_link = None;
    let mut pax = BTreeMap::new();
    let mut deferred_links = Vec::new();
    while offset
        .checked_add(BLOCK_SIZE)
        .is_some_and(|end| end <= archive.len())
    {
        let header = &archive[offset..offset + BLOCK_SIZE];
        offset += BLOCK_SIZE;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        verify_checksum(header)?;
        let size = parse_number(&header[124..136])?;
        let data_end = offset.checked_add(size).ok_or(TarError::TooLarge)?;
        if data_end > archive.len() {
            return Err(TarError::Truncated);
        }
        let data = &archive[offset..data_end];
        offset = data_end
            .checked_add((BLOCK_SIZE - size % BLOCK_SIZE) % BLOCK_SIZE)
            .ok_or(TarError::TooLarge)?;
        let entry_type = header[156];
        if entry_type == b'L' {
            long_name = Some(trim_nul(data).trim_end_matches('\0').to_string());
            continue;
        }
        if entry_type == b'K' {
            long_link = Some(trim_nul(data).trim_end_matches('\0').to_string());
            continue;
        }
        if entry_type == b'x' {
            pax = parse_pax(data)?;
            continue;
        }
        if entry_type == b'g' {
            continue;
        }
        let name = pax
            .remove("path")
            .or_else(|| long_name.take())
            .unwrap_or_else(|| tar_name(header));
        let relative = safe_relative_path(&name)?;
        if relative.as_os_str().is_empty() {
            pax.clear();
            continue;
        }
        let output = checked_output(destination, &relative)?;
        let mode = parse_number(&header[100..108]).unwrap_or(0o755) as u32;
        match entry_type {
            0 | b'0' | b'7' => {
                ensure_parent(destination, &output)?;
                clear_path(&output)?;
                let mut file = File::create(&output)?;
                file.write_all(data)?;
                fs::set_permissions(&output, fs::Permissions::from_mode(mode & 0o7777))?;
            }
            b'5' => {
                ensure_parent(destination, &output)?;
                if fs::symlink_metadata(&output)
                    .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
                {
                    clear_path(&output)?;
                }
                fs::create_dir_all(&output)?;
                fs::set_permissions(&output, fs::Permissions::from_mode(mode & 0o7777))?;
            }
            b'2' => {
                ensure_parent(destination, &output)?;
                clear_path(&output)?;
                let target = pax
                    .remove("linkpath")
                    .or_else(|| long_link.take())
                    .unwrap_or_else(|| trim_nul(&header[157..257]));
                safe_link_target(&relative, &target)?;
                symlink(target, output)?;
            }
            b'1' => {
                let target = pax
                    .remove("linkpath")
                    .or_else(|| long_link.take())
                    .unwrap_or_else(|| trim_nul(&header[157..257]));
                let target = safe_relative_path(&target)?;
                deferred_links.push((output, target));
            }
            _ => {}
        }
        pax.clear();
    }
    for (output, target) in deferred_links {
        ensure_parent(destination, &output)?;
        clear_path(&output)?;
        let target = checked_output(destination, &target)?;
        fs::hard_link(target, output)?;
    }
    Ok(())
}

fn verify_checksum(header: &[u8]) -> Result<(), TarError> {
    let expected = parse_number(&header[148..156])?;
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                32
            } else {
                usize::from(*byte)
            }
        })
        .sum::<usize>();
    if actual != expected {
        return Err(TarError::InvalidChecksum);
    }
    Ok(())
}

fn parse_number(bytes: &[u8]) -> Result<usize, TarError> {
    if bytes.first().is_some_and(|byte| byte & 0x80 != 0) {
        let mut value = usize::from(bytes[0] & 0x7f);
        for byte in &bytes[1..] {
            value = value
                .checked_mul(256)
                .and_then(|value| value.checked_add(usize::from(*byte)))
                .ok_or(TarError::TooLarge)?;
        }
        return Ok(value);
    }
    let value = trim_nul(bytes);
    let value = value.trim().trim_matches('\0').trim();
    if value.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(value, 8).map_err(|_| TarError::InvalidNumber)
}

fn tar_name(header: &[u8]) -> String {
    let name = trim_nul(&header[..100]);
    let prefix = trim_nul(&header[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn parse_pax(data: &[u8]) -> Result<BTreeMap<String, String>, TarError> {
    let mut output = BTreeMap::new();
    let mut offset = 0;
    while offset < data.len() {
        let space = data[offset..]
            .iter()
            .position(|byte| *byte == b' ')
            .map(|index| offset + index)
            .ok_or(TarError::InvalidPax)?;
        let length = std::str::from_utf8(&data[offset..space])
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or(TarError::InvalidPax)?;
        let end = offset.checked_add(length).ok_or(TarError::InvalidPax)?;
        if end > data.len() || space + 1 >= end {
            return Err(TarError::InvalidPax);
        }
        let record = std::str::from_utf8(&data[space + 1..end])
            .map_err(|_| TarError::InvalidPax)?
            .trim_end_matches('\n');
        if let Some((key, value)) = record.split_once('=') {
            output.insert(key.to_string(), value.to_string());
        }
        offset = end;
    }
    Ok(output)
}

fn safe_relative_path(value: &str) -> Result<PathBuf, TarError> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(TarError::UnsafePath(value.to_string()));
    }
    Ok(path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect())
}

fn safe_link_target(link: &Path, value: &str) -> Result<(), TarError> {
    let target = Path::new(value);
    if target.is_absolute() {
        return Err(TarError::UnsafePath(value.to_string()));
    }
    let mut resolved = link
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => resolved.push(value.to_os_string()),
            Component::ParentDir if resolved.pop().is_some() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(TarError::UnsafePath(value.to_string()));
            }
        }
    }
    Ok(())
}

fn checked_output(destination: &Path, relative: &Path) -> Result<PathBuf, TarError> {
    let output = destination.join(relative);
    let mut current = destination.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(TarError::UnsafePath(relative.display().to_string()));
        }
    }
    Ok(output)
}

fn ensure_parent(destination: &Path, output: &Path) -> Result<(), TarError> {
    if let Some(parent) = output.parent() {
        let relative = parent
            .strip_prefix(destination)
            .map_err(|_| TarError::UnsafePath(output.display().to_string()))?;
        checked_output(destination, relative)?;
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn clear_path(path: &Path) -> Result<(), std::io::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn trim_nul(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|byte| *byte == 0).next().unwrap_or_default()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: u8, data: &[u8], link: &str) -> Vec<u8> {
        let mut header = [0_u8; BLOCK_SIZE];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000755\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[156] = kind;
        header[157..157 + link.len()].copy_from_slice(link.as_bytes());
        header[257..263].copy_from_slice(b"ustar\0");
        header[148..156].fill(b' ');
        let checksum = header.iter().map(|byte| usize::from(*byte)).sum::<usize>();
        header[148..156].copy_from_slice(format!("{:06o}\0 ", checksum).as_bytes());
        let mut output = header.to_vec();
        output.extend_from_slice(data);
        output.resize(output.len().next_multiple_of(BLOCK_SIZE), 0);
        output
    }

    #[test]
    fn extracts_regular_files_and_rejects_traversal() {
        let temporary = tempfile::tempdir().unwrap();
        let mut archive = entry("root/", b'5', &[], "");
        archive.extend(entry("root/file", b'0', b"payload", ""));
        archive.extend([0_u8; 1024]);
        extract_tar(&archive, temporary.path()).unwrap();
        assert_eq!(
            fs::read(temporary.path().join("root/file")).unwrap(),
            b"payload"
        );

        let mut unsafe_archive = entry("../escape", b'0', b"payload", "");
        unsafe_archive.extend([0_u8; 1024]);
        assert!(matches!(
            extract_tar(&unsafe_archive, temporary.path()),
            Err(TarError::UnsafePath(_))
        ));
    }

    #[test]
    fn rejects_writes_through_archive_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let mut archive = entry("link", b'2', &[], "target");
        archive.extend(entry("link/escape", b'0', b"payload", ""));
        archive.extend([0_u8; 1024]);
        assert!(matches!(
            extract_tar(&archive, temporary.path()),
            Err(TarError::UnsafePath(_))
        ));
    }

    #[test]
    fn permits_only_symlinks_resolving_inside_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let mut archive = entry("root/lib/link", b'2', &[], "../share/file");
        archive.extend([0_u8; 1024]);
        extract_tar(&archive, temporary.path()).unwrap();
        assert_eq!(
            fs::read_link(temporary.path().join("root/lib/link")).unwrap(),
            Path::new("../share/file")
        );

        let mut unsafe_archive = entry("root/link", b'2', &[], "../../escape");
        unsafe_archive.extend([0_u8; 1024]);
        assert!(matches!(
            extract_tar(&unsafe_archive, temporary.path()),
            Err(TarError::UnsafePath(_))
        ));
    }
}
