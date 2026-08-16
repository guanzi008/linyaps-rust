use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use md5::Md5;
use patchkit::apply::ApplyOptions;
use patchkit::apply_tree::{ApplyToTreeOptions, apply_to_tree};
use patchkit::quilt::{Series, SeriesEntry};
use patchkit::unified::{PlainOrBinaryPatch, UnifiedPatch};
use sha1::Sha1;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ChecksumAlgorithm {
    Md5,
    Sha1,
    Sha256,
}

#[derive(Debug)]
struct DscFile {
    algorithm: ChecksumAlgorithm,
    name: String,
    digest: String,
    size: u64,
    path: PathBuf,
}

#[derive(Debug)]
struct Dsc {
    format: String,
    files: Vec<DscFile>,
}

pub async fn extract(
    descriptor: &Path,
    descriptor_url: &str,
    working: &Path,
    destination: &Path,
) -> Result<()> {
    let fields = parse_control(&fs::read(descriptor)?)?;
    let format = required_field(&fields, "format")?.trim().to_string();
    let mut dsc = Dsc {
        format,
        files: parse_associated_files(&fields)?,
    };
    if dsc.files.is_empty() {
        bail!("Debian source descriptor has no associated files");
    }
    let downloads = working.join("downloads");
    fs::create_dir_all(&downloads)?;
    for file in &mut dsc.files {
        file.path = downloads.join(&file.name);
        download_related_file(descriptor_url, file).await?;
    }
    let staged_descriptor = downloads.join(
        descriptor
            .file_name()
            .context("Debian source descriptor has no file name")?,
    );
    fs::copy(descriptor, &staged_descriptor)?;
    crate::source::clear_path(destination)?;
    fs::create_dir_all(destination)?;
    let rust_result = match dsc.format.as_str() {
        "1.0" => extract_v1(&dsc, working, destination),
        "3.0 (native)" => extract_native(&dsc, working, destination),
        "3.0 (quilt)" => extract_quilt(&dsc, working, destination),
        other => Err(anyhow::anyhow!("unsupported Debian source format: {other}")),
    };
    if rust_result.is_ok() {
        return Ok(());
    }
    crate::source::clear_path(destination)?;
    let dpkg_source =
        std::env::var_os("LINGLONG_DPKG_SOURCE").unwrap_or_else(|| "dpkg-source".into());
    extract_with_dpkg_source(&dpkg_source, &staged_descriptor, destination).map_err(|fallback| {
        let rust_error = rust_result.expect_err("checked above");
        anyhow::anyhow!(
            "Debian source extraction failed ({rust_error:#}); dpkg-source fallback failed: {fallback:#}"
        )
    })
}

fn extract_with_dpkg_source(
    executable: &OsStr,
    descriptor: &Path,
    destination: &Path,
) -> Result<()> {
    let status = Command::new(executable)
        .args(["-x", "--no-copy"])
        .arg(descriptor)
        .arg(destination)
        .status()
        .with_context(|| format!("failed to execute {}", Path::new(executable).display()))?;
    if !status.success() {
        bail!("dpkg-source exited with {status}");
    }
    Ok(())
}

fn parse_control(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let text =
        String::from_utf8(bytes.to_vec()).context("Debian source descriptor is not UTF-8")?;
    let cleartext = clear_signed_payload(&text)?;
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for raw_line in cleartext.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with([' ', '\t']) {
            let name = current
                .as_ref()
                .context("orphan continuation line in Debian descriptor")?;
            let value = fields.get_mut(name).expect("current field exists");
            value.push('\n');
            value.push_str(&line[1..]);
            continue;
        }
        if line.is_empty() {
            current = None;
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .with_context(|| format!("invalid Debian descriptor line: {line}"))?;
        let name = name.to_ascii_lowercase();
        if fields
            .insert(name.clone(), value.trim_start().to_string())
            .is_some()
        {
            bail!("duplicate Debian descriptor field: {name}");
        }
        current = Some(name);
    }
    Ok(fields)
}

fn clear_signed_payload(text: &str) -> Result<String> {
    if !text.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") {
        return Ok(text.to_string());
    }
    let mut lines = text.lines();
    let first = lines.next();
    debug_assert_eq!(first, Some("-----BEGIN PGP SIGNED MESSAGE-----"));
    for line in lines.by_ref() {
        if line.trim_end_matches('\r').is_empty() {
            break;
        }
    }
    let mut payload = String::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line == "-----BEGIN PGP SIGNATURE-----" {
            return Ok(payload);
        }
        payload.push_str(line.strip_prefix("- ").unwrap_or(line));
        payload.push('\n');
    }
    bail!("unterminated clear-signed Debian source descriptor")
}

fn required_field<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    fields
        .get(name)
        .map(String::as_str)
        .with_context(|| format!("Debian source descriptor is missing {name}"))
}

fn parse_associated_files(fields: &BTreeMap<String, String>) -> Result<Vec<DscFile>> {
    let mut files = BTreeMap::<String, DscFile>::new();
    for (field, algorithm) in [
        ("files", ChecksumAlgorithm::Md5),
        ("checksums-sha1", ChecksumAlgorithm::Sha1),
        ("checksums-sha256", ChecksumAlgorithm::Sha256),
    ] {
        let Some(value) = fields.get(field) else {
            continue;
        };
        for candidate in parse_checksums(value, field, algorithm)? {
            if let Some(current) = files.get_mut(&candidate.name) {
                if current.size != candidate.size {
                    bail!(
                        "Debian source file {} has inconsistent sizes {} and {}",
                        candidate.name,
                        current.size,
                        candidate.size
                    );
                }
                if candidate.algorithm > current.algorithm {
                    current.algorithm = candidate.algorithm;
                    current.digest = candidate.digest;
                }
            } else {
                files.insert(candidate.name.clone(), candidate);
            }
        }
    }
    if files.is_empty() {
        bail!("Debian source descriptor has no associated file checksums");
    }
    Ok(files.into_values().collect())
}

fn parse_checksums(value: &str, field: &str, algorithm: ChecksumAlgorithm) -> Result<Vec<DscFile>> {
    let mut files = Vec::new();
    let mut names = BTreeSet::new();
    for line in value.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let digest = parts
            .next()
            .with_context(|| format!("missing digest in Debian {field} field"))?;
        let size = parts
            .next()
            .context("missing file size in Debian descriptor")?
            .parse::<u64>()
            .context("invalid file size in Debian descriptor")?;
        let name = parts
            .next()
            .context("missing file name in Debian descriptor")?;
        if parts.next().is_some() {
            bail!("invalid Debian {field} line: {line}");
        }
        let expected_length = match algorithm {
            ChecksumAlgorithm::Md5 => 32,
            ChecksumAlgorithm::Sha1 => 40,
            ChecksumAlgorithm::Sha256 => 64,
        };
        if digest.len() != expected_length || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid {algorithm:?} digest for {name}");
        }
        validate_file_name(name)?;
        if !names.insert(name.to_string()) {
            bail!("duplicate Debian source file {name} in {field}");
        }
        files.push(DscFile {
            algorithm,
            name: name.to_string(),
            digest: digest.to_ascii_lowercase(),
            size,
            path: PathBuf::new(),
        });
    }
    Ok(files)
}

fn validate_file_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    if path.file_name() != Some(OsStr::new(name)) || name.is_empty() {
        bail!("unsafe associated Debian source file name: {name}");
    }
    Ok(())
}

async fn download_related_file(descriptor_url: &str, file: &DscFile) -> Result<()> {
    linyaps_core::tls::install_default_provider();
    if let Some(parent) = local_parent(descriptor_url) {
        fs::copy(parent.join(&file.name), &file.path)
            .with_context(|| format!("failed to read Debian source file {}", file.name))?;
    } else {
        let base = reqwest::Url::parse(descriptor_url)?;
        let url = base.join(&file.name)?;
        let response = reqwest::get(url).await?.error_for_status()?;
        let bytes = response.bytes().await?;
        fs::write(&file.path, bytes)?;
    }
    let metadata = fs::metadata(&file.path)?;
    if metadata.len() != file.size {
        bail!(
            "Debian source file {} has size {}, expected {}",
            file.name,
            metadata.len(),
            file.size
        );
    }
    let actual = checksum_file(&file.path, file.algorithm)?;
    if actual != file.digest {
        bail!(
            "Debian source file {} {:?} digest is {actual}, expected {}",
            file.name,
            file.algorithm,
            file.digest
        );
    }
    Ok(())
}

fn local_parent(url: &str) -> Option<PathBuf> {
    let path = url
        .strip_prefix("file://")
        .map(Path::new)
        .or_else(|| (!url.contains("://")).then(|| Path::new(url)))?;
    path.parent().map(Path::to_path_buf)
}

fn sha256_file(path: &Path) -> Result<String> {
    digest_file::<Sha256>(path)
}

fn checksum_file(path: &Path, algorithm: ChecksumAlgorithm) -> Result<String> {
    match algorithm {
        ChecksumAlgorithm::Md5 => digest_file::<Md5>(path),
        ChecksumAlgorithm::Sha1 => digest_file::<Sha1>(path),
        ChecksumAlgorithm::Sha256 => sha256_file(path),
    }
}

fn digest_file<D: Digest + Default>(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = D::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn extract_native(dsc: &Dsc, working: &Path, destination: &Path) -> Result<()> {
    let archives = tar_files(dsc);
    if archives.len() != 1 {
        bail!("3.0 (native) source must contain exactly one tar archive");
    }
    extract_tar_overlay(&archives[0].path, working, destination, true, 0)
}

fn extract_v1(dsc: &Dsc, working: &Path, destination: &Path) -> Result<()> {
    let mut archives = tar_files(dsc);
    let orig = archives
        .iter()
        .position(|file| file.name.contains(".orig.tar"));
    let archive = match orig {
        Some(index) => archives.remove(index),
        None if archives.len() == 1 => archives.remove(0),
        None => bail!("1.0 source does not identify one native or orig tar archive"),
    };
    if !archives.is_empty() {
        bail!("1.0 source contains unexpected additional tar archives");
    }
    extract_tar_overlay(&archive.path, working, destination, true, 0)?;
    if let Some(diff) = dsc
        .files
        .iter()
        .find(|file| file.name.ends_with(".diff.gz"))
    {
        let bytes = decompress(&diff.path)?;
        apply_patch_bytes(destination, &bytes, 1, false)?;
    }
    Ok(())
}

fn extract_quilt(dsc: &Dsc, working: &Path, destination: &Path) -> Result<()> {
    let mut main_orig = None;
    let mut components = Vec::new();
    let mut debian = None;
    for file in tar_files(dsc) {
        if file.name.contains(".orig.tar") {
            main_orig = Some(file);
        } else if let Some(component) = orig_component_name(&file.name) {
            components.push((component.to_string(), file));
        } else if file.name.contains(".debian.tar") {
            debian = Some(file);
        } else {
            bail!("unexpected archive in 3.0 (quilt) source: {}", file.name);
        }
    }
    let main_orig = main_orig.context("3.0 (quilt) source is missing orig tar archive")?;
    let debian = debian.context("3.0 (quilt) source is missing debian tar archive")?;
    extract_tar_overlay(&main_orig.path, working, destination, true, 0)?;
    crate::source::clear_path(&destination.join(".pc"))?;
    components.sort_by(|left, right| left.0.cmp(&right.0));
    for (index, (name, component)) in components.into_iter().enumerate() {
        let component_destination = destination.join(name);
        crate::source::clear_path(&component_destination)?;
        fs::create_dir_all(&component_destination)?;
        extract_tar_overlay(
            &component.path,
            working,
            &component_destination,
            true,
            index + 1,
        )?;
    }
    crate::source::clear_path(&destination.join("debian"))?;
    extract_tar_overlay(&debian.path, working, destination, false, usize::MAX)?;
    apply_quilt_series(destination)
}

fn orig_component_name(name: &str) -> Option<&str> {
    let (_, suffix) = name.split_once(".orig-")?;
    let (component, archive) = suffix.split_once(".tar")?;
    (!component.is_empty()
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && matches!(archive, "" | ".gz" | ".xz" | ".lzma" | ".bz2" | ".zst"))
    .then_some(component)
}

fn tar_files(dsc: &Dsc) -> Vec<&DscFile> {
    dsc.files
        .iter()
        .filter(|file| {
            [
                ".tar",
                ".tar.gz",
                ".tar.xz",
                ".tar.lzma",
                ".tar.bz2",
                ".tar.zst",
            ]
            .iter()
            .any(|suffix| file.name.ends_with(suffix))
        })
        .collect()
}

fn extract_tar_overlay(
    archive: &Path,
    working: &Path,
    destination: &Path,
    strip_root: bool,
    index: usize,
) -> Result<()> {
    let scratch = working.join(format!("archive-{index}"));
    crate::source::clear_path(&scratch)?;
    fs::create_dir_all(&scratch)?;
    let bytes = decompress(archive)?;
    linyaps_repository::extract_tar(&bytes, &scratch)?;
    let mut entries = fs::read_dir(&scratch)?.collect::<std::io::Result<Vec<_>>>()?;
    let source = if strip_root && entries.len() == 1 && entries[0].file_type()?.is_dir() {
        entries.remove(0).path()
    } else {
        scratch.clone()
    };
    overlay_contents(&source, destination)?;
    crate::source::clear_path(&scratch)?;
    Ok(())
}

fn decompress(path: &Path) -> Result<Vec<u8>> {
    let input = fs::read(path)?;
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    let mut output = Vec::new();
    if name.ends_with(".gz") || input.starts_with(&[0x1f, 0x8b]) {
        GzDecoder::new(Cursor::new(input)).read_to_end(&mut output)?;
    } else if name.ends_with(".xz") || input.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        lzma_rs::xz_decompress(&mut BufReader::new(Cursor::new(input)), &mut output)
            .map_err(|error| anyhow::anyhow!("failed to decompress xz source: {error}"))?;
    } else if name.ends_with(".lzma") {
        lzma_rs::lzma_decompress(&mut BufReader::new(Cursor::new(input)), &mut output)
            .map_err(|error| anyhow::anyhow!("failed to decompress lzma source: {error}"))?;
    } else if name.ends_with(".bz2") || input.starts_with(b"BZh") {
        BzDecoder::new(Cursor::new(input)).read_to_end(&mut output)?;
    } else if name.ends_with(".zst") || input.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        ruzstd::decoding::StreamingDecoder::new(Cursor::new(input))?.read_to_end(&mut output)?;
    } else {
        output = input;
    }
    Ok(output)
}

fn overlay_contents(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        overlay_entry(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn overlay_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        crate::source::clear_path(destination)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        symlink(fs::read_link(source)?, destination)?;
    } else if metadata.is_dir() {
        if fs::symlink_metadata(destination)
            .is_ok_and(|current| !current.is_dir() || current.file_type().is_symlink())
        {
            crate::source::clear_path(destination)?;
        }
        fs::create_dir_all(destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        overlay_contents(source, destination)?;
    } else if metadata.is_file() {
        crate::source::clear_path(destination)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
    }
    Ok(())
}

fn apply_quilt_series(destination: &Path) -> Result<()> {
    apply_quilt_series_for_vendor(destination, debian_vendor().as_deref())
}

fn apply_quilt_series_for_vendor(destination: &Path, vendor: Option<&str>) -> Result<()> {
    let patches = destination.join("debian/patches");
    let series_path = select_series_path(&patches, vendor);
    let series_name = series_path
        .file_name()
        .and_then(OsStr::to_str)
        .context("invalid Debian quilt series file name")?;
    if series_path.exists() && !series_path.is_file() {
        bail!("Debian quilt series is not a regular file");
    }
    if series_path.is_file() && series_name != "series" {
        let default_series = patches.join("series");
        if fs::symlink_metadata(&default_series)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            fs::remove_file(&default_series)?;
        }
        if !default_series.exists() {
            symlink(series_name, default_series)?;
        }
    }
    let pc = destination.join(".pc");
    fs::create_dir_all(&pc)?;
    fs::write(pc.join(".version"), b"2\n")?;
    fs::write(pc.join(".quilt_patches"), b"debian/patches\n")?;
    fs::write(pc.join(".quilt_series"), format!("{series_name}\n"))?;
    if !series_path.is_file() {
        fs::write(pc.join("applied-patches"), b"")?;
        return Ok(());
    }
    let series = Series::read(File::open(&series_path)?)?;
    let mut applied = String::new();
    for entry in series.entries {
        let SeriesEntry::Patch { name, options } = entry else {
            continue;
        };
        let patch_name = safe_relative(&name)?;
        let patch_path = patches.join(&patch_name);
        let bytes = fs::read(&patch_path)
            .with_context(|| format!("failed to read quilt patch {}", patch_path.display()))?;
        let (strip, reverse) = quilt_options(&options)?;
        let parsed = parse_patch_set(&bytes)?;
        validate_patch_paths(&parsed, strip)?;
        backup_patch_inputs(destination, &pc.join(&patch_name), &parsed, strip)?;
        apply_parsed_patches(destination, &parsed, strip, reverse)?;
        applied.push_str(&name);
        applied.push('\n');
    }
    fs::write(pc.join("applied-patches"), applied)?;
    Ok(())
}

fn quilt_options(options: &[String]) -> Result<(u32, bool)> {
    let _ = options;
    Ok((1, false))
}

fn apply_patch_bytes(destination: &Path, bytes: &[u8], strip: u32, reverse: bool) -> Result<()> {
    let patches = parse_patch_set(bytes)?;
    validate_patch_paths(&patches, strip)?;
    apply_parsed_patches(destination, &patches, strip, reverse)
}

fn parse_patch_set(bytes: &[u8]) -> Result<Vec<UnifiedPatch>> {
    let parsed = UnifiedPatch::parse_patches(
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| !is_binary_difference_marker(line))
            .map(<[u8]>::to_vec),
    )?;
    parsed
        .into_iter()
        .filter_map(|patch| match patch {
            PlainOrBinaryPatch::Plain(patch) => Some(Ok(patch)),
            PlainOrBinaryPatch::Binary(_) => None,
        })
        .collect()
}

fn is_binary_difference_marker(line: &[u8]) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.starts_with(b"Binary files ") && line.ends_with(b" differ")
}

fn select_series_path(patches: &Path, vendor: Option<&str>) -> PathBuf {
    let vendor = vendor.unwrap_or("debian").to_ascii_lowercase();
    let vendor_series = patches.join(format!("{vendor}.series"));
    if vendor_series.exists() {
        vendor_series
    } else {
        patches.join("series")
    }
}

fn debian_vendor() -> Option<String> {
    let origins = std::env::var_os("DPKG_ORIGINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/dpkg/origins"));
    let requested = std::env::var_os("DEB_VENDOR");
    let origin = requested
        .as_deref()
        .and_then(|vendor| find_origin(&origins, vendor))
        .unwrap_or_else(|| origins.join("default"));
    let content = fs::read_to_string(origin).ok()?;
    content.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("vendor")
            .then(|| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn find_origin(origins: &Path, vendor: &OsStr) -> Option<PathBuf> {
    let vendor = vendor.to_str()?;
    let mut candidates = vec![vendor.to_string(), vendor.to_ascii_lowercase()];
    let mut capitalized = vendor.to_ascii_lowercase();
    if let Some(first) = capitalized.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    candidates.push(capitalized);
    candidates
        .into_iter()
        .map(|candidate| origins.join(candidate))
        .find(|candidate| candidate.exists())
}

fn apply_parsed_patches(
    destination: &Path,
    patches: &[UnifiedPatch],
    strip: u32,
    reverse: bool,
) -> Result<()> {
    let options = ApplyToTreeOptions {
        apply: ApplyOptions::default(),
        strip,
        reverse,
        dry_run: false,
        backup_suffix: None,
        remove_empty_files: true,
    };
    let report = apply_to_tree(destination, patches, &options, None)?;
    if !report.applied() {
        bail!("Debian source patch did not apply cleanly");
    }
    Ok(())
}

fn validate_patch_paths(patches: &[UnifiedPatch], strip: u32) -> Result<()> {
    for patch in patches {
        for name in [&patch.orig_name, &patch.mod_name] {
            if patch_path(name, strip)?.is_some() {
                continue;
            }
        }
    }
    Ok(())
}

fn backup_patch_inputs(
    destination: &Path,
    backup: &Path,
    patches: &[UnifiedPatch],
    strip: u32,
) -> Result<()> {
    for patch in patches {
        let Some(path) = patch_path(&patch.orig_name, strip)? else {
            continue;
        };
        let source = destination.join(&path);
        if source.is_file() {
            let target = backup.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, target)?;
        }
    }
    Ok(())
}

fn patch_path(name: &[u8], strip: u32) -> Result<Option<PathBuf>> {
    let name = std::str::from_utf8(name).context("patch path is not UTF-8")?;
    let name = name.split('\t').next().unwrap_or(name);
    if name == "/dev/null" {
        return Ok(None);
    }
    let parts = name.split('/').collect::<Vec<_>>();
    if parts.len() <= strip as usize {
        bail!("patch path {name} has fewer than {strip} components");
    }
    let path = PathBuf::from(parts[strip as usize..].join("/"));
    validate_relative_path(&path)?;
    Ok(Some(path))
}

fn safe_relative(path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    validate_relative_path(&path)?;
    Ok(path)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsafe Debian source path: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_entry(name: &str, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000755\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[148..156].fill(b' ');
        let checksum = header.iter().map(|byte| usize::from(*byte)).sum::<usize>();
        header[148..156].copy_from_slice(format!("{:06o}\0 ", checksum).as_bytes());
        let mut output = header.to_vec();
        output.extend_from_slice(data);
        output.resize(output.len().next_multiple_of(512), 0);
        output
    }

    fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (name, data) in entries {
            archive.extend(tar_entry(name, b'0', data));
        }
        archive.extend([0_u8; 1024]);
        archive
    }

    fn source_file(name: &str, path: PathBuf) -> DscFile {
        DscFile {
            algorithm: ChecksumAlgorithm::Sha256,
            name: name.to_string(),
            digest: String::new(),
            size: 0,
            path,
        }
    }

    #[test]
    fn parses_clear_signed_descriptors() {
        let fields = parse_control(
            b"-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nFormat: 3.0 (native)\nChecksums-Sha256:\n abc 1 file\n-----BEGIN PGP SIGNATURE-----\nsignature\n",
        )
        .unwrap();
        assert_eq!(fields["format"], "3.0 (native)");
        assert_eq!(fields["checksums-sha256"], "\nabc 1 file");
    }

    #[test]
    fn rejects_unsafe_patch_paths() {
        assert!(patch_path(b"a/../../escape", 1).is_err());
        assert!(patch_path(b"/absolute", 0).is_err());
        assert_eq!(
            patch_path(b"a/src/file", 1).unwrap(),
            Some("src/file".into())
        );
    }

    #[test]
    fn resolves_native_and_quilt_patch_content() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("tree");
        fs::create_dir_all(destination.join("debian/patches")).unwrap();
        fs::write(destination.join("file"), b"old\n").unwrap();
        fs::write(
            destination.join("debian/patches/change.patch"),
            b"--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        fs::write(destination.join("debian/patches/series"), b"change.patch\n").unwrap();
        apply_quilt_series(&destination).unwrap();
        assert_eq!(fs::read(destination.join("file")).unwrap(), b"new\n");
        assert_eq!(
            fs::read(destination.join(".pc/change.patch/file")).unwrap(),
            b"old\n"
        );
    }

    #[test]
    fn extracts_quilt_components_and_replaces_debian_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let main = temporary.path().join("example_1.0.orig.tar");
        let component = temporary.path().join("example_1.0.orig-docs.tar");
        let debian = temporary.path().join("example_1.0-1.debian.tar");
        fs::write(
            &main,
            tar(&[
                ("example-1.0/top.txt", b"top\n"),
                ("example-1.0/docs/old.txt", b"old\n"),
                ("example-1.0/debian/stale", b"stale\n"),
                ("example-1.0/.pc/untrusted", b"untrusted\n"),
            ]),
        )
        .unwrap();
        fs::write(
            &component,
            tar(&[("component-root/new.txt", b"component\n")]),
        )
        .unwrap();
        fs::write(&debian, tar(&[("debian/control", b"Source: example\n")])).unwrap();
        let dsc = Dsc {
            format: "3.0 (quilt)".to_string(),
            files: vec![
                source_file("example_1.0.orig.tar", main),
                source_file("example_1.0.orig-docs.tar", component),
                source_file("example_1.0-1.debian.tar", debian),
            ],
        };
        let working = temporary.path().join("working");
        let destination = temporary.path().join("output");
        fs::create_dir_all(&working).unwrap();
        fs::create_dir_all(&destination).unwrap();

        extract_quilt(&dsc, &working, &destination).unwrap();

        assert_eq!(fs::read(destination.join("top.txt")).unwrap(), b"top\n");
        assert_eq!(
            fs::read(destination.join("docs/new.txt")).unwrap(),
            b"component\n"
        );
        assert!(!destination.join("docs/old.txt").exists());
        assert!(!destination.join("debian/stale").exists());
        assert_eq!(
            fs::read(destination.join("debian/control")).unwrap(),
            b"Source: example\n"
        );
        assert!(!destination.join(".pc/untrusted").exists());
        assert_eq!(
            fs::read(destination.join(".pc/applied-patches")).unwrap(),
            b""
        );
    }

    #[test]
    fn applies_vendor_series_with_dpkg_option_semantics() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("tree");
        let patches = destination.join("debian/patches");
        fs::create_dir_all(&patches).unwrap();
        fs::write(destination.join("file"), b"old\n").unwrap();
        fs::write(
            patches.join("change.patch"),
            b"--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        fs::write(
            patches.join("ubuntu.series"),
            b"change.patch -p0 -R --unsupported\n",
        )
        .unwrap();

        apply_quilt_series_for_vendor(&destination, Some("Ubuntu")).unwrap();

        assert_eq!(fs::read(destination.join("file")).unwrap(), b"new\n");
        assert_eq!(
            fs::read_link(patches.join("series")).unwrap(),
            PathBuf::from("ubuntu.series")
        );
        assert_eq!(
            fs::read(destination.join(".pc/.quilt_series")).unwrap(),
            b"ubuntu.series\n"
        );
    }

    #[test]
    fn ignores_binary_difference_markers_like_dpkg_patch() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path();
        fs::write(destination.join("blob"), [0_u8, 1, 2, 3]).unwrap();
        fs::write(destination.join("file"), b"old\n").unwrap();

        apply_patch_bytes(
            destination,
            b"Binary files a/blob and b/blob differ\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n",
            1,
            false,
        )
        .unwrap();

        assert_eq!(fs::read(destination.join("blob")).unwrap(), [0, 1, 2, 3]);
        assert_eq!(fs::read(destination.join("file")).unwrap(), b"new\n");
    }

    #[tokio::test]
    async fn extracts_local_native_source_package() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_name = "example_1.0.tar";
        let archive = tar(&[("example-1.0/file.txt", b"source\n")]);
        let digest = format!("{:x}", Sha256::digest(&archive));
        fs::write(temporary.path().join(archive_name), &archive).unwrap();
        let descriptor = temporary.path().join("example_1.0.dsc");
        fs::write(
            &descriptor,
            format!(
                "Format: 3.0 (native)\nSource: example\nVersion: 1.0\nChecksums-Sha256:\n {digest} {} {archive_name}\n",
                archive.len()
            ),
        )
        .unwrap();
        let working = temporary.path().join("working");
        let destination = temporary.path().join("output");
        fs::create_dir_all(&working).unwrap();
        extract(
            &descriptor,
            descriptor.to_str().unwrap(),
            &working,
            &destination,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(destination.join("file.txt")).unwrap(), b"source\n");
    }

    #[tokio::test]
    async fn extracts_legacy_md5_only_source_package() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_name = "example_1.0.orig.tar";
        let archive = tar(&[("example-1.0/legacy.txt", b"legacy\n")]);
        let digest = format!("{:x}", Md5::digest(&archive));
        fs::write(temporary.path().join(archive_name), &archive).unwrap();
        let descriptor = temporary.path().join("example_1.0.dsc");
        fs::write(
            &descriptor,
            format!(
                "Format: 1.0\nSource: example\nVersion: 1.0\nFiles:\n {digest} {} {archive_name}\n",
                archive.len()
            ),
        )
        .unwrap();
        let working = temporary.path().join("working");
        let destination = temporary.path().join("output");
        fs::create_dir_all(&working).unwrap();

        extract(
            &descriptor,
            descriptor.to_str().unwrap(),
            &working,
            &destination,
        )
        .await
        .unwrap();

        assert_eq!(
            fs::read(destination.join("legacy.txt")).unwrap(),
            b"legacy\n"
        );
    }

    #[test]
    fn dpkg_source_fallback_passes_descriptor_and_destination() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("dpkg-source");
        fs::write(
            &executable,
            "#!/bin/sh\n[ \"$1\" = -x ]\n[ \"$2\" = --no-copy ]\n[ -f \"$3\" ]\nmkdir -p \"$4\"\nprintf extracted > \"$4/result\"\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let descriptor = temporary.path().join("source.dsc");
        fs::write(&descriptor, "descriptor").unwrap();
        let destination = temporary.path().join("output");

        extract_with_dpkg_source(executable.as_os_str(), &descriptor, &destination).unwrap();

        assert_eq!(fs::read(destination.join("result")).unwrap(), b"extracted");
    }
}
