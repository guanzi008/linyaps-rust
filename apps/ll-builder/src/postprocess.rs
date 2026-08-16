use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use goblin::elf::Elf;
use linyaps_api::BuilderProject;
use linyaps_core::Architecture;

use crate::source::clear_path;

const CONFIGURATION_DIRECTORIES: &[(&str, &[&str])] = &[
    ("share/applications", &["desktop"]),
    ("share/applications/context-menus", &["conf"]),
    ("share/dbus-1/services", &["service"]),
    (
        "lib/systemd/user",
        &[
            "service",
            "socket",
            "device",
            "mount",
            "automount",
            "swap",
            "target",
            "path",
            "timer",
            "slice",
            "scope",
        ],
    ),
    (
        "share/systemd/user",
        &[
            "service",
            "socket",
            "device",
            "mount",
            "automount",
            "swap",
            "target",
            "path",
            "timer",
            "slice",
            "scope",
        ],
    ),
];

const LEGACY_CHECK_CONFIGURATION_DIRECTORIES: &[(&str, &[&str])] = &[
    ("share/applications", &["desktop"]),
    ("share/applications/context-menus", &["conf"]),
    ("share/dbus-1/services", &["service"]),
    (
        "lib/systemd/user",
        &[
            "service",
            "socket",
            "device",
            "mount",
            "automount",
            "swap",
            "target",
            "path",
            "timer",
            "slice",
            "scope",
        ],
    ),
];

pub fn rewrite_application_configuration(
    project: &BuilderProject,
    build_output: &Path,
) -> Result<()> {
    if project.package.kind != "app" {
        return Ok(());
    }
    for (relative, extensions) in CONFIGURATION_DIRECTORIES {
        let directory = build_output.join(relative);
        if !directory.is_dir() {
            continue;
        }
        for path in matching_files(&directory, extensions)? {
            rewrite_configuration_file(
                &path,
                &project.package.id,
                path.extension()
                    .is_some_and(|extension| extension == "desktop"),
            )?;
        }
    }
    Ok(())
}

pub fn validate_exported_configuration(
    project: &BuilderProject,
    files: &Path,
) -> Result<Vec<PathBuf>> {
    if project.package.kind != "app" {
        return Ok(Vec::new());
    }
    validate_exported_configuration_for_app(&project.package.id, files)
}

pub fn validate_exported_configuration_for_app(app_id: &str, files: &Path) -> Result<Vec<PathBuf>> {
    let mut invalid = Vec::new();
    for (relative, extensions) in LEGACY_CHECK_CONFIGURATION_DIRECTORIES {
        let directory = files.join(relative);
        if !directory.is_dir() {
            continue;
        }
        for path in matching_files_recursive(&directory, extensions)? {
            if !path
                .file_name()
                .is_some_and(|name| legacy_prefix_matches(name, app_id))
            {
                invalid.push(path);
            }
        }
    }
    invalid.sort();
    invalid.dedup();
    Ok(invalid)
}

pub fn strip_debug_symbols(build_output: &Path, install_prefix: &str) -> Result<()> {
    let mut files = Vec::new();
    collect_regular_files(build_output, build_output, &mut files)?;
    for relative in files {
        if relative
            .extension()
            .is_some_and(|extension| extension == "debug")
        {
            continue;
        }
        let path = build_output.join(&relative);
        let data = fs::read(&path)?;
        let Ok(elf) = Elf::parse(&data) else {
            continue;
        };
        if !has_section(&elf, ".symtab") {
            continue;
        }
        let build_id = build_id(&elf, &data).unwrap_or_default();
        let build_id_hex = hex(&build_id);
        let (directory, filename) = if build_id_hex.len() >= 2 {
            (&build_id_hex[..2], format!("{}.debug", &build_id_hex[2..]))
        } else {
            ("", ".debug".to_string())
        };
        let debug_relative = Path::new("lib/debug/.build-id")
            .join(directory)
            .join(filename);
        let debug_file = build_output.join(&debug_relative);
        if let Some(parent) = debug_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let strip = find_executable("LINGLONG_EU_STRIP", "eu-strip")?;
        let status = Command::new(&strip)
            .arg(&path)
            .arg("-f")
            .arg(&debug_file)
            .status()
            .with_context(|| format!("failed to execute {}", strip.display()))?;
        if !status.success() {
            bail!(
                "{} failed to strip {}: {status}",
                strip.display(),
                path.display()
            );
        }

        let installed_file = Path::new(install_prefix).join(&relative);
        let installed_relative = installed_file.strip_prefix("/").unwrap_or(&installed_file);
        let mut debug_link_name = installed_relative.as_os_str().to_os_string();
        debug_link_name.push(".debug");
        let debug_link_relative = Path::new("lib/debug").join(debug_link_name);
        let debug_link = build_output.join(debug_link_relative);
        if let Some(parent) = debug_link.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::symlink_metadata(&debug_link).is_ok() {
            clear_path(&debug_link)?;
        }
        let target = Path::new(install_prefix).join(&debug_relative);
        symlink(target, debug_link)?;
    }
    Ok(())
}

pub fn check_runtime_dependencies(
    project: &BuilderProject,
    application: &Path,
    base: &Path,
    runtime: Option<&Path>,
    output: &Path,
) -> Result<()> {
    check_runtime_dependencies_for_app(&project.package.id, application, base, runtime, output)
}

pub fn check_runtime_dependencies_for_app(
    app_id: &str,
    application: &Path,
    base: &Path,
    runtime: Option<&Path>,
    output: &Path,
) -> Result<()> {
    let application_prefix = PathBuf::from(format!("/opt/apps/{app_id}/files"));
    check_runtime_dependencies_with_mappings(
        application_prefix.clone(),
        &[(application_prefix, application.to_path_buf())],
        base,
        runtime,
        output,
    )
}

pub fn check_runtime_dependencies_for_paths(
    applications: &[PathBuf],
    base: &Path,
    runtime: Option<&Path>,
    output: &Path,
) -> Result<()> {
    if applications.is_empty() {
        bail!("no application paths were provided");
    }
    let mut mappings = Vec::new();
    for application in applications {
        let host = if application.is_absolute() {
            application.clone()
        } else {
            env::current_dir()?.join(application)
        };
        let virtual_root = normalize_absolute(&host)
            .with_context(|| format!("invalid application path: {}", application.display()))?;
        mappings.push((virtual_root, host));
    }
    check_runtime_dependencies_with_mappings(
        mappings[0].0.clone(),
        &mappings,
        base,
        runtime,
        output,
    )
}

fn check_runtime_dependencies_with_mappings(
    application_prefix: PathBuf,
    applications: &[(PathBuf, PathBuf)],
    base: &Path,
    runtime: Option<&Path>,
    output: &Path,
) -> Result<()> {
    let filesystem = VirtualFilesystem::new(application_prefix, applications, runtime, base)?;
    let mut executables = Vec::new();
    let mut queue = VecDeque::new();
    for (virtual_root, application) in applications {
        executables.clear();
        collect_regular_files(application, application, &mut executables)?;
        for relative in &executables {
            let virtual_path = virtual_root.join(relative);
            let Some(object) = DynamicObject::load(&virtual_path, &filesystem)? else {
                continue;
            };
            if object.has_libc_start {
                queue.push_back(object);
            }
        }
    }

    let mut visited = BTreeSet::new();
    let mut needed_base_files = BTreeSet::new();
    let mut missing = Vec::new();
    while let Some(object) = queue.pop_front() {
        if !visited.insert(object.virtual_path.clone()) {
            continue;
        }
        for library in &object.libraries {
            let Some(resolved) = filesystem.resolve_library(&object, library)? else {
                missing.push(format!(
                    "couldn't find dependency \"{library}\" of {}",
                    object.virtual_path.display()
                ));
                continue;
            };
            if !resolved.starts_with("/opt") && !resolved.starts_with("/runtime") {
                needed_base_files.insert(resolved.clone());
            }
            if !visited.contains(&resolved)
                && let Some(dependency) = DynamicObject::load(&resolved, &filesystem)?
            {
                queue.push_back(dependency);
            }
        }
    }
    if !missing.is_empty() {
        bail!("{}", missing.join("\n"));
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut yaml = "# DO NOT EDIT THIS FILE, GENERATED BY ldd-check.sh\n\ndepends:".to_string();
    if needed_base_files.is_empty() {
        yaml.push_str(" []\n");
    } else {
        yaml.push('\n');
        for path in needed_base_files {
            yaml.push_str(" - ");
            yaml.push_str(&path.to_string_lossy());
            yaml.push('\n');
        }
    }
    fs::write(output, yaml)?;
    Ok(())
}

struct DynamicObject {
    virtual_path: PathBuf,
    libraries: Vec<String>,
    rpaths: Vec<String>,
    runpaths: Vec<String>,
    has_libc_start: bool,
}

impl DynamicObject {
    fn load(path: &Path, filesystem: &VirtualFilesystem) -> Result<Option<Self>> {
        let Some(host_path) = filesystem.resolve_virtual_path(path)? else {
            return Ok(None);
        };
        let data = fs::read(&host_path)?;
        let Ok(elf) = Elf::parse(&data) else {
            return Ok(None);
        };
        let has_libc_start = elf.dynsyms.iter().any(|symbol| {
            symbol.st_shndx == 0
                && elf
                    .dynstrtab
                    .get_at(symbol.st_name)
                    .is_some_and(|name| name == "__libc_start_main")
        });
        Ok(Some(Self {
            virtual_path: normalize_absolute(path).context("invalid virtual ELF path")?,
            libraries: elf
                .libraries
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            rpaths: elf
                .rpaths
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            runpaths: elf
                .runpaths
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            has_libc_start,
        }))
    }
}

struct VirtualFilesystem {
    application_prefix: PathBuf,
    mappings: Vec<(PathBuf, PathBuf)>,
    libraries: BTreeMap<OsString, Vec<PathBuf>>,
    triplet: String,
}

impl VirtualFilesystem {
    fn new(
        application_prefix: PathBuf,
        applications: &[(PathBuf, PathBuf)],
        runtime: Option<&Path>,
        base: &Path,
    ) -> Result<Self> {
        let mut mappings = applications.to_vec();
        let mut libraries = BTreeMap::<OsString, Vec<PathBuf>>::new();
        for (virtual_root, host_root) in applications {
            index_library_candidates(host_root, host_root, virtual_root, &mut libraries)?;
        }
        if let Some(runtime) = runtime {
            mappings.push((PathBuf::from("/runtime"), runtime.to_path_buf()));
            index_library_candidates(runtime, runtime, Path::new("/runtime"), &mut libraries)?;
        }
        mappings.push((PathBuf::from("/"), base.to_path_buf()));
        for candidates in libraries.values_mut() {
            candidates.sort_by_key(|path| library_rank(path, &application_prefix));
            candidates.dedup();
        }
        Ok(Self {
            application_prefix,
            mappings,
            libraries,
            triplet: Architecture::current()?.triplet().to_string(),
        })
    }

    fn resolve_library(&self, object: &DynamicObject, library: &str) -> Result<Option<PathBuf>> {
        if library.contains('/') {
            let path = if Path::new(library).is_absolute() {
                PathBuf::from(library)
            } else {
                object
                    .virtual_path
                    .parent()
                    .unwrap_or(Path::new("/"))
                    .join(library)
            };
            return self.existing_virtual_path(&path);
        }
        let origin = object.virtual_path.parent().unwrap_or(Path::new("/"));
        let mut search = Vec::new();
        let dynamic_paths = if object.runpaths.is_empty() {
            &object.rpaths
        } else {
            &object.runpaths
        };
        for value in dynamic_paths {
            for path in value.split(':') {
                if let Some(path) = expand_dynamic_path(path, origin, &self.triplet) {
                    search.push(path);
                }
            }
        }
        search.extend(default_library_directories(
            &self.application_prefix,
            &self.triplet,
        ));
        for directory in search {
            if let Some(path) = self.existing_virtual_path(&directory.join(library))? {
                return Ok(Some(path));
            }
        }
        if let Some(candidates) = self.libraries.get(OsStr::new(library)) {
            for candidate in candidates {
                if let Some(path) = self.existing_virtual_path(candidate)? {
                    return Ok(Some(path));
                }
            }
        }
        Ok(None)
    }

    fn existing_virtual_path(&self, path: &Path) -> Result<Option<PathBuf>> {
        let Some(path) = normalize_absolute(path) else {
            return Ok(None);
        };
        Ok(self.resolve_virtual_path(&path)?.map(|_| path))
    }

    fn resolve_virtual_path(&self, path: &Path) -> Result<Option<PathBuf>> {
        let Some(mut current) = normalize_absolute(path) else {
            return Ok(None);
        };
        for _ in 0..40 {
            let Some(host_path) = self.host_path(&current) else {
                return Ok(None);
            };
            let metadata = match fs::symlink_metadata(&host_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_symlink() {
                return Ok(metadata.is_file().then_some(host_path));
            }
            let target = fs::read_link(host_path)?;
            let target = if target.is_absolute() {
                target
            } else {
                current.parent().unwrap_or(Path::new("/")).join(target)
            };
            let Some(normalized) = normalize_absolute(&target) else {
                return Ok(None);
            };
            current = normalized;
        }
        bail!("too many symbolic links while resolving {}", path.display())
    }

    fn host_path(&self, virtual_path: &Path) -> Option<PathBuf> {
        self.mappings.iter().find_map(|(virtual_root, host_root)| {
            virtual_path
                .strip_prefix(virtual_root)
                .ok()
                .map(|relative| host_root.join(relative))
        })
    }
}

fn index_library_candidates(
    root: &Path,
    directory: &Path,
    virtual_root: &Path,
    output: &mut BTreeMap<OsString, Vec<PathBuf>>,
) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            index_library_candidates(root, &path, virtual_root, output)?;
            continue;
        }
        if !(metadata.is_file() || metadata.file_type().is_symlink()) {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        if !name
            .as_encoded_bytes()
            .windows(3)
            .any(|window| window == b".so")
        {
            continue;
        }
        let relative = path.strip_prefix(root)?;
        output
            .entry(name.to_os_string())
            .or_default()
            .push(virtual_root.join(relative));
    }
    Ok(())
}

fn library_rank(path: &Path, application_prefix: &Path) -> (u8, usize, PathBuf) {
    let source = if path.starts_with(application_prefix) {
        0
    } else if path.starts_with("/runtime") {
        1
    } else {
        2
    };
    (source, path.components().count(), path.to_path_buf())
}

fn default_library_directories(application_prefix: &Path, triplet: &str) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    for root in [application_prefix, Path::new("/runtime"), Path::new("")] {
        for relative in [
            format!("lib/{triplet}"),
            format!("usr/lib/{triplet}"),
            "lib64".to_string(),
            "usr/lib64".to_string(),
            "lib".to_string(),
            "usr/lib".to_string(),
        ] {
            directories.push(if root.as_os_str().is_empty() {
                Path::new("/").join(relative)
            } else {
                root.join(relative)
            });
        }
    }
    directories
}

fn expand_dynamic_path(value: &str, origin: &Path, triplet: &str) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    let platform = env::consts::ARCH;
    let value = value
        .replace("${ORIGIN}", &origin.to_string_lossy())
        .replace("$ORIGIN", &origin.to_string_lossy())
        .replace("${LIB}", "lib")
        .replace("$LIB", "lib")
        .replace("${PLATFORM}", platform)
        .replace("$PLATFORM", platform)
        .replace("${TRIPLET}", triplet)
        .replace("$TRIPLET", triplet);
    let path = PathBuf::from(value);
    let resolved = if path.is_absolute() {
        path
    } else {
        origin.join(path)
    };
    normalize_absolute(&resolved)
}

fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn rewrite_configuration_file(path: &Path, app_id: &str, desktop: bool) -> Result<()> {
    let original = fs::read_to_string(path)
        .with_context(|| format!("application configuration is not UTF-8: {}", path.display()))?;
    let had_final_newline = original.ends_with('\n');
    let mut output = original
        .split_terminator('\n')
        .map(str::to_string)
        .collect::<Vec<_>>();
    if desktop {
        let mut marked = Vec::with_capacity(output.len());
        for line in output {
            let has_desktop_entry = line.contains("[Desktop Entry]");
            marked.push(line);
            if has_desktop_entry {
                marked.push(format!("X-linglong={app_id}"));
            }
        }
        output = marked
            .into_iter()
            .map(|line| {
                if line.contains("TryExec") {
                    "TryExec=ll-cli".to_string()
                } else {
                    line
                }
            })
            .collect();
    }
    for line in &mut output {
        if matches!(configuration_key(line), Some("Exec" | "ExecStart")) {
            let equals = line.find('=').expect("configuration key has equals sign");
            *line = format!(
                "{}/usr/bin/ll-cli run {app_id} -- {}",
                &line[..=equals],
                &line[equals + 1..]
            );
        }
    }
    let mut rewritten = output.join("\n");
    if had_final_newline {
        rewritten.push('\n');
    }
    fs::write(path, rewritten)?;
    Ok(())
}

fn legacy_prefix_matches(name: &OsStr, app_id: &str) -> bool {
    let name = name.as_bytes();
    let pattern = app_id.as_bytes();
    name.len() >= pattern.len()
        && pattern
            .iter()
            .zip(name)
            .all(|(expected, actual)| *expected == b'.' || expected == actual)
}

fn configuration_key(line: &str) -> Option<&str> {
    let equals = line.find('=')?;
    let key = line[..equals].trim_end_matches([' ', '\t']);
    (!key.is_empty() && !key.starts_with([' ', '\t'])).then_some(key)
}

fn matching_files(directory: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let mut files = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && has_extension(path, extensions))
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn matching_files_recursive(directory: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    collect_matching_files(directory, extensions, &mut output)?;
    output.sort();
    Ok(output)
}

fn collect_matching_files(
    directory: &Path,
    extensions: &[&str],
    output: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_matching_files(&path, extensions, output)?;
        } else if metadata.is_file() && has_extension(&path, extensions) {
            output.push(path);
        }
    }
    Ok(())
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extensions.contains(&extension))
}

fn collect_regular_files(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_regular_files(root, &path, output)?;
        } else if metadata.is_file() {
            output.push(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn has_section(elf: &Elf<'_>, name: &str) -> bool {
    elf.section_headers.iter().any(|section| {
        elf.shdr_strtab
            .get_at(section.sh_name)
            .is_some_and(|section_name| section_name == name)
    })
}

fn build_id(elf: &Elf<'_>, data: &[u8]) -> Option<Vec<u8>> {
    let section = elf.section_headers.iter().find(|section| {
        elf.shdr_strtab
            .get_at(section.sh_name)
            .is_some_and(|name| name == ".note.gnu.build-id")
    })?;
    let start = usize::try_from(section.sh_offset).ok()?;
    let size = usize::try_from(section.sh_size).ok()?;
    let notes = data.get(start..start.checked_add(size)?)?;
    let mut offset = 0_usize;
    while offset.checked_add(12)? <= notes.len() {
        let namesz = read_u32(&notes[offset..offset + 4], elf.little_endian)? as usize;
        let descsz = read_u32(&notes[offset + 4..offset + 8], elf.little_endian)? as usize;
        let note_type = read_u32(&notes[offset + 8..offset + 12], elf.little_endian)?;
        offset += 12;
        let name_end = offset.checked_add(namesz)?;
        let name = notes.get(offset..name_end)?;
        offset = align4(name_end)?;
        let desc_end = offset.checked_add(descsz)?;
        let descriptor = notes.get(offset..desc_end)?;
        offset = align4(desc_end)?;
        if note_type == 3 && name.strip_suffix(&[0]).unwrap_or(name) == b"GNU" {
            return Some(descriptor.to_vec());
        }
    }
    None
}

fn read_u32(bytes: &[u8], little_endian: bool) -> Option<u32> {
    let bytes: [u8; 4] = bytes.try_into().ok()?;
    Some(if little_endian {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn find_executable(variable: &str, name: &str) -> Result<PathBuf> {
    if let Some(path) = env::var_os(variable).map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "{} points to a missing executable: {}",
            variable,
            path.display()
        );
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!("couldn't find {name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn project() -> BuilderProject {
        serde_yml::from_str(
            r#"version: "1"
package:
  id: org.example.App
  name: Example
  version: 1.0.0.0
  kind: app
  description: Example
command: [example]
base: org.deepin.base/23.1.0
build: example
"#,
        )
        .unwrap()
    }

    #[test]
    fn rewrites_desktop_and_service_commands() {
        let temporary = tempdir().unwrap();
        let desktop = temporary
            .path()
            .join("share/applications/org.example.App.desktop");
        let service = temporary
            .path()
            .join("lib/systemd/user/org.example.App.service");
        fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        fs::create_dir_all(service.parent().unwrap()).unwrap();
        fs::write(
            &desktop,
            "[Desktop Entry]\nTryExec=/opt/demo\nExec =/opt/demo --flag\n",
        )
        .unwrap();
        fs::write(&service, "[Service]\nExecStart=/opt/demo --service\n").unwrap();

        rewrite_application_configuration(&project(), temporary.path()).unwrap();

        assert_eq!(
            fs::read_to_string(desktop).unwrap(),
            "[Desktop Entry]\nX-linglong=org.example.App\nTryExec=ll-cli\nExec =/usr/bin/ll-cli run org.example.App -- /opt/demo --flag\n"
        );
        assert_eq!(
            fs::read_to_string(service).unwrap(),
            "[Service]\nExecStart=/usr/bin/ll-cli run org.example.App -- /opt/demo --service\n"
        );
    }

    #[test]
    fn reports_configuration_without_application_prefix() {
        let temporary = tempdir().unwrap();
        let valid = temporary
            .path()
            .join("share/applications/org.example.App.desktop");
        let invalid = temporary.path().join("share/applications/demo.desktop");
        fs::create_dir_all(valid.parent().unwrap()).unwrap();
        fs::write(&valid, "valid").unwrap();
        fs::write(&invalid, "invalid").unwrap();
        assert_eq!(
            validate_exported_configuration(&project(), temporary.path()).unwrap(),
            [invalid]
        );
    }

    #[test]
    fn configuration_check_matches_frozen_grep_and_directory_set() {
        let temporary = tempdir().unwrap();
        let regex_match = temporary
            .path()
            .join("share/applications/orgXexampleYApp.desktop");
        let ignored = temporary
            .path()
            .join("share/systemd/user/not-prefixed.service");
        fs::create_dir_all(regex_match.parent().unwrap()).unwrap();
        fs::create_dir_all(ignored.parent().unwrap()).unwrap();
        fs::write(&regex_match, "accepted by grep").unwrap();
        fs::write(&ignored, "not checked by frozen helper").unwrap();

        assert!(
            validate_exported_configuration(&project(), temporary.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rewrite_matches_frozen_sed_substrings_and_crlf() {
        let temporary = tempdir().unwrap();
        let desktop = temporary
            .path()
            .join("share/applications/org.example.App.desktop");
        fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        fs::write(
            &desktop,
            "# [Desktop Entry] TryExec\nComment=TryExecNot\nExec =demo\r\n",
        )
        .unwrap();

        rewrite_application_configuration(&project(), temporary.path()).unwrap();

        assert_eq!(
            fs::read_to_string(desktop).unwrap(),
            "TryExec=ll-cli\nX-linglong=org.example.App\nTryExec=ll-cli\nExec =/usr/bin/ll-cli run org.example.App -- demo\r\n"
        );
    }

    #[test]
    fn parses_build_id_of_current_executable() {
        let executable = env::current_exe().unwrap();
        let data = fs::read(executable).unwrap();
        let elf = Elf::parse(&data).unwrap();
        assert!(!build_id(&elf, &data).unwrap().is_empty());
    }

    #[test]
    fn frozen_dependency_yaml_format_is_exact() {
        let temporary = tempdir().unwrap();
        let application = temporary.path().join("application");
        let base = temporary.path().join("base");
        let output = temporary.path().join("depends.yaml");
        fs::create_dir_all(&application).unwrap();
        fs::create_dir_all(&base).unwrap();

        check_runtime_dependencies_for_app("org.example.App", &application, &base, None, &output)
            .unwrap();

        assert_eq!(
            fs::read_to_string(output).unwrap(),
            "# DO NOT EDIT THIS FILE, GENERATED BY ldd-check.sh\n\ndepends: []\n"
        );
    }
}
