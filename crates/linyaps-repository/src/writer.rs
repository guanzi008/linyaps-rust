use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::str::FromStr;

use fs_erofs::mkfs::{
    CompressedAlgo, CompressedFileSpec, CompressedIndexFormat, Node, NodeMeta, XattrSpec,
    build_image,
};
use linyaps_api::{LayerInfo, PackageInfoV2};
use thiserror::Error;

const LAYER_MAGIC_PREFIX: &[u8] = b"<<< deepin linglong layer archive >>>";
const LAYER_MAGIC_LENGTH: usize = 40;
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ELFDATA2MSB: u8 = 2;
const SHN_XINDEX: u16 = 0xffff;
const SHT_PROGBITS: u32 = 1;

#[derive(Debug, Error)]
pub enum ErofsBuildError {
    #[error("failed to read source tree: {0}")]
    Io(#[from] std::io::Error),
    #[error("source tree contains a non-UTF-8 name: {0}")]
    NonUtf8Name(String),
    #[error("failed to build EROFS image: {0}")]
    Image(String),
    #[error("unsupported EROFS compressor: {0}")]
    UnsupportedCompression(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErofsCompression {
    Lz4,
    Lzma,
    Zstd,
    None,
}

impl FromStr for ErofsCompression {
    type Err = ErofsBuildError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lz4" => Ok(Self::Lz4),
            "lzma" => Ok(Self::Lzma),
            "zstd" => Ok(Self::Zstd),
            "none" => Ok(Self::None),
            value => Err(ErofsBuildError::UnsupportedCompression(value.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum LayerWriteError {
    #[error("failed to write layer file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize layer metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to build layer payload: {0}")]
    Erofs(#[from] ErofsBuildError),
    #[error("layer metadata is too large")]
    MetadataTooLarge,
}

#[derive(Debug, Error)]
pub enum UabWriteError {
    #[error("failed to write UAB: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid ELF template: {0}")]
    InvalidElf(String),
    #[error("invalid ELF section name: {0}")]
    InvalidName(String),
    #[error("UAB is too large for this ELF class")]
    TooLarge,
}

pub fn build_erofs_image(source: impl AsRef<Path>) -> Result<Vec<u8>, ErofsBuildError> {
    build_erofs_image_with_compression(source, ErofsCompression::None, false)
}

pub fn build_erofs_image_with_compression(
    source: impl AsRef<Path>,
    compression: ErofsCompression,
    ztailpacking: bool,
) -> Result<Vec<u8>, ErofsBuildError> {
    let root = source_node(source.as_ref(), compression, ztailpacking)?;
    build_image(root, 12).map_err(|error| ErofsBuildError::Image(error.to_string()))
}

pub fn write_layer_file(
    source: impl AsRef<Path>,
    info: &PackageInfoV2,
    output: impl AsRef<Path>,
) -> Result<(), LayerWriteError> {
    write_layer_file_with_compression(source, info, output, ErofsCompression::Lzma)
}

pub fn write_layer_file_with_compression(
    source: impl AsRef<Path>,
    info: &PackageInfoV2,
    output: impl AsRef<Path>,
    compression: ErofsCompression,
) -> Result<(), LayerWriteError> {
    let payload = build_erofs_image_with_compression(source, compression, false)?;
    let metadata = serde_json::to_vec(&LayerInfo {
        info: serde_json::to_value(info)?,
        version: "1".to_string(),
    })?;
    let metadata_length =
        u32::try_from(metadata.len()).map_err(|_| LayerWriteError::MetadataTooLarge)?;
    let output = output.as_ref();
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = output.with_extension(format!(
        "{}.tmp-{}",
        output
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = File::create(&temporary)?;
        let mut magic = [0_u8; LAYER_MAGIC_LENGTH];
        magic[..LAYER_MAGIC_PREFIX.len()].copy_from_slice(LAYER_MAGIC_PREFIX);
        file.write_all(&magic)?;
        file.write_all(&metadata_length.to_le_bytes())?;
        file.write_all(&metadata)?;
        file.write_all(&payload)?;
        file.sync_all()?;
        fs::rename(&temporary, output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn source_node(
    path: &Path,
    compression: ErofsCompression,
    ztailpacking: bool,
) -> Result<Node, ErofsBuildError> {
    let metadata = fs::symlink_metadata(path)?;
    let mode = metadata.mode() as u16;
    let node_meta = NodeMeta {
        uid: metadata.uid(),
        gid: metadata.gid(),
        mtime: metadata.mtime().max(0) as u64,
        mtime_nsec: metadata.mtime_nsec().max(0) as u32,
    };
    let xattrs = read_xattrs(path, metadata.file_type().is_symlink())?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        let mut entries = BTreeMap::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|name| {
                ErofsBuildError::NonUtf8Name(String::from_utf8_lossy(name.as_bytes()).into_owned())
            })?;
            entries.insert(name, source_node(&entry.path(), compression, ztailpacking)?);
        }
        return Ok(Node::Dir {
            mode,
            entries,
            meta: node_meta,
            xattrs,
        });
    }
    if file_type.is_file() {
        let data = fs::read(path)?;
        return Ok(match compression {
            ErofsCompression::None => Node::File {
                mode,
                data,
                meta: node_meta,
                xattrs,
            },
            ErofsCompression::Lz4 | ErofsCompression::Lzma | ErofsCompression::Zstd
                if data.is_empty() =>
            {
                Node::File {
                    mode,
                    data,
                    meta: node_meta,
                    xattrs,
                }
            }
            ErofsCompression::Lz4 | ErofsCompression::Lzma | ErofsCompression::Zstd => {
                Node::CompressedFile(CompressedFileSpec {
                    mode,
                    data,
                    algo: match compression {
                        ErofsCompression::Lz4 => CompressedAlgo::Lz4,
                        ErofsCompression::Lzma => CompressedAlgo::Lzma,
                        ErofsCompression::Zstd => CompressedAlgo::Zstd,
                        ErofsCompression::None => unreachable!(),
                    },
                    lclusterbits: 0,
                    meta: node_meta,
                    xattrs,
                    index_format: CompressedIndexFormat::Legacy,
                    ztailpacking,
                    target_pcluster_blocks: CompressedFileSpec::default_target_pcluster_blocks(),
                })
            }
        });
    }
    if file_type.is_symlink() {
        let target = fs::read_link(path)?;
        let target = target.into_os_string().into_string().map_err(|target| {
            ErofsBuildError::NonUtf8Name(String::from_utf8_lossy(target.as_bytes()).into_owned())
        })?;
        return Ok(Node::Symlink {
            mode,
            target,
            meta: node_meta,
            xattrs,
        });
    }
    if file_type.is_char_device() || file_type.is_block_device() {
        return Ok(Node::Device {
            mode,
            rdev: metadata.rdev() as u32,
            meta: node_meta,
            xattrs,
        });
    }
    Ok(Node::Special {
        mode,
        meta: node_meta,
        xattrs,
    })
}

fn read_xattrs(path: &Path, symlink: bool) -> Result<Vec<XattrSpec>, ErofsBuildError> {
    let mut names_buffer = vec![0_u8; 64 * 1024];
    let names = if symlink {
        rustix::fs::llistxattr(path, &mut names_buffer)
    } else {
        rustix::fs::listxattr(path, &mut names_buffer)
    };
    let names_length = match names {
        Ok(length) => length,
        Err(error) if error == rustix::io::Errno::NOTSUP || error == rustix::io::Errno::PERM => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(std::io::Error::from_raw_os_error(error.raw_os_error()).into()),
    };
    names_buffer.truncate(names_length);
    let mut output = Vec::new();
    for raw_name in names_buffer
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let Some((index, short_name)) = xattr_name(raw_name) else {
            continue;
        };
        let name = OsStr::from_bytes(raw_name);
        let mut value_buffer = vec![0_u8; 64 * 1024];
        let value = if symlink {
            rustix::fs::lgetxattr(path, name, &mut value_buffer)
        } else {
            rustix::fs::getxattr(path, name, &mut value_buffer)
        };
        match value {
            Ok(length) => {
                value_buffer.truncate(length);
                output.push(XattrSpec::new(index, short_name, value_buffer));
            }
            Err(error)
                if error == rustix::io::Errno::NODATA || error == rustix::io::Errno::PERM => {}
            Err(error) => {
                return Err(std::io::Error::from_raw_os_error(error.raw_os_error()).into());
            }
        }
    }
    Ok(output)
}

fn xattr_name(name: &[u8]) -> Option<(u8, Vec<u8>)> {
    if let Some(name) = name.strip_prefix(b"user.") {
        return Some((1, name.to_vec()));
    }
    if name == b"system.posix_acl_access" {
        return Some((2, Vec::new()));
    }
    if name == b"system.posix_acl_default" {
        return Some((3, Vec::new()));
    }
    if let Some(name) = name.strip_prefix(b"trusted.") {
        return Some((4, name.to_vec()));
    }
    name.strip_prefix(b"security.")
        .map(|name| (6, name.to_vec()))
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn read_u16(self, bytes: &[u8]) -> u16 {
        let bytes = [bytes[0], bytes[1]];
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }

    fn read_u32(self, bytes: &[u8]) -> u32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }

    fn read_u64(self, bytes: &[u8]) -> u64 {
        let bytes = [
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ];
        match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        }
    }

    fn write_u16(self, destination: &mut [u8], value: u16) {
        let bytes = match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        };
        destination.copy_from_slice(&bytes);
    }

    fn write_u32(self, destination: &mut [u8], value: u32) {
        let bytes = match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        };
        destination.copy_from_slice(&bytes);
    }

    fn write_u64(self, destination: &mut [u8], value: u64) {
        let bytes = match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        };
        destination.copy_from_slice(&bytes);
    }
}

pub fn append_elf_sections(
    template: impl AsRef<Path>,
    output: impl AsRef<Path>,
    sections: &[(&str, &[u8])],
) -> Result<(), UabWriteError> {
    let template = template.as_ref();
    let mut image = fs::read(template)?;
    if image.len() < 64 || &image[..4] != b"\x7fELF" {
        return Err(UabWriteError::InvalidElf("invalid ELF header".to_string()));
    }
    let class = image[4];
    let endian = match image[5] {
        ELFDATA2LSB => Endian::Little,
        ELFDATA2MSB => Endian::Big,
        value => {
            return Err(UabWriteError::InvalidElf(format!(
                "unknown data encoding {value}"
            )));
        }
    };
    let (section_offset, entry_size, mut section_count, mut names_index) = match class {
        ELFCLASS32 => (
            u64::from(endian.read_u32(&image[32..36])),
            usize::from(endian.read_u16(&image[46..48])),
            usize::from(endian.read_u16(&image[48..50])),
            usize::from(endian.read_u16(&image[50..52])),
        ),
        ELFCLASS64 => (
            endian.read_u64(&image[40..48]),
            usize::from(endian.read_u16(&image[58..60])),
            usize::from(endian.read_u16(&image[60..62])),
            usize::from(endian.read_u16(&image[62..64])),
        ),
        value => {
            return Err(UabWriteError::InvalidElf(format!(
                "unknown ELF class {value}"
            )));
        }
    };
    let minimum = if class == ELFCLASS32 { 40 } else { 64 };
    if section_offset == 0 || entry_size < minimum {
        return Err(UabWriteError::InvalidElf(
            "invalid section header table".to_string(),
        ));
    }
    let table_start = usize::try_from(section_offset).map_err(|_| UabWriteError::TooLarge)?;
    let first_end = table_start
        .checked_add(entry_size)
        .ok_or(UabWriteError::TooLarge)?;
    if first_end > image.len() {
        return Err(UabWriteError::InvalidElf(
            "section table exceeds template".to_string(),
        ));
    }
    let first = &image[table_start..first_end];
    if section_count == 0 {
        section_count = if class == ELFCLASS32 {
            usize::try_from(endian.read_u32(&first[20..24])).map_err(|_| UabWriteError::TooLarge)?
        } else {
            usize::try_from(endian.read_u64(&first[32..40])).map_err(|_| UabWriteError::TooLarge)?
        };
    }
    if names_index == usize::from(SHN_XINDEX) {
        names_index = if class == ELFCLASS32 {
            usize::try_from(endian.read_u32(&first[24..28])).map_err(|_| UabWriteError::TooLarge)?
        } else {
            usize::try_from(endian.read_u32(&first[40..44])).map_err(|_| UabWriteError::TooLarge)?
        };
    }
    if section_count == 0 || section_count > 1_000_000 || names_index >= section_count {
        return Err(UabWriteError::InvalidElf(
            "invalid section header table".to_string(),
        ));
    }
    let table_end = table_start
        .checked_add(
            entry_size
                .checked_mul(section_count)
                .ok_or(UabWriteError::TooLarge)?,
        )
        .ok_or(UabWriteError::TooLarge)?;
    if table_end > image.len() {
        return Err(UabWriteError::InvalidElf(
            "section table exceeds template".to_string(),
        ));
    }
    let mut headers = image[table_start..table_end]
        .chunks_exact(entry_size)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let names_header = &headers[names_index];
    let (names_offset, names_size) = if class == ELFCLASS32 {
        (
            u64::from(endian.read_u32(&names_header[16..20])),
            u64::from(endian.read_u32(&names_header[20..24])),
        )
    } else {
        (
            endian.read_u64(&names_header[24..32]),
            endian.read_u64(&names_header[32..40]),
        )
    };
    let names_start = usize::try_from(names_offset).map_err(|_| UabWriteError::TooLarge)?;
    let names_end = names_start
        .checked_add(usize::try_from(names_size).map_err(|_| UabWriteError::TooLarge)?)
        .ok_or(UabWriteError::TooLarge)?;
    if names_end > image.len() {
        return Err(UabWriteError::InvalidElf(
            "section name table exceeds template".to_string(),
        ));
    }
    let mut names = image[names_start..names_end].to_vec();
    if names.last() != Some(&0) {
        names.push(0);
    }
    let mut additions = Vec::new();
    for (name, data) in sections {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(UabWriteError::InvalidName((*name).to_string()));
        }
        align(&mut image, 8);
        let data_offset = image.len() as u64;
        image.extend_from_slice(data);
        let name_offset = u32::try_from(names.len()).map_err(|_| UabWriteError::TooLarge)?;
        names.extend_from_slice(name.as_bytes());
        names.push(0);
        additions.push((name_offset, data_offset, data.len() as u64));
    }
    align(&mut image, 8);
    let new_names_offset = image.len() as u64;
    image.extend_from_slice(&names);
    if class == ELFCLASS32 {
        endian.write_u32(
            &mut headers[names_index][16..20],
            u32::try_from(new_names_offset).map_err(|_| UabWriteError::TooLarge)?,
        );
        endian.write_u32(
            &mut headers[names_index][20..24],
            u32::try_from(names.len()).map_err(|_| UabWriteError::TooLarge)?,
        );
    } else {
        endian.write_u64(&mut headers[names_index][24..32], new_names_offset);
        endian.write_u64(&mut headers[names_index][32..40], names.len() as u64);
    }
    for (name_offset, data_offset, data_size) in additions {
        let mut header = vec![0_u8; entry_size];
        endian.write_u32(&mut header[0..4], name_offset);
        endian.write_u32(&mut header[4..8], SHT_PROGBITS);
        if class == ELFCLASS32 {
            endian.write_u32(
                &mut header[16..20],
                u32::try_from(data_offset).map_err(|_| UabWriteError::TooLarge)?,
            );
            endian.write_u32(
                &mut header[20..24],
                u32::try_from(data_size).map_err(|_| UabWriteError::TooLarge)?,
            );
            endian.write_u32(&mut header[32..36], 1);
        } else {
            endian.write_u64(&mut header[24..32], data_offset);
            endian.write_u64(&mut header[32..40], data_size);
            endian.write_u64(&mut header[48..56], 1);
        }
        headers.push(header);
    }
    align(&mut image, 8);
    let new_table_offset = image.len() as u64;
    for header in headers.iter() {
        image.extend_from_slice(header);
    }
    let new_table_start = usize::try_from(new_table_offset).map_err(|_| UabWriteError::TooLarge)?;
    if class == ELFCLASS32 {
        endian.write_u32(
            &mut image[32..36],
            u32::try_from(new_table_offset).map_err(|_| UabWriteError::TooLarge)?,
        );
        if headers.len() >= 0xff00 {
            endian.write_u16(&mut image[48..50], 0);
            endian.write_u32(
                &mut image[new_table_start + 20..new_table_start + 24],
                u32::try_from(headers.len()).map_err(|_| UabWriteError::TooLarge)?,
            );
        } else {
            endian.write_u16(&mut image[48..50], headers.len() as u16);
        }
        if names_index >= 0xff00 {
            endian.write_u16(&mut image[50..52], SHN_XINDEX);
            endian.write_u32(
                &mut image[new_table_start + 24..new_table_start + 28],
                u32::try_from(names_index).map_err(|_| UabWriteError::TooLarge)?,
            );
        } else {
            endian.write_u16(&mut image[50..52], names_index as u16);
        }
    } else {
        endian.write_u64(&mut image[40..48], new_table_offset);
        if headers.len() >= 0xff00 {
            endian.write_u16(&mut image[60..62], 0);
            endian.write_u64(
                &mut image[new_table_start + 32..new_table_start + 40],
                headers.len() as u64,
            );
        } else {
            endian.write_u16(&mut image[60..62], headers.len() as u16);
        }
        if names_index >= 0xff00 {
            endian.write_u16(&mut image[62..64], SHN_XINDEX);
            endian.write_u32(
                &mut image[new_table_start + 40..new_table_start + 44],
                u32::try_from(names_index).map_err(|_| UabWriteError::TooLarge)?,
            );
        } else {
            endian.write_u16(&mut image[62..64], names_index as u16);
        }
    }
    fs::write(output.as_ref(), image)?;
    fs::set_permissions(
        output.as_ref(),
        fs::Permissions::from_mode(template.metadata()?.mode()),
    )?;
    Ok(())
}

fn align(bytes: &mut Vec<u8>, alignment: usize) {
    let padding = (alignment - bytes.len() % alignment) % alignment;
    bytes.resize(bytes.len() + padding, 0);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use linyaps_api::{PackageInfoV2, UabLayer, UabMetaInfo, UabSections};
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::*;
    use crate::{UabFile, read_layer_info, unpack_layer};

    fn package_info() -> PackageInfoV2 {
        PackageInfoV2 {
            arch: vec!["x86_64".to_string()],
            base: "org.deepin.Base/23.1.0".to_string(),
            channel: "main".to_string(),
            command: Some(vec!["/bin/demo".to_string()]),
            compatible_version: None,
            description: None,
            extension_implementation: None,
            extensions: None,
            id: "org.example.App".to_string(),
            kind: "app".to_string(),
            module: "binary".to_string(),
            name: "Demo".to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 7,
            uuid: None,
            version: "1.0.0.0".to_string(),
        }
    }

    fn extended_section_template() -> Vec<u8> {
        const SECTION_COUNT: usize = 0xff00;
        const SECTION_SIZE: usize = 64;
        const TABLE_OFFSET: usize = 80;
        let names = b"\0.shstrtab\0";
        let mut image = vec![0_u8; TABLE_OFFSET + SECTION_COUNT * SECTION_SIZE];
        image[..4].copy_from_slice(b"\x7fELF");
        image[4] = ELFCLASS64;
        image[5] = ELFDATA2LSB;
        image[6] = 1;
        image[40..48].copy_from_slice(&(TABLE_OFFSET as u64).to_le_bytes());
        image[52..54].copy_from_slice(&64_u16.to_le_bytes());
        image[58..60].copy_from_slice(&(SECTION_SIZE as u16).to_le_bytes());
        image[60..62].copy_from_slice(&0_u16.to_le_bytes());
        image[62..64].copy_from_slice(&1_u16.to_le_bytes());
        image[64..64 + names.len()].copy_from_slice(names);
        image[TABLE_OFFSET + 32..TABLE_OFFSET + 40]
            .copy_from_slice(&(SECTION_COUNT as u64).to_le_bytes());
        let names_header = TABLE_OFFSET + SECTION_SIZE;
        image[names_header..names_header + 4].copy_from_slice(&1_u32.to_le_bytes());
        image[names_header + 4..names_header + 8].copy_from_slice(&3_u32.to_le_bytes());
        image[names_header + 24..names_header + 32].copy_from_slice(&64_u64.to_le_bytes());
        image[names_header + 32..names_header + 40]
            .copy_from_slice(&(names.len() as u64).to_le_bytes());
        image[names_header + 48..names_header + 56].copy_from_slice(&1_u64.to_le_bytes());
        image
    }

    #[test]
    fn layer_file_round_trip() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("files/bin")).unwrap();
        fs::write(source.join("files/bin/demo"), "payload").unwrap();
        let info = package_info();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec_pretty(&info).unwrap(),
        )
        .unwrap();
        let layer = temporary.path().join("demo.layer");
        write_layer_file(&source, &info, &layer).unwrap();

        let header = read_layer_info(&layer).unwrap();
        assert_eq!(header.version, "1");
        assert_eq!(header.info, serde_json::to_value(&info).unwrap());
        let extracted = temporary.path().join("extracted");
        unpack_layer(&layer, &extracted).unwrap();
        assert_eq!(
            fs::read(extracted.join("files/bin/demo")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn zstd_erofs_round_trip() {
        assert_eq!(
            "zstd".parse::<ErofsCompression>().unwrap(),
            ErofsCompression::Zstd
        );
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir_all(source.join("files/share")).unwrap();
        let payload = b"zstd repository round trip\n".repeat(512);
        fs::write(source.join("files/share/data.bin"), &payload).unwrap();

        let image =
            build_erofs_image_with_compression(&source, ErofsCompression::Zstd, false).unwrap();
        let image_path = temporary.path().join("zstd.erofs");
        fs::write(&image_path, image).unwrap();
        let image_file = File::open(image_path).unwrap();
        let extracted = temporary.path().join("extracted");
        crate::unpack_erofs_file(&image_file, 0, None, &extracted).unwrap();
        assert_eq!(
            fs::read(extracted.join("files/share/data.bin")).unwrap(),
            payload
        );
    }

    #[test]
    fn erofs_and_elf_sections_round_trip() {
        let temporary = tempdir().unwrap();
        let tree = temporary.path().join("tree");
        fs::create_dir_all(tree.join("layers/org.example.App/binary/files/bin")).unwrap();
        fs::write(
            tree.join("layers/org.example.App/binary/files/bin/demo"),
            "payload",
        )
        .unwrap();
        symlink(
            "demo",
            tree.join("layers/org.example.App/binary/files/bin/demo-link"),
        )
        .unwrap();
        let bundle = build_erofs_image(&tree).unwrap();
        let digest = linyaps_core::hex_encode(Sha256::digest(&bundle));
        let metadata = UabMetaInfo {
            digest,
            layers: vec![UabLayer {
                info: package_info(),
                minified: false,
            }],
            only_app: Some(true),
            sections: UabSections {
                bundle: "linglong.bundle".to_string(),
                icon: None,
            },
            uuid: "test-uab".to_string(),
            version: "1".to_string(),
        };
        let metadata = serde_json::to_vec(&metadata).unwrap();
        let output = temporary.path().join("demo.uab");
        append_elf_sections(
            std::env::current_exe().unwrap(),
            &output,
            &[
                ("linglong.bundle", bundle.as_slice()),
                ("linglong.meta", metadata.as_slice()),
            ],
        )
        .unwrap();
        let uab = UabFile::open(&output).unwrap();
        uab.verify().unwrap();
        let extracted = temporary.path().join("extracted");
        uab.unpack_bundle(&extracted).unwrap();
        assert_eq!(
            fs::read(extracted.join("layers/org.example.App/binary/files/bin/demo")).unwrap(),
            b"payload"
        );
        assert_eq!(
            fs::read_link(extracted.join("layers/org.example.App/binary/files/bin/demo-link"))
                .unwrap(),
            Path::new("demo")
        );
    }

    #[test]
    fn appends_to_elf_with_extended_section_count() {
        let temporary = tempdir().unwrap();
        let template = temporary.path().join("extended-template");
        let output = temporary.path().join("extended-output");
        fs::write(&template, extended_section_template()).unwrap();

        append_elf_sections(&template, &output, &[("extra", b"payload")]).unwrap();

        let bytes = fs::read(&output).unwrap();
        assert_eq!(u16::from_le_bytes(bytes[60..62].try_into().unwrap()), 0);
        let table_offset =
            usize::try_from(u64::from_le_bytes(bytes[40..48].try_into().unwrap())).unwrap();
        assert_eq!(
            u64::from_le_bytes(
                bytes[table_offset + 32..table_offset + 40]
                    .try_into()
                    .unwrap()
            ),
            0xff01
        );
        let uab = UabFile::open(&output).unwrap();
        assert_eq!(uab.read_section("extra").unwrap(), b"payload");
    }
}
