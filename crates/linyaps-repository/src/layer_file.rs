use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::{BlockRead, Error as BlockError};
use fs_erofs::{FileType, Filesystem, Inode};
use linyaps_api::LayerInfo;
use thiserror::Error;

const MAGIC_PREFIX: &[u8] = b"<<< deepin linglong layer archive >>>";
const MAGIC_LENGTH: usize = 40;
const LENGTH_FIELD_SIZE: u64 = 4;

#[derive(Debug, Error)]
pub enum LayerFileError {
    #[error("failed to read layer file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid magic number, this is not a layer")]
    InvalidMagic,
    #[error("invalid layer metadata length: {0}")]
    InvalidMetadataLength(u32),
    #[error("failed to parse layer metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to read EROFS payload: {0}")]
    Erofs(String),
    #[error("invalid layer entry name")]
    InvalidEntryName,
    #[error("unsupported special layer entry: {0}")]
    UnsupportedSpecial(PathBuf),
}

pub fn read_layer_info(path: impl AsRef<Path>) -> Result<LayerInfo, LayerFileError> {
    let mut file = File::open(path)?;
    read_layer_info_from(&mut file)
}

pub fn read_layer_info_from(file: &mut File) -> Result<LayerInfo, LayerFileError> {
    file.seek(SeekFrom::Start(0))?;
    let (info, _) = read_header(file)?;
    Ok(info)
}

pub fn unpack_layer(
    path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<LayerInfo, LayerFileError> {
    let mut file = File::open(path)?;
    unpack_layer_file(&mut file, destination)
}

pub fn unpack_layer_file(
    file: &mut File,
    destination: impl AsRef<Path>,
) -> Result<LayerInfo, LayerFileError> {
    file.seek(SeekFrom::Start(0))?;
    let (info, payload_offset) = read_header(file)?;
    unpack_erofs_file(file, payload_offset, None, destination)?;
    Ok(info)
}

pub fn unpack_erofs_file(
    file: &File,
    payload_offset: u64,
    payload_size: Option<u64>,
    destination: impl AsRef<Path>,
) -> Result<(), LayerFileError> {
    let device = Arc::new(OffsetDevice::new(
        file.try_clone()?,
        payload_offset,
        payload_size,
    )?);
    let filesystem =
        Filesystem::open(device).map_err(|error| LayerFileError::Erofs(error.to_string()))?;
    let destination = destination.as_ref();
    if fs::symlink_metadata(destination).is_ok() {
        if fs::symlink_metadata(destination)?.is_dir() {
            fs::remove_dir_all(destination)?;
        } else {
            fs::remove_file(destination)?;
        }
    }
    fs::create_dir_all(destination)?;
    let root = filesystem
        .root_inode()
        .map_err(|error| LayerFileError::Erofs(error.to_string()))?;
    let mut hardlinks = HashMap::new();
    extract_directory(&filesystem, &root, destination, &mut hardlinks)?;
    apply_inode_metadata(&filesystem, &root, destination, false)?;
    Ok(())
}

fn read_header(file: &mut File) -> Result<(LayerInfo, u64), LayerFileError> {
    let mut magic = [0_u8; MAGIC_LENGTH];
    file.read_exact(&mut magic)?;
    if !magic.starts_with(MAGIC_PREFIX) || magic[MAGIC_PREFIX.len()..].iter().any(|byte| *byte != 0)
    {
        return Err(LayerFileError::InvalidMagic);
    }

    let mut length = [0_u8; LENGTH_FIELD_SIZE as usize];
    file.read_exact(&mut length)?;
    let metadata_length = u32::from_le_bytes(length);
    let remaining = file
        .metadata()?
        .len()
        .saturating_sub(MAGIC_LENGTH as u64 + LENGTH_FIELD_SIZE);
    if u64::from(metadata_length) > remaining {
        return Err(LayerFileError::InvalidMetadataLength(metadata_length));
    }

    let mut metadata = vec![0_u8; metadata_length as usize];
    file.read_exact(&mut metadata)?;
    Ok((
        serde_json::from_slice(&metadata)?,
        MAGIC_LENGTH as u64 + LENGTH_FIELD_SIZE + u64::from(metadata_length),
    ))
}

struct OffsetDevice {
    file: File,
    offset: u64,
    size: u64,
}

impl OffsetDevice {
    fn new(file: File, offset: u64, size: Option<u64>) -> Result<Self, std::io::Error> {
        let file_size = file.metadata()?.len();
        if offset > file_size || size.is_some_and(|size| offset.saturating_add(size) > file_size) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EROFS section exceeds source file",
            ));
        }
        let size = size.unwrap_or(file_size - offset);
        Ok(Self { file, offset, size })
    }
}

impl BlockRead for OffsetDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> fs_core::Result<()> {
        let length = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if offset.checked_add(length).is_none_or(|end| end > self.size) {
            return Err(BlockError::OutOfBounds {
                offset,
                len: length,
                size: self.size,
            });
        }
        self.file
            .read_exact_at(buffer, self.offset + offset)
            .map_err(BlockError::Io)
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }
}

fn extract_directory(
    filesystem: &Filesystem,
    directory: &Inode,
    destination: &Path,
    hardlinks: &mut HashMap<u64, PathBuf>,
) -> Result<(), LayerFileError> {
    for entry in filesystem
        .read_dir(directory)
        .map_err(|error| LayerFileError::Erofs(error.to_string()))?
    {
        if matches!(entry.name.as_slice(), b"." | b"..") {
            continue;
        }
        if entry.name.is_empty() || entry.name.contains(&b'/') || entry.name.contains(&0) {
            return Err(LayerFileError::InvalidEntryName);
        }
        let name = OsString::from_vec(entry.name);
        let path = destination.join(name);
        let inode = filesystem
            .read_inode(entry.nid)
            .map_err(|error| LayerFileError::Erofs(error.to_string()))?;
        match inode.file_type() {
            FileType::Dir => {
                fs::create_dir(&path)?;
                extract_directory(filesystem, &inode, &path, hardlinks)?;
                apply_inode_metadata(filesystem, &inode, &path, false)?;
            }
            FileType::RegularFile => {
                if let Some(existing) = hardlinks.get(&inode.nid) {
                    fs::hard_link(existing, &path)?;
                } else {
                    let length = usize::try_from(inode.size).map_err(|_| {
                        LayerFileError::Erofs("file is too large for this platform".to_string())
                    })?;
                    let mut content = vec![0_u8; length];
                    if !content.is_empty() {
                        filesystem
                            .read_file(&inode, 0, &mut content)
                            .map_err(|error| LayerFileError::Erofs(error.to_string()))?;
                    }
                    let mut output = File::create(&path)?;
                    output.write_all(&content)?;
                    hardlinks.insert(inode.nid, path.clone());
                    apply_inode_metadata(filesystem, &inode, &path, false)?;
                }
            }
            FileType::Symlink => {
                let target = filesystem
                    .read_symlink_target(&inode)
                    .map_err(|error| LayerFileError::Erofs(error.to_string()))?;
                symlink(OsString::from_vec(target), &path)?;
                apply_inode_metadata(filesystem, &inode, &path, true)?;
            }
            FileType::Fifo | FileType::ChrDev | FileType::BlkDev | FileType::Sock => {
                if let Some(existing) = hardlinks.get(&inode.nid) {
                    fs::hard_link(existing, &path)?;
                } else {
                    create_special_entry(&path, &inode)?;
                    hardlinks.insert(inode.nid, path.clone());
                    apply_inode_metadata(filesystem, &inode, &path, false)?;
                }
            }
            FileType::Unknown => return Err(LayerFileError::UnsupportedSpecial(path)),
        }
    }
    Ok(())
}

fn create_special_entry(path: &Path, inode: &Inode) -> Result<(), LayerFileError> {
    let file_type = match inode.file_type() {
        FileType::Fifo => rustix::fs::FileType::Fifo,
        FileType::ChrDev => rustix::fs::FileType::CharacterDevice,
        FileType::BlkDev => rustix::fs::FileType::BlockDevice,
        FileType::Sock => rustix::fs::FileType::Socket,
        _ => return Err(LayerFileError::UnsupportedSpecial(path.to_path_buf())),
    };
    let device = inode
        .rdev()
        .map_or(0, |(major, minor)| rustix::fs::makedev(major, minor));
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        file_type,
        rustix::fs::Mode::from_raw_mode(u32::from(inode.mode)),
        device,
    )
    .map_err(|error| LayerFileError::Io(std::io::Error::from_raw_os_error(error.raw_os_error())))
}

fn apply_inode_metadata(
    filesystem: &Filesystem,
    inode: &Inode,
    path: &Path,
    symlink_entry: bool,
) -> Result<(), LayerFileError> {
    if inode.uid != u32::MAX && inode.gid != u32::MAX {
        let flags = if symlink_entry {
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW
        } else {
            rustix::fs::AtFlags::empty()
        };
        if let Err(error) = rustix::fs::chownat(
            rustix::fs::CWD,
            path,
            Some(rustix::fs::Uid::from_raw(inode.uid)),
            Some(rustix::fs::Gid::from_raw(inode.gid)),
            flags,
        ) && error != rustix::io::Errno::PERM
            && error != rustix::io::Errno::NOTSUP
        {
            return Err(LayerFileError::Io(std::io::Error::from_raw_os_error(
                error.raw_os_error(),
            )));
        }
    }
    if !symlink_entry {
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(u32::from(inode.mode & 0o7777)),
        )?;
    }
    for (name, value) in filesystem
        .xattrs(inode)
        .map_err(|error| LayerFileError::Erofs(error.to_string()))?
    {
        if name.contains(&0) {
            continue;
        }
        let name = OsStr::from_bytes(&name);
        let result = if symlink_entry {
            rustix::fs::lsetxattr(path, name, &value, rustix::fs::XattrFlags::empty())
        } else {
            rustix::fs::setxattr(path, name, &value, rustix::fs::XattrFlags::empty())
        };
        if let Err(error) = result
            && error != rustix::io::Errno::PERM
            && error != rustix::io::Errno::NOTSUP
        {
            return Err(LayerFileError::Io(std::io::Error::from_raw_os_error(
                error.raw_os_error(),
            )));
        }
    }
    let timestamp = rustix::fs::Timespec {
        tv_sec: i64::try_from(inode.mtime).unwrap_or(i64::MAX),
        tv_nsec: i64::from(inode.mtime_nsec.min(999_999_999)),
    };
    let timestamps = rustix::fs::Timestamps {
        last_access: timestamp,
        last_modification: timestamp,
    };
    let flags = if symlink_entry {
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW
    } else {
        rustix::fs::AtFlags::empty()
    };
    if let Err(error) = rustix::fs::utimensat(rustix::fs::CWD, path, &timestamps, flags)
        && error != rustix::io::Errno::PERM
        && error != rustix::io::Errno::NOTSUP
    {
        return Err(LayerFileError::Io(std::io::Error::from_raw_os_error(
            error.raw_os_error(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::FileTypeExt;

    use fs_erofs::mkfs::{
        DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE, Node, NodeMeta, build_image,
    };
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn layer_bytes(metadata: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0_u8; MAGIC_LENGTH];
        bytes[..MAGIC_PREFIX.len()].copy_from_slice(MAGIC_PREFIX);
        bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        bytes.extend_from_slice(metadata);
        bytes.extend_from_slice(b"erofs payload");
        bytes
    }

    #[test]
    fn reads_upstream_layer_header() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("demo.layer");
        let metadata = serde_json::to_vec(&json!({
            "info": {"id": "org.example.demo"},
            "version": "1"
        }))
        .unwrap();
        fs::write(&path, layer_bytes(&metadata)).unwrap();

        let info = read_layer_info(path).unwrap();
        assert_eq!(info.version, "1");
        assert_eq!(info.info["id"], "org.example.demo");
    }

    #[test]
    fn rejects_magic_and_truncated_metadata() {
        let temporary = tempdir().unwrap();
        let bad_magic = temporary.path().join("bad-magic.layer");
        fs::write(&bad_magic, vec![0_u8; MAGIC_LENGTH + 4]).unwrap();
        assert!(matches!(
            read_layer_info(bad_magic),
            Err(LayerFileError::InvalidMagic)
        ));

        let truncated = temporary.path().join("truncated.layer");
        let mut bytes = layer_bytes(b"{}");
        bytes[MAGIC_LENGTH..MAGIC_LENGTH + 4].copy_from_slice(&100_u32.to_le_bytes());
        fs::write(&truncated, bytes).unwrap();
        assert!(matches!(
            read_layer_info(truncated),
            Err(LayerFileError::InvalidMetadataLength(100))
        ));
    }

    #[test]
    fn unpacks_erofs_payload_without_external_tools() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("demo.layer");
        let destination = temporary.path().join("unpacked");
        let info_json = br#"{"id":"org.example.demo"}"#.to_vec();
        let image = build_image(
            Node::Dir {
                mode: DEFAULT_DIR_MODE,
                entries: BTreeMap::from([
                    (
                        "info.json".to_string(),
                        Node::File {
                            mode: DEFAULT_FILE_MODE,
                            data: info_json.clone(),
                            meta: NodeMeta::default(),
                            xattrs: Vec::new(),
                        },
                    ),
                    (
                        "link".to_string(),
                        Node::Symlink {
                            mode: DEFAULT_SYMLINK_MODE,
                            target: "info.json".to_string(),
                            meta: NodeMeta::default(),
                            xattrs: Vec::new(),
                        },
                    ),
                ]),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
            12,
        )
        .unwrap();
        let metadata = serde_json::to_vec(&json!({
            "info": {"id": "org.example.demo"},
            "version": "1"
        }))
        .unwrap();
        let mut bytes = layer_bytes(&metadata);
        bytes.truncate(MAGIC_LENGTH + LENGTH_FIELD_SIZE as usize + metadata.len());
        bytes.extend_from_slice(&image);
        fs::write(&path, bytes).unwrap();

        let layer = unpack_layer(&path, &destination).unwrap();
        assert_eq!(layer.info["id"], "org.example.demo");
        assert_eq!(fs::read(destination.join("info.json")).unwrap(), info_json);
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("info.json")
        );
    }

    #[test]
    fn unpacks_fifo_entries() {
        let temporary = tempdir().unwrap();
        let image = build_image(
            Node::Dir {
                mode: DEFAULT_DIR_MODE,
                entries: BTreeMap::from([(
                    "notifications".to_string(),
                    Node::Special {
                        mode: 0x1000 | 0o640,
                        meta: NodeMeta::default(),
                        xattrs: Vec::new(),
                    },
                )]),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
            12,
        )
        .unwrap();
        let image_path = temporary.path().join("image.erofs");
        fs::write(&image_path, image).unwrap();
        let image_file = File::open(image_path).unwrap();
        let destination = temporary.path().join("unpacked");
        unpack_erofs_file(&image_file, 0, None, &destination).unwrap();
        assert!(
            fs::symlink_metadata(destination.join("notifications"))
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }
}
