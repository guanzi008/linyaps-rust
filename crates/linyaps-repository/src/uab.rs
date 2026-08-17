use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use linyaps_api::UabMetaInfo;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{LayerFileError, TarError, extract_tar, unpack_erofs_file};

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ELFDATA2MSB: u8 = 2;
const SHN_XINDEX: u16 = 0xffff;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UabSection {
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Error)]
pub enum UabError {
    #[error("failed to read UAB: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid ELF file: {0}")]
    InvalidElf(String),
    #[error("UAB section not found: {0}")]
    MissingSection(String),
    #[error("failed to parse UAB metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("UAB bundle digest mismatch: expected {expected}, calculated {calculated}")]
    DigestMismatch {
        expected: String,
        calculated: String,
    },
    #[error("failed to unpack UAB bundle: {0}")]
    Erofs(#[from] LayerFileError),
    #[error("failed to unpack UAB signature data: {0}")]
    Tar(#[from] TarError),
}

#[derive(Debug)]
pub struct UabFile {
    file: File,
    sections: BTreeMap<String, UabSection>,
}

impl UabFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, UabError> {
        Self::from_file(File::open(path)?)
    }

    pub fn from_file(file: File) -> Result<Self, UabError> {
        let sections = parse_sections(&file)?;
        Ok(Self { file, sections })
    }

    pub fn section(&self, name: &str) -> Result<&UabSection, UabError> {
        self.sections
            .get(name)
            .ok_or_else(|| UabError::MissingSection(name.to_string()))
    }

    pub fn metadata(&self) -> Result<UabMetaInfo, UabError> {
        let data = self.read_section("linglong.meta")?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn verify(&self) -> Result<(), UabError> {
        let metadata = self.metadata()?;
        let bundle = self.section(&metadata.sections.bundle)?;
        let calculated = digest_section(&self.file, bundle)?;
        if calculated != metadata.digest {
            return Err(UabError::DigestMismatch {
                expected: metadata.digest,
                calculated,
            });
        }
        Ok(())
    }

    pub fn unpack_bundle(&self, destination: impl AsRef<Path>) -> Result<UabMetaInfo, UabError> {
        let metadata = self.metadata()?;
        let bundle = self.section(&metadata.sections.bundle)?;
        unpack_erofs_file(&self.file, bundle.offset, Some(bundle.size), destination)?;
        Ok(metadata)
    }

    pub fn bundle_source(&self) -> Result<(File, UabSection), UabError> {
        let metadata = self.metadata()?;
        let section = self.section(&metadata.sections.bundle)?.clone();
        Ok((self.file.try_clone()?, section))
    }

    pub fn extract_sign_data(&self, destination_root: impl AsRef<Path>) -> Result<bool, UabError> {
        let Some(section) = self.sections.get("linglong.bundle.sign") else {
            return Ok(false);
        };
        let size = usize::try_from(section.size).map_err(|_| {
            UabError::InvalidElf("section linglong.bundle.sign is too large".to_string())
        })?;
        let mut archive = vec![0; size];
        self.file.read_exact_at(&mut archive, section.offset)?;
        let destination = destination_root
            .as_ref()
            .join("entries/share/deepin-elf-verify/.elfsign");
        extract_tar(&archive, destination)?;
        Ok(true)
    }

    pub fn read_section(&self, name: &str) -> Result<Vec<u8>, UabError> {
        let section = self.section(name)?;
        let size = usize::try_from(section.size)
            .map_err(|_| UabError::InvalidElf(format!("section {name} is too large")))?;
        let mut data = vec![0; size];
        self.file.read_exact_at(&mut data, section.offset)?;
        Ok(data)
    }
}

fn digest_section(file: &File, section: &UabSection) -> Result<String, UabError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut offset = section.offset;
    let mut remaining = section.size;
    while remaining != 0 {
        let length = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        file.read_exact_at(&mut buffer[..length], offset)?;
        hasher.update(&buffer[..length]);
        offset += length as u64;
        remaining -= length as u64;
    }
    Ok(linyaps_core::hex_encode(hasher.finalize()))
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, bytes: &[u8]) -> u16 {
        let bytes = [bytes[0], bytes[1]];
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }

    fn u32(self, bytes: &[u8]) -> u32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }

    fn u64(self, bytes: &[u8]) -> u64 {
        let bytes = [
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ];
        match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        }
    }
}

#[derive(Clone, Copy)]
struct RawSection {
    name: u32,
    offset: u64,
    size: u64,
    link: u32,
}

fn parse_sections(file: &File) -> Result<BTreeMap<String, UabSection>, UabError> {
    let file_size = file.metadata()?.len();
    let mut header = [0_u8; 64];
    file.read_exact_at(&mut header, 0)?;
    if &header[..4] != ELF_MAGIC {
        return Err(UabError::InvalidElf("invalid magic".to_string()));
    }
    let class = header[4];
    let endian = match header[5] {
        ELFDATA2LSB => Endian::Little,
        ELFDATA2MSB => Endian::Big,
        value => {
            return Err(UabError::InvalidElf(format!(
                "unknown ELF data encoding {value}"
            )));
        }
    };
    let (section_offset, entry_size, mut section_count, mut names_index, minimum_entry_size) =
        match class {
            ELFCLASS32 => (
                u64::from(endian.u32(&header[32..36])),
                u64::from(endian.u16(&header[46..48])),
                u64::from(endian.u16(&header[48..50])),
                u64::from(endian.u16(&header[50..52])),
                40_u64,
            ),
            ELFCLASS64 => (
                endian.u64(&header[40..48]),
                u64::from(endian.u16(&header[58..60])),
                u64::from(endian.u16(&header[60..62])),
                u64::from(endian.u16(&header[62..64])),
                64_u64,
            ),
            value => return Err(UabError::InvalidElf(format!("unknown ELF class {value}"))),
        };
    if section_offset == 0 || entry_size < minimum_entry_size {
        return Err(UabError::InvalidElf(
            "missing or invalid section header table".to_string(),
        ));
    }
    let first = read_raw_section(file, class, endian, section_offset, entry_size, file_size)?;
    if section_count == 0 {
        section_count = first.size;
    }
    if names_index == u64::from(SHN_XINDEX) {
        names_index = u64::from(first.link);
    }
    if section_count == 0 || section_count > 1_000_000 || names_index >= section_count {
        return Err(UabError::InvalidElf(
            "invalid section count or string table index".to_string(),
        ));
    }
    let table_size = entry_size
        .checked_mul(section_count)
        .and_then(|size| section_offset.checked_add(size))
        .ok_or_else(|| UabError::InvalidElf("section table overflow".to_string()))?;
    if table_size > file_size {
        return Err(UabError::InvalidElf(
            "section table exceeds file".to_string(),
        ));
    }

    let mut raw_sections = Vec::with_capacity(section_count as usize);
    for index in 0..section_count {
        raw_sections.push(read_raw_section(
            file,
            class,
            endian,
            section_offset + index * entry_size,
            entry_size,
            file_size,
        )?);
    }
    let names = raw_sections[names_index as usize];
    let names_size = usize::try_from(names.size)
        .map_err(|_| UabError::InvalidElf("section name table is too large".to_string()))?;
    let mut names_data = vec![0_u8; names_size];
    file.read_exact_at(&mut names_data, names.offset)?;

    let mut sections = BTreeMap::new();
    for raw in raw_sections {
        let name_offset = raw.name as usize;
        if name_offset >= names_data.len() {
            continue;
        }
        let end = names_data[name_offset..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| name_offset + offset)
            .unwrap_or(names_data.len());
        let name = std::str::from_utf8(&names_data[name_offset..end])
            .map_err(|_| UabError::InvalidElf("non-UTF-8 section name".to_string()))?;
        if !name.is_empty() {
            sections.insert(
                name.to_string(),
                UabSection {
                    offset: raw.offset,
                    size: raw.size,
                },
            );
        }
    }
    Ok(sections)
}

fn read_raw_section(
    file: &File,
    class: u8,
    endian: Endian,
    offset: u64,
    entry_size: u64,
    file_size: u64,
) -> Result<RawSection, UabError> {
    if offset
        .checked_add(entry_size)
        .is_none_or(|end| end > file_size)
    {
        return Err(UabError::InvalidElf(
            "section header exceeds file".to_string(),
        ));
    }
    let size = usize::try_from(entry_size)
        .map_err(|_| UabError::InvalidElf("section header is too large".to_string()))?;
    let mut data = vec![0_u8; size];
    file.read_exact_at(&mut data, offset)?;
    let section = if class == ELFCLASS32 {
        RawSection {
            name: endian.u32(&data[0..4]),
            offset: u64::from(endian.u32(&data[16..20])),
            size: u64::from(endian.u32(&data[20..24])),
            link: endian.u32(&data[24..28]),
        }
    } else {
        RawSection {
            name: endian.u32(&data[0..4]),
            offset: endian.u64(&data[24..32]),
            size: endian.u64(&data[32..40]),
            link: endian.u32(&data[40..44]),
        }
    };
    if section
        .offset
        .checked_add(section.size)
        .is_none_or(|end| end > file_size)
    {
        return Err(UabError::InvalidElf("section exceeds file".to_string()));
    }
    Ok(section)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use linyaps_api::{PackageInfoV2, UabLayer, UabSections};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn reads_metadata_and_verifies_bundle_digest() {
        let bundle = b"bundle payload";
        let digest = linyaps_core::hex_encode(Sha256::digest(bundle));
        let metadata = UabMetaInfo {
            digest,
            layers: vec![UabLayer {
                info: PackageInfoV2 {
                    arch: vec!["x86_64".to_string()],
                    base: String::new(),
                    channel: "main".to_string(),
                    command: None,
                    compatible_version: None,
                    description: None,
                    extension_implementation: None,
                    extensions: None,
                    id: "org.example.demo".to_string(),
                    kind: "app".to_string(),
                    module: "binary".to_string(),
                    name: "Demo".to_string(),
                    permissions: None,
                    runtime: None,
                    schema_version: "1.0".to_string(),
                    size: 0,
                    uuid: None,
                    version: "1.0.0.0".to_string(),
                },
                minified: false,
            }],
            only_app: Some(false),
            sections: UabSections {
                bundle: "linglong.bundle".to_string(),
                icon: None,
            },
            uuid: "00000000-0000-4000-8000-000000000000".to_string(),
            version: "1".to_string(),
        };
        let bytes = elf_fixture(&serde_json::to_vec(&metadata).unwrap(), bundle);
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("demo.uab");
        fs::write(&path, bytes).unwrap();
        let uab = UabFile::open(path).unwrap();
        assert_eq!(uab.metadata().unwrap(), metadata);
        uab.verify().unwrap();
        assert!(
            !uab.extract_sign_data(temporary.path().join("unsigned"))
                .unwrap()
        );
    }

    #[test]
    fn extracts_signature_tar_section() {
        let temporary = tempdir().unwrap();
        let mut archive = tar_entry("./hello", b"Hello, World!");
        archive.extend([0_u8; 1024]);
        let path = temporary.path().join("signed.uab");
        crate::append_elf_sections(
            std::env::current_exe().unwrap(),
            &path,
            &[("linglong.bundle.sign", archive.as_slice())],
        )
        .unwrap();

        let uab = UabFile::open(path).unwrap();
        let overlay = temporary.path().join("overlay");
        assert!(uab.extract_sign_data(&overlay).unwrap());
        assert_eq!(
            fs::read(overlay.join("entries/share/deepin-elf-verify/.elfsign/hello")).unwrap(),
            b"Hello, World!"
        );
    }

    fn tar_entry(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[148..156].fill(b' ');
        let checksum = header.iter().map(|byte| usize::from(*byte)).sum::<usize>();
        header[148..156].copy_from_slice(format!("{:06o}\0 ", checksum).as_bytes());
        let mut output = header.to_vec();
        output.extend_from_slice(data);
        output.resize(output.len().next_multiple_of(512), 0);
        output
    }

    fn elf_fixture(metadata: &[u8], bundle: &[u8]) -> Vec<u8> {
        let names = b"\0linglong.meta\0linglong.bundle\0.shstrtab\0";
        let metadata_offset = 64_u64;
        let bundle_offset = metadata_offset + metadata.len() as u64;
        let names_offset = bundle_offset + bundle.len() as u64;
        let section_offset = (names_offset + names.len() as u64 + 7) & !7;
        let mut bytes = vec![0_u8; section_offset as usize + 4 * 64];
        bytes[..4].copy_from_slice(ELF_MAGIC);
        bytes[4] = ELFCLASS64;
        bytes[5] = ELFDATA2LSB;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[40..48].copy_from_slice(&section_offset.to_le_bytes());
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64_u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&4_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&3_u16.to_le_bytes());
        bytes[metadata_offset as usize..bundle_offset as usize].copy_from_slice(metadata);
        bytes[bundle_offset as usize..names_offset as usize].copy_from_slice(bundle);
        bytes[names_offset as usize..names_offset as usize + names.len()].copy_from_slice(names);
        write_section(
            &mut bytes,
            section_offset + 64,
            1,
            metadata_offset,
            metadata.len() as u64,
        );
        write_section(
            &mut bytes,
            section_offset + 128,
            15,
            bundle_offset,
            bundle.len() as u64,
        );
        write_section(
            &mut bytes,
            section_offset + 192,
            31,
            names_offset,
            names.len() as u64,
        );
        bytes
    }

    fn write_section(bytes: &mut [u8], offset: u64, name: u32, data: u64, size: u64) {
        let offset = offset as usize;
        bytes[offset..offset + 4].copy_from_slice(&name.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&1_u32.to_le_bytes());
        bytes[offset + 24..offset + 32].copy_from_slice(&data.to_le_bytes());
        bytes[offset + 32..offset + 40].copy_from_slice(&size.to_le_bytes());
        bytes[offset + 48..offset + 56].copy_from_slice(&1_u64.to_le_bytes());
    }
}
