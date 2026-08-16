use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs_core::{BlockRead, Error as BlockError};
use fs_erofs::{FileType as ErofsFileType, Filesystem as ErofsFilesystem, Inode};
use fuser::{
    BackgroundSession, FUSE_ROOT_ID, FileAttr, FileType, Filesystem as FuseFilesystem, MountOption,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyXattr, Request,
};

const ATTRIBUTE_TTL: Duration = Duration::from_secs(1);

struct OffsetDevice {
    file: File,
    offset: u64,
    size: u64,
}

impl OffsetDevice {
    fn new(file: File, offset: u64, size: u64) -> io::Result<Self> {
        let file_size = file.metadata()?.len();
        if offset.checked_add(size).is_none_or(|end| end > file_size) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EROFS section exceeds source file",
            ));
        }
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

pub struct ReadOnlyFilesystem {
    filesystem: ErofsFilesystem,
    nodes: HashMap<u64, u64>,
    identities: HashMap<u64, u64>,
    parents: HashMap<u64, u64>,
    next_inode: u64,
}

impl ReadOnlyFilesystem {
    pub fn new(file: File, offset: u64, size: u64) -> io::Result<Self> {
        let device = Arc::new(OffsetDevice::new(file, offset, size)?);
        let filesystem = ErofsFilesystem::open(device).map_err(erofs_error)?;
        let root = filesystem.root_inode().map_err(erofs_error)?;
        let mut nodes = HashMap::new();
        nodes.insert(FUSE_ROOT_ID, root.nid);
        let mut identities = HashMap::new();
        identities.insert(root.nid, FUSE_ROOT_ID);
        let mut parents = HashMap::new();
        parents.insert(FUSE_ROOT_ID, FUSE_ROOT_ID);
        Ok(Self {
            filesystem,
            nodes,
            identities,
            parents,
            next_inode: FUSE_ROOT_ID + 1,
        })
    }

    fn inode(&self, inode: u64) -> io::Result<Inode> {
        let nid = self
            .nodes
            .get(&inode)
            .copied()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        self.filesystem.read_inode(nid).map_err(erofs_error)
    }

    fn inode_for(&mut self, nid: u64, parent: u64) -> u64 {
        if let Some(inode) = self.identities.get(&nid) {
            return *inode;
        }
        let inode = self.next_inode;
        self.next_inode = self.next_inode.saturating_add(1);
        self.nodes.insert(inode, nid);
        self.identities.insert(nid, inode);
        self.parents.insert(inode, parent);
        inode
    }

    fn attributes(&self, inode: u64, node: &Inode) -> FileAttr {
        let timestamp = UNIX_EPOCH
            .checked_add(Duration::new(node.mtime, node.mtime_nsec.min(999_999_999)))
            .unwrap_or(UNIX_EPOCH);
        FileAttr {
            ino: inode,
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: timestamp,
            mtime: timestamp,
            ctime: timestamp,
            crtime: UNIX_EPOCH,
            kind: file_type(node.file_type()),
            perm: node.mode & 0o7777,
            nlink: node.nlink,
            uid: node.uid,
            gid: node.gid,
            rdev: node.raw_u,
            blksize: self
                .filesystem
                .superblock()
                .block_size()
                .try_into()
                .unwrap_or(u32::MAX),
            flags: 0,
        }
    }

    fn xattrs(&self, inode: u64) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let inode = self.inode(inode)?;
        self.filesystem.xattrs(&inode).map_err(erofs_error)
    }
}

impl FuseFilesystem for ReadOnlyFilesystem {
    fn lookup(&mut self, _request: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let result = (|| {
            if name.as_bytes().is_empty()
                || name.as_bytes().contains(&b'/')
                || name.as_bytes().contains(&0)
            {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            let directory = self.inode(parent)?;
            let node = self
                .filesystem
                .lookup(&directory, name.as_bytes())
                .map_err(erofs_error)?;
            let inode = self.inode_for(node.nid, parent);
            Ok::<_, io::Error>(self.attributes(inode, &node))
        })();
        match result {
            Ok(attributes) => reply.entry(&ATTRIBUTE_TTL, &attributes, 0),
            Err(error) => reply.error(error_code(&error)),
        }
    }

    fn getattr(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        _handle: Option<u64>,
        reply: ReplyAttr,
    ) {
        match self.inode(inode).map(|node| self.attributes(inode, &node)) {
            Ok(attributes) => reply.attr(&ATTRIBUTE_TTL, &attributes),
            Err(error) => reply.error(error_code(&error)),
        }
    }

    fn readlink(&mut self, _request: &Request<'_>, inode: u64, reply: ReplyData) {
        let result = self.inode(inode).and_then(|node| {
            self.filesystem
                .read_symlink_target(&node)
                .map_err(erofs_error)
        });
        match result {
            Ok(target) => reply.data(&target),
            Err(error) => reply.error(error_code(&error)),
        }
    }

    fn open(&mut self, _request: &Request<'_>, inode: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(libc::EROFS);
            return;
        }
        match self.inode(inode) {
            Ok(node) if node.is_regular_file() => reply.opened(0, 0),
            Ok(node) if node.is_dir() => reply.error(libc::EISDIR),
            Ok(_) => reply.error(libc::EINVAL),
            Err(error) => reply.error(error_code(&error)),
        }
    }

    fn read(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        _handle: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let result = (|| {
            let node = self.inode(inode)?;
            if !node.is_regular_file() {
                return Err(io::Error::from_raw_os_error(if node.is_dir() {
                    libc::EISDIR
                } else {
                    libc::EINVAL
                }));
            }
            let offset = offset as u64;
            if offset >= node.size {
                return Ok(Vec::new());
            }
            let length =
                usize::try_from((node.size - offset).min(u64::from(size))).unwrap_or(size as usize);
            let mut buffer = vec![0_u8; length];
            self.filesystem
                .read_file(&node, offset, &mut buffer)
                .map_err(erofs_error)?;
            Ok::<_, io::Error>(buffer)
        })();
        match result {
            Ok(buffer) => reply.data(&buffer),
            Err(error) => reply.error(error_code(&error)),
        }
    }

    fn opendir(&mut self, _request: &Request<'_>, inode: u64, _flags: i32, reply: ReplyOpen) {
        match self.inode(inode) {
            Ok(node) if node.is_dir() => reply.opened(0, 0),
            Ok(_) => reply.error(libc::ENOTDIR),
            Err(error) => reply.error(error_code(&error)),
        }
    }

    fn readdir(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        _handle: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let result = (|| {
            let directory = self.inode(inode)?;
            if !directory.is_dir() {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
            let parent = self.parents.get(&inode).copied().unwrap_or(FUSE_ROOT_ID);
            let mut entries = vec![
                (inode, FileType::Directory, OsString::from(".")),
                (parent, FileType::Directory, OsString::from("..")),
            ];
            let directory_entries = self.filesystem.read_dir(&directory).map_err(erofs_error)?;
            for entry in directory_entries {
                if matches!(entry.name.as_slice(), b"." | b"..") {
                    continue;
                }
                if entry.name.is_empty() || entry.name.contains(&b'/') || entry.name.contains(&0) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid EROFS directory entry name",
                    ));
                }
                let node = self.filesystem.read_inode(entry.nid).map_err(erofs_error)?;
                let child = self.inode_for(node.nid, inode);
                entries.push((
                    child,
                    file_type(node.file_type()),
                    OsString::from_vec(entry.name),
                ));
            }
            Ok::<_, io::Error>(entries)
        })();
        let entries = match result {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(error_code(&error));
                return;
            }
        };
        for (index, (entry_inode, kind, name)) in
            entries.into_iter().enumerate().skip(offset as usize)
        {
            if reply.add(entry_inode, (index + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&mut self, _request: &Request<'_>, _inode: u64, reply: ReplyStatfs) {
        let superblock = self.filesystem.superblock();
        let block_size = superblock.block_size().try_into().unwrap_or(u32::MAX);
        reply.statfs(
            u64::from(superblock.blocks),
            0,
            0,
            superblock.inos,
            0,
            block_size,
            255,
            block_size,
        );
    }

    fn getxattr(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let result = self.xattrs(inode).and_then(|attributes| {
            attributes
                .into_iter()
                .find_map(|(candidate, value)| (candidate == name.as_bytes()).then_some(value))
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODATA))
        });
        reply_xattr(result, size, reply);
    }

    fn listxattr(&mut self, _request: &Request<'_>, inode: u64, size: u32, reply: ReplyXattr) {
        let result = self.xattrs(inode).map(|attributes| {
            let mut names = Vec::new();
            for (name, _) in attributes {
                names.extend_from_slice(&name);
                names.push(0);
            }
            names
        });
        reply_xattr(result, size, reply);
    }

    fn access(&mut self, _request: &Request<'_>, inode: u64, _mask: i32, reply: ReplyEmpty) {
        match self.inode(inode) {
            Ok(_) => reply.ok(),
            Err(error) => reply.error(error_code(&error)),
        }
    }
}

pub fn mount_read_only(
    file: File,
    offset: u64,
    size: u64,
    mountpoint: impl AsRef<Path>,
) -> io::Result<BackgroundSession> {
    let options = [
        MountOption::RO,
        MountOption::DefaultPermissions,
        MountOption::FSName("erofs".to_string()),
        MountOption::Subtype("erofs".to_string()),
        MountOption::CUSTOM("nonempty".to_string()),
    ];
    let configured = env::var_os("FUSERMOUNT_PROG")
        .map(PathBuf::from)
        .or_else(find_fusermount);
    let path_guard = configured
        .as_deref()
        .map(FusermountPathGuard::new)
        .transpose()?;
    if env::var_os("UAB_EROFSFUSE_VERBOSE").is_some() {
        if let Some(configured) = configured {
            eprintln!("use fusermount:{}", configured.display());
        } else {
            eprintln!("fusermount not found");
        }
    }
    let result = fuser::spawn_mount2(
        ReadOnlyFilesystem::new(file, offset, size)?,
        mountpoint,
        &options,
    );
    drop(path_guard);
    result
}

struct FusermountPathGuard {
    previous_path: Option<OsString>,
    directory: PathBuf,
}

impl FusermountPathGuard {
    fn new(program: &Path) -> io::Result<Self> {
        let program = resolve_program(program)?;
        let directory = env::temp_dir().join(format!(
            "linyaps-fusermount-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir(&directory)?;
        if let Err(error) = symlink(&program, directory.join("fusermount3")) {
            let _ = fs::remove_dir(&directory);
            return Err(error);
        }
        let previous_path = env::var_os("PATH");
        let mut paths = vec![directory.clone()];
        if let Some(previous) = &previous_path {
            paths.extend(env::split_paths(previous));
        }
        let path = env::join_paths(paths)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        unsafe { env::set_var("PATH", path) };
        Ok(Self {
            previous_path,
            directory,
        })
    }
}

impl Drop for FusermountPathGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous_path {
            unsafe { env::set_var("PATH", previous) };
        } else {
            unsafe { env::remove_var("PATH") };
        }
        let _ = fs::remove_file(self.directory.join("fusermount3"));
        let _ = fs::remove_dir(&self.directory);
    }
}

fn resolve_program(program: &Path) -> io::Result<PathBuf> {
    if program.is_absolute() || program.components().count() > 1 {
        return if program.is_absolute() {
            Ok(program.to_path_buf())
        } else {
            Ok(env::current_dir()?.join(program))
        };
    }
    let path = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&path) {
        let candidate = directory.join(program);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", program.display()),
    ))
}

fn find_fusermount() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                if env::var_os("UAB_EROFSFUSE_VERBOSE").is_some() {
                    eprintln!("failed to open directory {}: {error}", directory.display());
                }
                continue;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let bytes = name.as_bytes();
            let Some(suffix) = bytes.strip_prefix(b"fusermount") else {
                continue;
            };
            if !suffix.iter().all(u8::is_ascii_digit) {
                continue;
            }
            let metadata = match fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    if env::var_os("UAB_EROFSFUSE_VERBOSE").is_some() {
                        eprintln!("stat error: {error}");
                    }
                    continue;
                }
            };
            if metadata.uid() != 0 || metadata.mode() & libc::S_ISUID == 0 {
                if env::var_os("UAB_EROFSFUSE_VERBOSE").is_some() {
                    eprintln!("skip {}", entry.path().display());
                }
                continue;
            }
            return Some(entry.path());
        }
    }
    None
}

fn file_type(kind: ErofsFileType) -> FileType {
    match kind {
        ErofsFileType::Dir => FileType::Directory,
        ErofsFileType::RegularFile => FileType::RegularFile,
        ErofsFileType::Symlink => FileType::Symlink,
        ErofsFileType::ChrDev => FileType::CharDevice,
        ErofsFileType::BlkDev => FileType::BlockDevice,
        ErofsFileType::Fifo => FileType::NamedPipe,
        ErofsFileType::Sock | ErofsFileType::Unknown => FileType::Socket,
    }
}

fn erofs_error(error: fs_erofs::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn error_code(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn reply_xattr(result: io::Result<Vec<u8>>, size: u32, reply: ReplyXattr) {
    match result {
        Ok(value) if size == 0 => reply.size(value.len().try_into().unwrap_or(u32::MAX)),
        Ok(value) if size as usize >= value.len() => reply.data(&value),
        Ok(_) => reply.error(libc::ERANGE),
        Err(error) => reply.error(error_code(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_erofs_directly_and_assigns_stable_fuse_inodes() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("file"), "data").unwrap();
        std::os::unix::fs::symlink("file", source.join("link")).unwrap();
        let image = linyaps_repository::build_erofs_image(&source).unwrap();
        let path = temporary.path().join("bundle.erofs");
        std::fs::write(&path, &image).unwrap();

        let mut filesystem =
            ReadOnlyFilesystem::new(File::open(path).unwrap(), 0, image.len() as u64).unwrap();
        let root = filesystem.inode(FUSE_ROOT_ID).unwrap();
        let file = filesystem.filesystem.lookup(&root, b"file").unwrap();
        let link = filesystem.filesystem.lookup(&root, b"link").unwrap();
        let first = filesystem.inode_for(file.nid, FUSE_ROOT_ID);
        assert_eq!(first, filesystem.inode_for(file.nid, FUSE_ROOT_ID));
        assert_eq!(file_type(file.file_type()), FileType::RegularFile);
        assert_eq!(file_type(link.file_type()), FileType::Symlink);
        let mut content = vec![0; file.size as usize];
        filesystem
            .filesystem
            .read_file(&file, 0, &mut content)
            .unwrap();
        assert_eq!(content, b"data");
    }

    #[test]
    fn rejects_erofs_sections_outside_the_backing_file() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("short");
        std::fs::write(&path, [0_u8; 16]).unwrap();
        assert!(OffsetDevice::new(File::open(path).unwrap(), 8, 9).is_err());
    }
}
