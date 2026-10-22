use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::path::{Path, PathBuf};

use linyaps_api::RunContextConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Architecture;

pub const EXTENSION_ID: &str = "org.deepin.driver.display.nvidia";

#[derive(Debug, thiserror::Error)]
pub enum HostNvidiaError {
    #[error("host NVIDIA driver: {0}")]
    Io(#[from] io::Error),
    #[error("host NVIDIA driver: {0}")]
    Invalid(String),
}

#[derive(Debug)]
struct Library {
    source: PathBuf,
    aliases: BTreeSet<String>,
}

#[derive(Debug)]
pub struct HostNvidia {
    pub name: String,
    pub fingerprint: String,
    architecture: Architecture,
    libraries: BTreeMap<PathBuf, Library>,
    manifests: BTreeMap<PathBuf, Value>,
    auxiliary: BTreeMap<PathBuf, PathBuf>,
    sockets: Vec<PathBuf>,
}

pub struct PreparedHostNvidia {
    pub mounts: Vec<Value>,
    pub environment: BTreeMap<String, String>,
}

pub fn extension_name() -> Option<String> {
    driver_version(Path::new("/"))
        .map(|version| format!("{EXTENSION_ID}.{}", version.replace('.', "-")))
}

pub fn is_nvidia_extension(name: &str) -> bool {
    name == EXTENSION_ID || name.starts_with(&format!("{EXTENSION_ID}."))
}

fn valid_version(value: &str) -> bool {
    value.len() <= 64
        && value.contains('.')
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn driver_version(root: &Path) -> Option<String> {
    if let Ok(version) = fs::read_to_string(root.join("sys/module/nvidia/version"))
        && valid_version(version.trim())
    {
        return Some(version.trim().to_string());
    }
    fs::read_to_string(root.join("proc/driver/nvidia/version"))
        .ok()?
        .lines()
        .find(|line| line.starts_with("NVRM version:"))?
        .split_whitespace()
        .find(|word| valid_version(word))
        .map(str::to_string)
}

impl HostNvidia {
    pub fn discover(architecture: Architecture) -> Result<Option<Self>, HostNvidiaError> {
        Self::discover_at(Path::new("/"), architecture)
    }

    pub fn from_config(
        config: &RunContextConfig,
        architecture: Architecture,
    ) -> Result<Option<Self>, HostNvidiaError> {
        let Some(name) = &config.host_nvidia_extension else {
            if config.host_nvidia_fingerprint.is_some() {
                return Err(HostNvidiaError::Invalid(
                    "fingerprint without hostNvidiaExtension".into(),
                ));
            }
            return Ok(None);
        };
        let host = Self::discover(architecture)?.ok_or_else(|| {
            HostNvidiaError::Invalid("kernel driver is no longer loaded; retry ll-cli run".into())
        })?;
        host.validate(name, config.host_nvidia_fingerprint.as_deref())?;
        Ok(Some(host))
    }

    fn validate(&self, name: &str, fingerprint: Option<&str>) -> Result<(), HostNvidiaError> {
        if name != self.name || fingerprint != Some(self.fingerprint.as_str()) {
            return Err(HostNvidiaError::Invalid(
                "driver files changed or run context is stale; retry ll-cli run".into(),
            ));
        }
        Ok(())
    }

    fn discover_at(
        root: &Path,
        architecture: Architecture,
    ) -> Result<Option<Self>, HostNvidiaError> {
        let Some(version) = driver_version(root) else {
            return Ok(None);
        };
        let mut host = Self {
            name: format!("{EXTENSION_ID}.{}", version.replace('.', "-")),
            fingerprint: String::new(),
            architecture,
            libraries: BTreeMap::new(),
            manifests: BTreeMap::new(),
            auxiliary: BTreeMap::new(),
            sockets: Vec::new(),
        };
        let mut directories = vec![
            format!("usr/lib/{}", architecture.triplet()),
            format!("lib/{}", architecture.triplet()),
        ];
        directories.extend(
            ["usr/lib64", "usr/lib", "lib64", "lib", "usr/lib32", "lib32"].map(str::to_string),
        );
        if architecture == Architecture::X86_64 {
            directories
                .extend(["usr/lib/i386-linux-gnu", "lib/i386-linux-gnu"].map(str::to_string));
        }
        let mut seen = BTreeSet::new();
        for directory in directories {
            for suffix in [
                "",
                "nvidia",
                "nvidia/current",
                "vdpau",
                "gbm",
                "nvidia/vdpau",
                "nvidia/current/vdpau",
            ] {
                let path = root.join(&directory).join(suffix);
                let Ok(entries) = fs::read_dir(path) else {
                    continue;
                };
                let mut entries = entries.collect::<Result<Vec<_>, _>>()?;
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let filename = entry.file_name().to_string_lossy().into_owned();
                    if !vendor_library(&filename) {
                        continue;
                    }
                    let Some(source) = trusted_file(root, &entry.path()) else {
                        continue;
                    };
                    let Some(canonical_name) = source.file_name().and_then(|name| name.to_str())
                    else {
                        continue;
                    };
                    if !vendor_library(canonical_name) || !matches_version(canonical_name, &version)
                    {
                        continue;
                    }
                    if !seen.insert(source.clone()) {
                        for library in host
                            .libraries
                            .values_mut()
                            .filter(|library| library.source == source)
                        {
                            library.aliases.insert(filename.clone());
                        }
                        continue;
                    }
                    let Ok(elf) = ElfLibrary::read(&source) else {
                        continue;
                    };
                    let Some(directory) = library_directory(architecture, elf.class, elf.machine)
                    else {
                        continue;
                    };
                    let mut aliases = BTreeSet::from([filename, canonical_name.to_string()]);
                    if let Some(soname) = elf
                        .soname
                        .filter(|soname| safe_filename(soname) && vendor_library(soname))
                    {
                        aliases.insert(soname);
                    }
                    host.libraries
                        .entry(Path::new(directory).join(canonical_name))
                        .or_insert(Library { source, aliases });
                }
            }
        }
        if !host.libraries.iter().any(|(path, library)| {
            path.parent() == Some(Path::new("orig"))
                && library.aliases.iter().any(|alias| {
                    [
                        "libcuda.so",
                        "libnvidia-ml.so",
                        "libEGL_nvidia.so",
                        "libGLX_nvidia.so",
                    ]
                    .iter()
                    .any(|prefix| alias.starts_with(prefix))
                })
        }) {
            return Err(HostNvidiaError::Invalid(format!(
                "no matching {architecture} user-space libraries for loaded driver {version}"
            )));
        }
        host.collect_manifests(root)?;
        for binary in [
            "nvidia-smi",
            "nvidia-debugdump",
            "nvidia-cuda-mps-control",
            "nvidia-cuda-mps-server",
        ] {
            for directory in ["usr/bin", "usr/sbin", "bin", "sbin"] {
                if let Some(source) = trusted_file(root, &root.join(directory).join(binary)) {
                    host.auxiliary.insert(Path::new("bin").join(binary), source);
                    break;
                }
            }
        }
        for source in [
            "usr/share/nvidia/nvoptix.bin",
            "usr/share/nvidia/files/nvoptix.bin",
        ] {
            if let Some(source) = trusted_file(root, &root.join(source)) {
                host.auxiliary
                    .insert(PathBuf::from("orig/nvoptix.bin"), source);
                break;
            }
        }
        for socket in [
            "run/nvidia-persistenced/socket",
            "run/nvidia-fabricmanager/socket",
        ] {
            let path = root.join(socket);
            if fs::metadata(&path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
                host.sockets.push(path);
            }
        }
        host.fingerprint = host.compute_fingerprint()?;
        Ok(Some(host))
    }

    fn collect_manifests(&mut self, root: &Path) -> Result<(), HostNvidiaError> {
        let aliases = self
            .libraries
            .values()
            .flat_map(|library| library.aliases.iter().cloned())
            .collect::<BTreeSet<_>>();
        for directory in [
            "glvnd/egl_vendor.d",
            "egl/egl_external_platform.d",
            "vulkan/icd.d",
            "vulkan/implicit_layer.d",
        ] {
            for base in ["etc", "usr/local/share", "usr/share"] {
                let Ok(entries) = fs::read_dir(root.join(base).join(directory)) else {
                    continue;
                };
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !name.to_ascii_lowercase().contains("nvidia") || !name.ends_with(".json") {
                        continue;
                    }
                    let Some(source) = trusted_file(root, &entry.path()) else {
                        continue;
                    };
                    if fs::metadata(&source)?.len() > 1024 * 1024 {
                        continue;
                    }
                    let Ok(mut manifest) = serde_json::from_slice::<Value>(&fs::read(source)?)
                    else {
                        continue;
                    };
                    if rewrite_library_paths(&mut manifest, &aliases).is_some_and(|count| count > 0)
                    {
                        self.manifests
                            .entry(Path::new("share").join(directory).join(name))
                            .or_insert(manifest);
                    }
                }
            }
        }
        Ok(())
    }

    fn compute_fingerprint(&self) -> Result<String, HostNvidiaError> {
        let mut digest = Sha256::new();
        digest.update(b"linyaps-host-nvidia-v1\0");
        digest.update(self.name.as_bytes());
        for (destination, source) in self
            .libraries
            .iter()
            .map(|(path, library)| (path, &library.source))
            .chain(self.auxiliary.iter())
        {
            let metadata = fs::metadata(source)?;
            digest.update(format!(
                "\0{}\0{}\0{}:{}:{}:{}:{}:{}:{}",
                destination.display(),
                source.display(),
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec()
            ));
        }
        for library in self.libraries.values() {
            for alias in &library.aliases {
                digest.update(alias.as_bytes());
                digest.update([0]);
            }
        }
        for (path, manifest) in &self.manifests {
            digest.update(path.as_os_str().as_encoded_bytes());
            digest.update(manifest.to_string());
        }
        Ok(crate::hex_encode(digest.finalize()))
    }

    pub fn ld_conf(&self) -> String {
        let prefix = format!("/opt/extensions/{}", self.name);
        let mut paths = BTreeSet::new();
        for library in self.libraries.keys() {
            paths.insert(format!(
                "{prefix}/{}\n",
                library.parent().unwrap().display()
            ));
        }
        paths.into_iter().collect()
    }

    pub fn prepare(
        &self,
        bundle: &Path,
        expose_vulkan: bool,
    ) -> Result<PreparedHostNvidia, HostNvidiaError> {
        let root = bundle.join("host-extensions").join(&self.name);
        fs::create_dir_all(&root)?;
        let prefix = Path::new("/opt/extensions").join(&self.name);
        let mut mounts = vec![readonly_bind(&root, &prefix)];
        for (relative, library) in &self.libraries {
            let target = root.join(relative);
            fs::create_dir_all(target.parent().unwrap())?;
            File::create(&target)?;
            mounts.push(readonly_bind(&library.source, &prefix.join(relative)));
        }
        for (relative, library) in &self.libraries {
            for alias in &library.aliases {
                let link = root.join(relative.parent().unwrap()).join(alias);
                if fs::symlink_metadata(&link).is_err() {
                    symlink(relative.file_name().unwrap(), link)?;
                }
            }
        }
        for (relative, source) in &self.auxiliary {
            let target = root.join(relative);
            fs::create_dir_all(target.parent().unwrap())?;
            File::create(target)?;
            mounts.push(readonly_bind(source, &prefix.join(relative)));
        }
        let mut environment = BTreeMap::new();
        let ld_path = self.ld_conf().lines().collect::<Vec<_>>().join(":");
        environment.insert("LD_LIBRARY_PATH".into(), ld_path);
        environment.insert(
            "NVIDIA_CTK_LIBCUDA_DIR".into(),
            prefix.join("orig").display().to_string(),
        );
        if self
            .libraries
            .values()
            .any(|library| library.aliases.contains("nvidia-drm_gbm.so"))
        {
            environment.insert(
                "GBM_BACKENDS_PATH".into(),
                format!(
                    "{}/orig:{}/orig/32:/usr/lib/{}/gbm:/usr/lib64/gbm:/usr/lib/gbm:/lib64/gbm:/lib/gbm",
                    prefix.display(),
                    prefix.display(),
                    self.architecture.triplet()
                ),
            );
        }
        if self.auxiliary.keys().any(|path| path.starts_with("bin")) {
            environment.insert("PATH".into(), prefix.join("bin").display().to_string());
        }
        for (relative, manifest) in &self.manifests {
            let directory = relative.parent().unwrap();
            let category = directory.file_name().unwrap().to_str().unwrap();
            if !expose_vulkan && matches!(category, "icd.d" | "implicit_layer.d") {
                continue;
            }
            let target = root.join(relative);
            fs::create_dir_all(target.parent().unwrap())?;
            fs::write(&target, manifest.to_string())?;
            let container_directory = prefix.join(directory).display().to_string();
            match category {
                "egl_vendor.d" => merge_path(
                    &mut environment,
                    "__EGL_VENDOR_LIBRARY_DIRS",
                    &format!(
                        "{container_directory}:/etc/glvnd/egl_vendor.d:/usr/share/glvnd/egl_vendor.d"
                    ),
                ),
                "egl_external_platform.d" => {
                    for key in [
                        "EGL_EXTERNAL_PLATFORM_CONFIG_DIRS",
                        "__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS",
                    ] {
                        merge_path(
                            &mut environment,
                            key,
                            &format!(
                                "{container_directory}:/etc/egl/egl_external_platform.d:/usr/share/egl/egl_external_platform.d"
                            ),
                        );
                    }
                }
                "icd.d" => merge_path(
                    &mut environment,
                    "VK_ADD_DRIVER_FILES",
                    &prefix.join(relative).display().to_string(),
                ),
                "implicit_layer.d" => mounts.push(readonly_bind(
                    &target,
                    &Path::new("/etc/vulkan/implicit_layer.d").join(format!(
                        "linyaps-host-{}",
                        relative.file_name().unwrap().to_string_lossy()
                    )),
                )),
                _ => {}
            }
        }
        for socket in &self.sockets {
            mounts.push(readonly_bind(socket, socket));
        }
        Ok(PreparedHostNvidia {
            mounts,
            environment,
        })
    }
}

impl PreparedHostNvidia {
    pub fn apply(self, mounts: &mut Vec<Value>, environment: &mut BTreeMap<String, String>) {
        mounts.extend(self.mounts);
        for (key, value) in self.environment {
            if matches!(key.as_str(), "NVIDIA_CTK_LIBCUDA_DIR" | "GBM_BACKENDS_PATH") {
                environment.entry(key).or_insert(value);
            } else {
                merge_path(environment, &key, &value);
            }
        }
    }
}

fn merge_path(environment: &mut BTreeMap<String, String>, key: &str, value: &str) {
    let original = environment.get(key).map(String::as_str).unwrap_or_default();
    let mut values = Vec::new();
    for entry in value
        .split(':')
        .chain(original.split(':'))
        .filter(|entry| !entry.is_empty())
    {
        if !values.contains(&entry) {
            values.push(entry);
        }
    }
    environment.insert(key.to_string(), values.join(":"));
}

fn readonly_bind(source: &Path, destination: &Path) -> Value {
    json!({"source": source, "destination": destination, "type": "bind", "options": ["bind", "ro", "nosuid", "nodev"]})
}

fn safe_filename(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0', ':'])
}

fn vendor_library(name: &str) -> bool {
    name.contains(".so")
        && [
            "libnvidia-",
            "libcuda.so",
            "libnvcuvid.so",
            "libnvoptix.so",
            "libGLX_nvidia.so",
            "libEGL_nvidia.so",
            "libGLESv1_CM_nvidia.so",
            "libGLESv2_nvidia.so",
            "libvdpau_nvidia.so",
            "nvidia-drm_gbm.so",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn matches_version(name: &str, version: &str) -> bool {
    [
        "libnvidia-egl-wayland.so",
        "libnvidia-egl-wayland2.so",
        "libnvidia-egl-gbm.so",
        "libnvidia-egl-xcb.so",
        "libnvidia-egl-xlib.so",
        "libnvidia-api.so",
        "nvidia-drm_gbm.so",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || name.ends_with(&format!(".so.{version}"))
}

fn trusted_file(root: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    if !["usr", "lib", "lib64", "lib32", "etc", "opt", "bin", "sbin"]
        .iter()
        .any(|directory| canonical.starts_with(root.join(directory)))
        || !canonical.is_file()
    {
        return None;
    }
    Some(canonical)
}

fn rewrite_library_paths(value: &mut Value, aliases: &BTreeSet<String>) -> Option<usize> {
    let mut count = 0;
    match value {
        Value::Object(object) => {
            if let Some(path) = object.get_mut("library_path") {
                let filename = Path::new(path.as_str()?).file_name()?.to_str()?;
                if !aliases.contains(filename) {
                    return None;
                }
                *path = Value::String(filename.to_string());
                count += 1;
            }
            for (key, value) in object {
                if key != "library_path" {
                    count += rewrite_library_paths(value, aliases)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                count += rewrite_library_paths(value, aliases)?;
            }
        }
        _ => {}
    }
    Some(count)
}

fn library_directory(architecture: Architecture, class: u8, machine: u16) -> Option<&'static str> {
    let native = match architecture {
        Architecture::X86_64 => 62,
        Architecture::Arm64 => 183,
        Architecture::LoongArch64 | Architecture::Loong64 => 258,
        Architecture::RiscV64 => 243,
        Architecture::Sw64 => 268,
        Architecture::Mips64 => 8,
        Architecture::Unknown => return None,
    };
    if class == 2
        && (machine == native || (architecture == Architecture::Sw64 && machine == 0x9916))
    {
        Some("orig")
    } else if architecture == Architecture::X86_64 && class == 1 && machine == 3 {
        Some("orig/32")
    } else {
        None
    }
}

struct ElfLibrary {
    class: u8,
    machine: u16,
    soname: Option<String>,
}

impl ElfLibrary {
    fn read(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let header = read_region(&mut file, 0, 64)?;
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid NVIDIA ELF library");
        if &header[..4] != b"\x7fELF"
            || !matches!(header[4], 1 | 2)
            || header[5] != 1
            || header[6] != 1
            || number(&header, 16, 2) != 3
        {
            return Err(invalid());
        }
        let class = header[4];
        let machine = number(&header, 18, 2) as u16;
        let (phoff, entry_size, count, word) = if class == 2 {
            (
                number(&header, 32, 8),
                number(&header, 54, 2),
                number(&header, 56, 2),
                8,
            )
        } else {
            (
                number(&header, 28, 4),
                number(&header, 42, 2),
                number(&header, 44, 2),
                4,
            )
        };
        if count > 1024 || entry_size != if class == 2 { 56 } else { 32 } {
            return Err(invalid());
        }
        let headers = read_region(&mut file, phoff, (entry_size * count) as usize)?;
        let mut loads = Vec::new();
        let mut dynamic = None;
        for program in headers.chunks_exact(entry_size as usize) {
            let kind = number(program, 0, 4);
            let (offset, address, size) = if class == 2 {
                (
                    number(program, 8, 8),
                    number(program, 16, 8),
                    number(program, 32, 8),
                )
            } else {
                (
                    number(program, 4, 4),
                    number(program, 8, 4),
                    number(program, 16, 4),
                )
            };
            if kind == 1 {
                loads.push((offset, address, size));
            } else if kind == 2 {
                dynamic = Some((offset, size));
            }
        }
        let mut soname = None;
        if let Some((offset, size)) = dynamic {
            if size > 1024 * 1024 {
                return Err(invalid());
            }
            let data = read_region(&mut file, offset, size as usize)?;
            let mut string_table = None;
            let mut string_size = None;
            let mut name_offset = None;
            for entry in data.chunks_exact(word * 2) {
                let value = number(entry, word, word);
                match number(entry, 0, word) {
                    0 => break,
                    5 => string_table = Some(value),
                    10 => string_size = Some(value),
                    14 => name_offset = Some(value),
                    _ => {}
                }
            }
            if let (Some(table), Some(size), Some(name)) = (string_table, string_size, name_offset)
            {
                let address = table.checked_add(name).ok_or_else(invalid)?;
                let (offset, start, length) = loads
                    .iter()
                    .find(|(_, start, length)| address >= *start && address - start < *length)
                    .ok_or_else(invalid)?;
                let remaining = size
                    .checked_sub(name)
                    .ok_or_else(invalid)?
                    .min(length - (address - start))
                    .min(256);
                let data = read_region(
                    &mut file,
                    offset.checked_add(address - start).ok_or_else(invalid)?,
                    remaining as usize,
                )?;
                let end = data
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(invalid)?;
                soname = Some(
                    std::str::from_utf8(&data[..end])
                        .map_err(|_| invalid())?
                        .to_string(),
                );
            }
        }
        Ok(Self {
            class,
            machine,
            soname,
        })
    }
}

fn number(bytes: &[u8], offset: usize, length: usize) -> u64 {
    let mut value = [0; 8];
    value[..length].copy_from_slice(&bytes[offset..offset + length]);
    u64::from_le_bytes(value)
}

fn read_region(file: &mut File, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    if offset.checked_add(length as u64).is_none_or(|end| {
        end > file
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or_default()
    }) {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated NVIDIA ELF library",
        ));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_number(bytes: &mut [u8], offset: usize, length: usize, value: u64) {
        bytes[offset..offset + length].copy_from_slice(&value.to_le_bytes()[..length]);
    }

    fn elf_library(class: u8, machine: u16, soname: &str) -> Vec<u8> {
        let mut bytes = vec![0; 512];
        bytes[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', class, 1, 1]);
        put_number(&mut bytes, 16, 2, 3);
        put_number(&mut bytes, 18, 2, u64::from(machine));
        put_number(&mut bytes, 20, 4, 1);
        let word = if class == 2 { 8 } else { 4 };
        let entry_size = if class == 2 { 56 } else { 32 };
        if class == 2 {
            put_number(&mut bytes, 52, 2, 64);
            put_number(&mut bytes, 32, 8, 64);
            put_number(&mut bytes, 54, 2, entry_size);
            put_number(&mut bytes, 56, 2, 2);
        } else {
            put_number(&mut bytes, 40, 2, 52);
            put_number(&mut bytes, 28, 4, 64);
            put_number(&mut bytes, 42, 2, entry_size);
            put_number(&mut bytes, 44, 2, 2);
        }
        for (index, kind, offset, size) in [(0, 1, 0, 512), (1, 2, 192, word * 8)] {
            let start = 64 + index * entry_size as usize;
            put_number(&mut bytes, start, 4, kind);
            if class == 2 {
                put_number(&mut bytes, start + 8, 8, offset);
                put_number(&mut bytes, start + 16, 8, offset);
                put_number(&mut bytes, start + 32, 8, size as u64);
            } else {
                put_number(&mut bytes, start + 4, 4, offset);
                put_number(&mut bytes, start + 8, 4, offset);
                put_number(&mut bytes, start + 16, 4, size as u64);
            }
        }
        for (index, tag, value) in [(0, 5, 320), (1, 10, soname.len() as u64 + 1), (2, 14, 0)] {
            let start = 192 + index * word * 2;
            put_number(&mut bytes, start, word, tag);
            put_number(&mut bytes, start + word, word, value);
        }
        bytes[320..320 + soname.len()].copy_from_slice(soname.as_bytes());
        bytes
    }

    fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
        let destination = root.join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, contents).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "sys/module/nvidia/version", "580.95.05\n");
        write(
            root.path(),
            "usr/lib/libcuda.so.580.95.05",
            elf_library(2, 62, "libcuda.so.1"),
        );
        root
    }

    fn discover(root: &Path) -> HostNvidia {
        HostNvidia::discover_at(root, Architecture::X86_64)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn detects_loaded_driver_and_validates_proc_fallback() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            HostNvidia::discover_at(root.path(), Architecture::X86_64)
                .unwrap()
                .is_none()
        );
        write(
            root.path(),
            "proc/driver/nvidia/version",
            "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  580.95.05  Release Build\nGCC version: gcc 15.2.1\n",
        );
        assert_eq!(driver_version(root.path()).as_deref(), Some("580.95.05"));
        write(root.path(), "sys/module/nvidia/version", "590.1\n");
        assert_eq!(driver_version(root.path()).as_deref(), Some("590.1"));
        for invalid in [
            "", "580", "../580.1", "580..1", "580.1/x", "580.1:0", "580.1\0",
        ] {
            assert!(!valid_version(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn library_architectures_never_mix_native_and_foreign_machines() {
        for (architecture, machine) in [
            (Architecture::X86_64, 62),
            (Architecture::Arm64, 183),
            (Architecture::LoongArch64, 258),
            (Architecture::Loong64, 258),
            (Architecture::RiscV64, 243),
            (Architecture::Mips64, 8),
            (Architecture::Sw64, 268),
            (Architecture::Sw64, 0x9916),
        ] {
            assert_eq!(library_directory(architecture, 2, machine), Some("orig"));
            assert_eq!(library_directory(architecture, 1, machine), None);
        }
        assert_eq!(
            library_directory(Architecture::X86_64, 1, 3),
            Some("orig/32")
        );
        assert_eq!(library_directory(Architecture::Arm64, 2, 62), None);
        assert_eq!(library_directory(Architecture::Unknown, 2, 62), None);
    }

    #[test]
    fn reads_sonames_for_native_compat_and_large_sparse_libraries() {
        let root = fixture();
        let path = root.path().join("usr/lib/libcuda.so.580.95.05");
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(512 * 1024 * 1024)
            .unwrap();
        assert_eq!(
            ElfLibrary::read(&path).unwrap().soname.as_deref(),
            Some("libcuda.so.1")
        );
        write(
            root.path(),
            "usr/lib32/libcuda.so.580.95.05",
            elf_library(1, 3, "libcuda.so.1"),
        );
        let host = discover(root.path());
        assert_eq!(host.libraries.len(), 2);
        for directory in ["orig", "orig/32"] {
            let library = &host.libraries[&Path::new(directory).join("libcuda.so.580.95.05")];
            assert!(library.aliases.contains("libcuda.so.1"));
        }
    }

    #[test]
    fn rejects_wrong_version_architecture_untrusted_files_and_malformed_elf() {
        let root = fixture();
        write(
            root.path(),
            "usr/lib/libnvidia-ml.so.570.1",
            elf_library(2, 62, "libnvidia-ml.so.1"),
        );
        write(
            root.path(),
            "usr/lib/libnvcuvid.so.580.95.05",
            elf_library(2, 183, "libnvcuvid.so.1"),
        );
        write(
            root.path(),
            "usr/lib/libnvidia-glcore.so.580.95.05",
            b"not ELF",
        );
        write(
            root.path(),
            "usr/lib/libc.so.580.95.05",
            elf_library(2, 62, "libc.so.6"),
        );
        write(
            root.path(),
            "home/libnvidia-ml.so.580.95.05",
            elf_library(2, 62, "libnvidia-ml.so.1"),
        );
        symlink(
            "../../home/libnvidia-ml.so.580.95.05",
            root.path().join("usr/lib/libnvidia-ml.so.580.95.05"),
        )
        .unwrap();
        assert_eq!(discover(root.path()).libraries.len(), 1);
        write(root.path(), "sys/module/nvidia/version", "590.1");
        assert!(HostNvidia::discover_at(root.path(), Architecture::X86_64).is_err());
    }

    #[test]
    fn supports_debian_alternatives_and_independently_versioned_egl() {
        let root = fixture();
        write(
            root.path(),
            "usr/lib/x86_64-linux-gnu/nvidia/current/libEGL_nvidia.so.580.95.05",
            elf_library(2, 62, "libEGL_nvidia.so.0"),
        );
        symlink(
            "nvidia/current/libEGL_nvidia.so.580.95.05",
            root.path()
                .join("usr/lib/x86_64-linux-gnu/libEGL_nvidia.so.0"),
        )
        .unwrap();
        write(
            root.path(),
            "usr/lib/libnvidia-egl-wayland.so.1.1.20",
            elf_library(2, 62, "libnvidia-egl-wayland.so.1"),
        );
        let host = discover(root.path());
        assert_eq!(host.libraries.len(), 3);
        assert!(
            host.libraries[Path::new("orig/libEGL_nvidia.so.580.95.05")]
                .aliases
                .contains("libEGL_nvidia.so.0")
        );
    }

    #[test]
    fn manifests_are_rewritten_and_vulkan_is_additive_and_cdi_aware() {
        let root = fixture();
        write(
            root.path(),
            "usr/lib/libGLX_nvidia.so.580.95.05",
            elf_library(2, 62, "libGLX_nvidia.so.0"),
        );
        write(
            root.path(),
            "usr/lib/libEGL_nvidia.so.580.95.05",
            elf_library(2, 62, "libEGL_nvidia.so.0"),
        );
        write(
            root.path(),
            "usr/share/vulkan/icd.d/nvidia_icd.json",
            json!({"ICD":{"library_path":"/usr/lib/libGLX_nvidia.so.0", "api_version":"1.3.0"}})
                .to_string(),
        );
        write(
            root.path(),
            "usr/share/glvnd/egl_vendor.d/10_nvidia.json",
            json!({"ICD":{"library_path":"/usr/lib/libEGL_nvidia.so.0"}}).to_string(),
        );
        write(
            root.path(),
            "usr/share/vulkan/implicit_layer.d/nvidia_layers.json",
            json!({"layers":[{"name":"VK_LAYER_NV_optimus", "library_path":"/usr/lib/libGLX_nvidia.so.0"}]}).to_string(),
        );
        write(root.path(), "usr/share/vulkan/icd.d/nvidia_bad.json", "{");
        write(
            root.path(),
            "usr/share/vulkan/icd.d/nvidia_missing.json",
            json!({"ICD":{"library_path":"/evil/libmissing.so"}}).to_string(),
        );
        let host = discover(root.path());
        assert_eq!(host.manifests.len(), 3);
        assert_eq!(
            host.manifests[Path::new("share/vulkan/icd.d/nvidia_icd.json")]["ICD"]["library_path"],
            "libGLX_nvidia.so.0"
        );
        let bundle = root.path().join("bundle");
        let prepared = host.prepare(&bundle, true).unwrap();
        let mut mounts = Vec::new();
        let mut environment = BTreeMap::from([
            ("LD_LIBRARY_PATH".into(), "/app/lib".into()),
            ("VK_ADD_DRIVER_FILES".into(), "/existing/icd.json".into()),
        ]);
        prepared.apply(&mut mounts, &mut environment);
        assert!(environment["LD_LIBRARY_PATH"].ends_with(":/app/lib"));
        assert!(environment["VK_ADD_DRIVER_FILES"].ends_with(":/existing/icd.json"));
        assert!(!environment.contains_key("VK_ICD_FILENAMES"));
        assert!(!environment.contains_key("VK_ADD_IMPLICIT_LAYER_PATH"));
        assert!(!environment.contains_key("XDG_DATA_DIRS"));
        let implicit_layer = mounts
            .iter()
            .find(|mount| {
                mount["destination"]
                    == "/etc/vulkan/implicit_layer.d/linyaps-host-nvidia_layers.json"
            })
            .unwrap();
        let implicit_manifest: Value =
            serde_json::from_slice(&fs::read(implicit_layer["source"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(
            implicit_manifest["layers"][0]["library_path"],
            "libGLX_nvidia.so.0"
        );
        assert!(environment["__EGL_VENDOR_LIBRARY_DIRS"].contains("/usr/share/glvnd/egl_vendor.d"));
        assert!(
            mounts
                .iter()
                .all(|mount| mount["options"].as_array().unwrap().contains(&json!("ro")))
        );
        assert_eq!(
            mounts[0]["destination"],
            format!("/opt/extensions/{}", host.name)
        );
        assert_eq!(
            fs::read_link(
                bundle
                    .join("host-extensions")
                    .join(&host.name)
                    .join("orig/libcuda.so.1")
            )
            .unwrap(),
            Path::new("libcuda.so.580.95.05")
        );
        let explicit_cdi = host
            .prepare(&root.path().join("cdi-bundle"), false)
            .unwrap();
        assert!(!explicit_cdi.environment.contains_key("VK_ADD_DRIVER_FILES"));
        assert!(!explicit_cdi.mounts.iter().any(|mount| {
            mount["destination"]
                .as_str()
                .unwrap()
                .starts_with("/etc/vulkan/")
        }));
    }

    #[test]
    fn fingerprint_invalidates_same_version_replacements_and_manifest_changes() {
        let root = fixture();
        let first = discover(root.path());
        assert_eq!(first.fingerprint, discover(root.path()).fingerprint);
        first
            .validate(&first.name, Some(&first.fingerprint))
            .unwrap();
        assert!(
            first
                .validate("../../outside", Some(&first.fingerprint))
                .is_err()
        );
        assert!(first.validate(&first.name, None).is_err());
        write(
            root.path(),
            "usr/lib/replacement",
            elf_library(2, 62, "libcuda.so.1"),
        );
        fs::rename(
            root.path().join("usr/lib/replacement"),
            root.path().join("usr/lib/libcuda.so.580.95.05"),
        )
        .unwrap();
        let replacement = discover(root.path());
        assert_ne!(first.fingerprint, replacement.fingerprint);
        assert!(
            replacement
                .validate(&first.name, Some(&first.fingerprint))
                .is_err()
        );
        write(
            root.path(),
            "usr/share/vulkan/icd.d/nvidia_icd.json",
            json!({"ICD":{"library_path":"libcuda.so.1"}}).to_string(),
        );
        assert_ne!(replacement.fingerprint, discover(root.path()).fingerprint);
    }

    #[test]
    fn bounded_elf_parser_rejects_truncated_and_overflowing_tables() {
        let root = tempfile::tempdir().unwrap();
        for class in [1, 2] {
            let valid = elf_library(class, if class == 2 { 62 } else { 3 }, "libcuda.so.1");
            for length in [0, 7, 63, 90, 220, 320] {
                write(root.path(), "bad.so", &valid[..length]);
                assert!(ElfLibrary::read(&root.path().join("bad.so")).is_err());
            }
        }
        let mut bytes = elf_library(2, 62, "libcuda.so.1");
        put_number(&mut bytes, 32, 8, u64::MAX);
        write(root.path(), "bad.so", bytes);
        assert!(ElfLibrary::read(&root.path().join("bad.so")).is_err());
    }

    #[test]
    fn forwards_real_driver_sockets_without_launching_daemons() {
        let root = fixture();
        let socket = root.path().join("run/nvidia-persistenced/socket");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let host = discover(root.path());
        assert_eq!(host.sockets, vec![socket]);
    }

    #[test]
    fn wayland2_and_gbm_use_selected_libraries_without_becoming_a_driver() {
        let root = fixture();
        write(
            root.path(),
            "usr/lib/libnvidia-egl-wayland2.so.1.0.2",
            elf_library(2, 62, "libnvidia-egl-wayland2.so.1"),
        );
        write(
            root.path(),
            "usr/lib/libnvidia-allocator.so.580.95.05",
            elf_library(2, 62, "libnvidia-allocator.so.1"),
        );
        fs::create_dir_all(root.path().join("usr/lib/gbm")).unwrap();
        symlink(
            "../libnvidia-allocator.so.580.95.05",
            root.path().join("usr/lib/gbm/nvidia-drm_gbm.so"),
        )
        .unwrap();
        let host = discover(root.path());
        assert_eq!(host.libraries.len(), 3);
        let prepared = host.prepare(&root.path().join("bundle"), true).unwrap();
        let gbm_paths = prepared.environment["GBM_BACKENDS_PATH"]
            .split(':')
            .collect::<Vec<_>>();
        assert_eq!(gbm_paths[0], format!("/opt/extensions/{}/orig", host.name));
        assert!(gbm_paths.contains(&"/usr/lib/x86_64-linux-gnu/gbm"));
        assert!(gbm_paths.contains(&"/usr/lib64/gbm"));
        assert!(gbm_paths.contains(&"/usr/lib/gbm"));
        let mut environment = BTreeMap::from([("GBM_BACKENDS_PATH".into(), "/custom/gbm".into())]);
        prepared.apply(&mut Vec::new(), &mut environment);
        assert_eq!(environment["GBM_BACKENDS_PATH"], "/custom/gbm");
        write(root.path(), "sys/module/nvidia/version", "590.1");
        assert!(HostNvidia::discover_at(root.path(), Architecture::X86_64).is_err());
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    #[ignore = "requires Linux user namespaces and LINYAPS_TEST_OCI_RUNTIME pointing to ll-box"]
    fn oci_readonly_mounts_and_ldconfig_use_the_same_layout() {
        let runtime = std::env::var_os("LINYAPS_TEST_OCI_RUNTIME")
            .expect("set LINYAPS_TEST_OCI_RUNTIME to the ll-box executable");
        let temporary = fixture();
        let host = discover(temporary.path());
        let bundle = temporary.path().join("bundle");
        let rootfs = bundle.join("rootfs");
        let cache = temporary.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(rootfs.join("etc")).unwrap();
        fs::create_dir_all(rootfs.join("proc")).unwrap();
        fs::create_dir_all(rootfs.join("dev")).unwrap();
        fs::create_dir_all(rootfs.join("run/cache")).unwrap();
        fs::create_dir_all(rootfs.join("opt/extensions").join(&host.name)).unwrap();
        fs::write(rootfs.join("etc/ld.so.conf"), host.ld_conf()).unwrap();
        let mut mounts = vec![
            json!({"source":"proc", "destination":"/proc", "type":"proc"}),
            json!({"source":"tmpfs", "destination":"/dev", "type":"tmpfs", "options":["nosuid", "mode=755"]}),
        ];
        for directory in ["bin", "sbin", "usr", "lib", "lib64"] {
            let source = Path::new("/").join(directory);
            if source.is_dir() {
                fs::create_dir_all(rootfs.join(directory)).unwrap();
                mounts.push(json!({"source":source, "destination":source, "type":"bind", "options":["rbind", "ro"]}));
            }
        }
        mounts.push(
            json!({"source": cache, "destination":"/run/cache", "type":"bind", "options":["bind"]}),
        );
        let mut environment =
            BTreeMap::from([("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into())]);
        host.prepare(&bundle, false)
            .unwrap()
            .apply(&mut mounts, &mut environment);
        let script = format!(
            "set -eu; prefix=/opt/extensions/{}/orig; test -s \"$prefix/libcuda.so.1\"; test \"$(readlink \"$prefix/libcuda.so.1\")\" = libcuda.so.580.95.05; if touch \"$prefix/forbidden\" 2>/dev/null; then exit 1; fi; if printf bad >\"$prefix/libcuda.so.1\" 2>/dev/null; then exit 1; fi; /sbin/ldconfig -X -C /run/cache/ld.so.cache; /sbin/ldconfig -p -C /run/cache/ld.so.cache | grep -F \"$prefix/libcuda.so.1\"",
            host.name
        );
        let configuration = json!({
            "ociVersion":"1.0.2", "root":{"path":"rootfs", "readonly":true},
            "process":{"args":["/bin/sh", "-c", script], "cwd":"/", "user":{"uid":0,"gid":0}, "env":environment.into_iter().map(|(key,value)| format!("{key}={value}")).collect::<Vec<_>>()},
            "mounts":mounts, "linux":{
                "namespaces":[{"type":"user"},{"type":"mount"},{"type":"pid"}],
                "uidMappings":[{"containerID":0,"hostID":0,"size":1}],
                "gidMappings":[{"containerID":0,"hostID":0,"size":1}]
            }
        });
        fs::write(bundle.join("config.json"), configuration.to_string()).unwrap();
        let output = std::process::Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--propagation",
                "private",
            ])
            .arg(runtime)
            .arg("--root")
            .arg(temporary.path().join("state"))
            .args(["--cgroup-manager=disabled", "run", "--bundle"])
            .arg(&bundle)
            .arg("host-nvidia-mount-test")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(cache.join("ld.so.cache").is_file());
    }
}
