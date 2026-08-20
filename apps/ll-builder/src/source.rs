use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use linyaps_api::{BuilderConfig, BuilderProjectSource};
use sha2::{Digest, Sha256};

pub async fn fetch_sources(
    sources: &[BuilderProjectSource],
    internal_directory: &Path,
    config: &BuilderConfig,
) -> Result<()> {
    let destination = internal_directory.join("sources");
    clear_path(&destination)?;
    fs::create_dir_all(&destination)?;
    let cache = std::env::var_os("LINGLONG_FETCH_CACHE")
        .map(PathBuf::from)
        .or_else(|| config.cache.as_deref().map(PathBuf::from))
        .unwrap_or_else(|| internal_directory.join("cache"));
    fs::create_dir_all(&cache)?;
    for source in sources {
        fetch_source(source, &cache, &destination).await?;
    }
    Ok(())
}

async fn fetch_source(
    source: &BuilderProjectSource,
    cache: &Path,
    destination: &Path,
) -> Result<()> {
    let url = source.url.as_deref().context("source missing url")?;
    let name = source_name(source, url)?;
    let output = destination.join(name);
    match source.kind.as_str() {
        "git" => fetch_git(source, url, &output),
        "file" => {
            let digest = source.digest.as_deref().context("digest missing")?;
            fetch_file_source(&output, url, digest, cache).await
        }
        "archive" => {
            let digest = source.digest.as_deref().context("digest missing")?;
            fetch_archive_source(&output, url, digest, cache).await
        }
        "dsc" => fetch_dsc(source, url, cache, &output).await,
        _ => bail!("unknown source kind"),
    }
}

fn fetch_git(source: &BuilderProjectSource, url: &str, output: &Path) -> Result<()> {
    let commit = source.commit.as_deref().context("digest missing")?;
    fetch_git_source(output, url, commit, source.submodules.unwrap_or(true))
}

pub fn fetch_git_source(
    output: &Path,
    url: &str,
    commit: &str,
    recurse_submodules: bool,
) -> Result<()> {
    linyaps_core::tls::install_default_provider();
    if !output.join(".git").is_dir() {
        let rust_result = gix::url::parse(url.into())
            .map_err(anyhow::Error::from)
            .and_then(|url| clone_git_commit(&url, commit, output, recurse_submodules));
        if rust_result.is_ok() {
            return Ok(());
        }
    }
    fetch_git_with_command(output, url, commit, recurse_submodules)
}

fn fetch_git_with_command(
    output: &Path,
    url: &str,
    commit: &str,
    recurse_submodules: bool,
) -> Result<()> {
    fs::create_dir_all(output)?;
    let git = std::env::var_os("LINGLONG_GIT").unwrap_or_else(|| "git".into());
    if output.join(".git").is_dir() {
        run_command(
            Command::new(&git)
                .arg("-C")
                .arg(output)
                .args(["remote", "set-url", "origin", url]),
            "git remote set-url",
        )?;
    } else {
        run_command(
            Command::new(&git).arg("-C").arg(output).arg("init"),
            "git init",
        )?;
        run_command(
            Command::new(&git)
                .arg("-C")
                .arg(output)
                .args(["remote", "add", "origin", url]),
            "git remote add",
        )?;
    }
    run_command(
        Command::new(&git)
            .arg("-C")
            .arg(output)
            .args(["fetch", "origin", commit, "--depth", "1", "-n"]),
        "git fetch",
    )?;
    run_command(
        Command::new(&git).arg("-C").arg(output).args(["add", ":/"]),
        "git add",
    )?;
    run_command(
        Command::new(&git)
            .arg("-C")
            .arg(output)
            .args(["reset", "--hard", "FETCH_HEAD"]),
        "git reset",
    )?;
    if recurse_submodules {
        run_command(
            Command::new(&git).arg("-C").arg(output).args([
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--depth",
                "1",
            ]),
            "git submodule update",
        )?;
        run_command(
            Command::new(&git).arg("-C").arg(output).args([
                "submodule",
                "foreach",
                "git reset --hard HEAD",
            ]),
            "git submodule reset",
        )?;
    }
    Ok(())
}

fn run_command(command: &mut Command, description: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to execute {description}"))?;
    if !status.success() {
        bail!("{description} failed with {status}");
    }
    Ok(())
}

fn clone_git_commit(
    url: &gix::Url,
    commit: &str,
    output: &Path,
    recurse_submodules: bool,
) -> Result<()> {
    clear_path(output)?;
    let refspec =
        gix::bstr::BString::from(format!("+{commit}:refs/remotes/origin/linglong-builder"));
    let mut preparation = gix::prepare_clone(url.clone(), output)
        .with_context(|| format!("failed to prepare Git clone from {url}"))?
        .configure_remote(move |remote| {
            remote
                .with_refspecs([refspec.clone()], gix::remote::Direction::Fetch)
                .map_err(Into::into)
        });
    let interrupted = AtomicBool::new(false);
    let (mut checkout, _) = preparation
        .fetch_then_checkout(gix::progress::Discard, &interrupted)
        .with_context(|| format!("failed to fetch Git source {url}"))?;
    let expected = checkout
        .repo()
        .rev_parse_single(commit)
        .with_context(|| format!("Git commit {commit} was not fetched from {url}"))?
        .object()?
        .peel_to_commit()?
        .id;
    let actual = expected.to_string();
    if actual != commit && !actual.starts_with(commit) {
        bail!("Git commit is {actual}, expected {commit}");
    }
    checkout.repo().reference(
        "HEAD",
        expected,
        gix::refs::transaction::PreviousValue::Any,
        "checkout: moving to requested source commit",
    )?;
    let (repository, _) = checkout
        .main_worktree(gix::progress::Discard, &interrupted)
        .with_context(|| format!("failed to check out Git commit {commit}"))?;
    if recurse_submodules {
        checkout_submodules(&repository)?;
    }
    Ok(())
}

fn checkout_submodules(repository: &gix::Repository) -> Result<()> {
    let Some(submodules) = repository.submodules()? else {
        return Ok(());
    };
    let parent_url = repository
        .find_remote("origin")?
        .url(gix::remote::Direction::Fetch)
        .context("Git origin has no fetch URL")?
        .clone();
    for submodule in submodules {
        let path = submodule.work_dir()?;
        let commit = submodule
            .head_id()?
            .with_context(|| format!("submodule {} has no commit", submodule.name()))?
            .to_string();
        let configured_url = submodule.url()?;
        let url = resolve_submodule_url(&parent_url, &configured_url)?;
        clone_git_commit(&url, &commit, &path, true)
            .with_context(|| format!("failed to check out submodule {}", submodule.name()))?;
    }
    Ok(())
}

fn resolve_submodule_url(parent: &gix::Url, child: &gix::Url) -> Result<gix::Url> {
    let child_path: &[u8] = child.path.as_ref();
    if child.scheme != gix::url::Scheme::File
        || (!child_path.starts_with(b"./") && !child_path.starts_with(b"../"))
    {
        return Ok(child.clone());
    }
    let mut resolved = parent.clone();
    let rooted = resolved.path.starts_with(b"/");
    let mut components: Vec<&[u8]> = Vec::new();
    for component in resolved
        .path
        .split(|byte| *byte == b'/')
        .chain(child_path.split(|byte| *byte == b'/'))
    {
        match component {
            b"" | b"." => {}
            b".." if components.last().is_some_and(|last| *last != b"..") => {
                components.pop();
            }
            b".." if rooted => {}
            _ => components.push(component),
        }
    }
    let mut path = Vec::new();
    if rooted {
        path.push(b'/');
    }
    for (index, component) in components.into_iter().enumerate() {
        if index != 0 {
            path.push(b'/');
        }
        path.extend_from_slice(component);
    }
    resolved.path = path.into();
    Ok(resolved)
}

async fn fetch_dsc(
    source: &BuilderProjectSource,
    url: &str,
    cache: &Path,
    output: &Path,
) -> Result<()> {
    let digest = source.digest.as_deref().context("digest missing")?;
    let file_name = source_name(source, url)?;
    fetch_dsc_source(output, url, digest, cache, &file_name).await
}

pub async fn fetch_file_source(output: &Path, url: &str, digest: &str, cache: &Path) -> Result<()> {
    fs::create_dir_all(cache)?;
    let cached = cache.join(format!("file_{digest}"));
    obtain_file(url, digest, &cached).await?;
    replace_with_link_or_copy(&cached, output)
}

pub async fn fetch_archive_source(
    output: &Path,
    url: &str,
    digest: &str,
    cache: &Path,
) -> Result<()> {
    fs::create_dir_all(cache)?;
    let cached_archive = cache.join(format!("download_{digest}"));
    let cached_tree = cache.join(format!("archive_{digest}"));
    obtain_file(url, digest, &cached_archive).await?;
    if !cached_tree.is_dir() {
        let temporary = cache.join(format!("tmp_{digest}-{}", std::process::id()));
        clear_path(&temporary)?;
        fs::create_dir_all(&temporary)?;
        if extract_archive(&cached_archive, &temporary).is_err() {
            clear_path(&temporary)?;
            fs::create_dir_all(&temporary)?;
            extract_archive_with_tar(&cached_archive, &temporary)?;
        }
        match fs::rename(&temporary, &cached_tree) {
            Ok(()) => {}
            Err(_) if cached_tree.is_dir() => {
                let _ = clear_path(&temporary);
            }
            Err(error) => return Err(error.into()),
        }
    }
    clear_path(output)?;
    copy_tree(&cached_tree, output)
}

fn extract_archive_with_tar(path: &Path, destination: &Path) -> Result<()> {
    let tar = std::env::var_os("LINGLONG_TAR").unwrap_or_else(|| "tar".into());
    run_command(
        Command::new(tar)
            .args(["--no-same-owner", "-xvf"])
            .arg(path)
            .arg("-C")
            .arg(destination),
        "tar extraction",
    )
}

pub async fn fetch_dsc_source(
    output: &Path,
    url: &str,
    digest: &str,
    cache: &Path,
    file_name: &str,
) -> Result<()> {
    fs::create_dir_all(cache)?;
    let cached = cache.join(format!("dsc_{digest}"));
    if cached.is_dir() {
        clear_path(output)?;
        return copy_tree(&cached, output);
    }
    let working = cache.join(format!("dsc-tmp-{digest}-{}", std::process::id()));
    clear_path(&working)?;
    fs::create_dir_all(&working)?;
    let descriptor = working.join(file_name);
    obtain_file(url, digest, &descriptor).await?;
    let extracted = working.join("source");
    let result = crate::debian_source::extract(&descriptor, url, &working, &extracted).await;
    if let Err(error) = result {
        let _ = clear_path(&working);
        return Err(error);
    }
    fs::rename(&extracted, &cached)?;
    let _ = clear_path(&working);
    copy_tree(&cached, output)
}

async fn obtain_file(url: &str, digest: &str, destination: &Path) -> Result<()> {
    linyaps_core::tls::install_default_provider();
    if destination.is_file() && sha256_file(destination)? == digest {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = destination.with_extension(format!("part-{}", std::process::id()));
    if let Some(path) = local_source_path(url) {
        fs::copy(path, &temporary).with_context(|| format!("failed to read source {url}"))?;
    } else {
        let response = reqwest::get(url).await?.error_for_status()?;
        fs::write(&temporary, response.bytes().await?)?;
    }
    let actual = sha256_file(&temporary)?;
    if actual != digest {
        let _ = fs::remove_file(&temporary);
        bail!("File SHA256 digest is {actual}, expected {digest}");
    }
    fs::rename(temporary, destination)?;
    Ok(())
}

fn extract_archive(path: &Path, destination: &Path) -> Result<()> {
    let bytes = fs::read(path)?;
    let mut archive = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        GzDecoder::new(Cursor::new(bytes)).read_to_end(&mut archive)?;
    } else if bytes.starts_with(b"BZh") {
        BzDecoder::new(Cursor::new(bytes)).read_to_end(&mut archive)?;
    } else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        lzma_rs::xz_decompress(&mut BufReader::new(Cursor::new(bytes)), &mut archive)
            .map_err(|error| anyhow::anyhow!("failed to decompress xz archive: {error}"))?;
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        ruzstd::decoding::StreamingDecoder::new(Cursor::new(bytes))?.read_to_end(&mut archive)?;
    } else if bytes.first() == Some(&0x5d) {
        lzma_rs::lzma_decompress(&mut BufReader::new(Cursor::new(bytes)), &mut archive)
            .map_err(|error| anyhow::anyhow!("failed to decompress lzma archive: {error}"))?;
    } else {
        archive = bytes;
    }
    extract_tar(&archive, destination)
}

pub(crate) fn extract_tar(archive: &[u8], destination: &Path) -> Result<()> {
    linyaps_repository::extract_tar(archive, destination)?;
    Ok(())
}

fn source_name(source: &BuilderProjectSource, url: &str) -> Result<String> {
    if let Some(name) = source.name.as_ref().filter(|name| !name.is_empty()) {
        return Ok(name.clone());
    }
    let without_query = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/');
    Path::new(without_query)
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .context("missing name and url field")
}

fn local_source_path(url: &str) -> Option<&Path> {
    if let Some(path) = url.strip_prefix("file://") {
        return Some(Path::new(path));
    }
    (!url.contains("://")).then(|| Path::new(url))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(linyaps_core::hex_encode(hasher.finalize()))
}

fn replace_with_link_or_copy(source: &Path, destination: &Path) -> Result<()> {
    clear_path(destination)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::hard_link(source, destination).is_err() {
        fs::copy(source, destination)?;
    }
    Ok(())
}

pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        symlink(fs::read_link(source)?, destination)?;
    } else if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
    }
    Ok(())
}

pub(crate) fn clear_path(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::Compression;
    use bzip2::write::BzEncoder;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn git_entry(
        name: &str,
        kind: gix::objs::tree::EntryKind,
        oid: gix::ObjectId,
    ) -> gix::objs::tree::Entry {
        gix::objs::tree::Entry {
            mode: kind.into(),
            filename: name.into(),
            oid,
        }
    }

    fn git_tree(
        repository: &gix::Repository,
        mut entries: Vec<gix::objs::tree::Entry>,
    ) -> gix::ObjectId {
        entries.sort();
        repository
            .write_object(&gix::objs::Tree { entries })
            .unwrap()
            .detach()
    }

    fn git_commit(
        repository: &gix::Repository,
        tree: gix::ObjectId,
        parents: &[gix::ObjectId],
        message: &str,
    ) -> gix::ObjectId {
        let signature = gix::actor::Signature {
            name: "Linyaps Test".into(),
            email: "test@linyaps.invalid".into(),
            time: gix::date::Time {
                seconds: 1_700_000_000,
                offset: 0,
            },
        };
        repository
            .write_object(&gix::objs::Commit {
                tree,
                parents: parents.iter().copied().collect(),
                author: signature.clone(),
                committer: signature,
                encoding: None,
                message: message.into(),
                extra_headers: Vec::new(),
            })
            .unwrap()
            .detach()
    }

    fn publish_git(repository: &gix::Repository, commit: gix::ObjectId) {
        repository
            .reference(
                "refs/heads/main",
                commit,
                gix::refs::transaction::PreviousValue::Any,
                "test",
            )
            .unwrap();
        repository
            .reference(
                "HEAD",
                commit,
                gix::refs::transaction::PreviousValue::Any,
                "test",
            )
            .unwrap();
    }

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

    #[test]
    fn extracts_tar_without_path_traversal() {
        let temporary = tempdir().unwrap();
        let mut archive = tar_entry("root/", b'5', &[]);
        archive.extend(tar_entry("root/file", b'0', b"payload"));
        archive.extend([0_u8; 1024]);
        extract_tar(&archive, temporary.path()).unwrap();
        assert_eq!(
            fs::read(temporary.path().join("root/file")).unwrap(),
            b"payload"
        );

        let mut unsafe_archive = tar_entry("../escape", b'0', b"payload");
        unsafe_archive.extend([0_u8; 1024]);
        assert!(extract_tar(&unsafe_archive, temporary.path()).is_err());
    }

    #[test]
    fn extracts_bzip2_and_zstd_tar_archives() {
        let temporary = tempdir().unwrap();
        let mut archive = tar_entry("root/", b'5', &[]);
        archive.extend(tar_entry("root/file", b'0', b"payload"));
        archive.extend([0_u8; 1024]);

        let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&archive).unwrap();
        let bzip2_path = temporary.path().join("source-bzip2");
        fs::write(&bzip2_path, encoder.finish().unwrap()).unwrap();
        let bzip2_output = temporary.path().join("bzip2");
        fs::create_dir(&bzip2_output).unwrap();
        extract_archive(&bzip2_path, &bzip2_output).unwrap();
        assert_eq!(
            fs::read(bzip2_output.join("root/file")).unwrap(),
            b"payload"
        );

        let zstd = hex::decode(concat!(
            "28b52ffd640027cd020062c30b11b0eb38499288c41b664adb5bd9820130053665",
            "a958d881573c770c725442feddca73d056e06f3e6fd5bdc35bab050f20602d2c",
            "1703f503cfc1aad65de1601f1ad002c929009fc1289d3f13be1f38e0a000db89",
            "c3019859ab01"
        ))
        .unwrap();
        let zstd_path = temporary.path().join("source-zstd");
        fs::write(&zstd_path, zstd).unwrap();
        let zstd_output = temporary.path().join("zstd");
        fs::create_dir(&zstd_output).unwrap();
        extract_archive(&zstd_path, &zstd_output).unwrap();
        assert_eq!(fs::read(zstd_output.join("root/file")).unwrap(), b"payload");
    }

    #[test]
    fn clones_requested_git_commit_with_modes_and_links() {
        let temporary = tempdir().unwrap();
        let remote_path = temporary.path().join("remote.git");
        let repository = gix::init_bare(&remote_path).unwrap();
        let old_blob = repository.write_blob(b"old\n").unwrap().detach();
        let executable = repository
            .write_blob(b"#!/bin/sh\necho test\n")
            .unwrap()
            .detach();
        let link = repository.write_blob(b"file.txt").unwrap().detach();
        let first_tree = git_tree(
            &repository,
            vec![
                git_entry("file.txt", gix::objs::tree::EntryKind::Blob, old_blob),
                git_entry(
                    "run.sh",
                    gix::objs::tree::EntryKind::BlobExecutable,
                    executable,
                ),
                git_entry("shortcut", gix::objs::tree::EntryKind::Link, link),
            ],
        );
        let first = git_commit(&repository, first_tree, &[], "first");
        let new_blob = repository.write_blob(b"new\n").unwrap().detach();
        let second_tree = git_tree(
            &repository,
            vec![git_entry(
                "file.txt",
                gix::objs::tree::EntryKind::Blob,
                new_blob,
            )],
        );
        let second = git_commit(&repository, second_tree, &[first], "second");
        publish_git(&repository, second);

        let output = temporary.path().join("checkout");
        let url = gix::url::parse(remote_path.to_string_lossy().as_bytes().into()).unwrap();
        clone_git_commit(&url, &first.to_string(), &output, false).unwrap();

        assert_eq!(fs::read(output.join("file.txt")).unwrap(), b"old\n");
        assert_ne!(
            fs::metadata(output.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(
            fs::read_link(output.join("shortcut")).unwrap(),
            Path::new("file.txt")
        );
        let checkout = gix::open(&output).unwrap();
        assert_eq!(checkout.head_id().unwrap().detach(), first);
    }

    #[test]
    fn reuses_existing_git_checkout_and_resets_changes() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temporary = tempdir().unwrap();
        let remote_path = temporary.path().join("remote.git");
        let repository = gix::init_bare(&remote_path).unwrap();
        let blob = repository.write_blob(b"remote\n").unwrap().detach();
        let tree = git_tree(
            &repository,
            vec![git_entry(
                "file.txt",
                gix::objs::tree::EntryKind::Blob,
                blob,
            )],
        );
        let commit = git_commit(&repository, tree, &[], "commit");
        publish_git(&repository, commit);
        let output = temporary.path().join("checkout");
        let url = remote_path.to_string_lossy();

        fetch_git_source(&output, &url, &commit.to_string(), false).unwrap();
        fs::write(output.join("file.txt"), b"local\n").unwrap();
        fs::write(output.join("untracked"), b"remove me\n").unwrap();
        fetch_git_source(&output, &url, &commit.to_string(), false).unwrap();

        assert_eq!(fs::read(output.join("file.txt")).unwrap(), b"remote\n");
        assert!(!output.join("untracked").exists());
    }

    #[test]
    fn clones_relative_git_submodules_recursively() {
        let temporary = tempdir().unwrap();
        let child_path = temporary.path().join("child.git");
        let child_repository = gix::init_bare(&child_path).unwrap();
        let child_blob = child_repository
            .write_blob(b"dependency\n")
            .unwrap()
            .detach();
        let child_tree = git_tree(
            &child_repository,
            vec![git_entry(
                "dependency.txt",
                gix::objs::tree::EntryKind::Blob,
                child_blob,
            )],
        );
        let child_commit = git_commit(&child_repository, child_tree, &[], "child");
        publish_git(&child_repository, child_commit);

        let parent_path = temporary.path().join("parent.git");
        let parent_repository = gix::init_bare(&parent_path).unwrap();
        let modules = parent_repository
            .write_blob(
                b"[submodule \"dependency\"]\n\tpath = deps/dependency\n\turl = ../child.git\n",
            )
            .unwrap()
            .detach();
        let deps_tree = git_tree(
            &parent_repository,
            vec![git_entry(
                "dependency",
                gix::objs::tree::EntryKind::Commit,
                child_commit,
            )],
        );
        let parent_tree = git_tree(
            &parent_repository,
            vec![
                git_entry(".gitmodules", gix::objs::tree::EntryKind::Blob, modules),
                git_entry("deps", gix::objs::tree::EntryKind::Tree, deps_tree),
            ],
        );
        let parent_commit = git_commit(&parent_repository, parent_tree, &[], "parent");
        publish_git(&parent_repository, parent_commit);

        let output = temporary.path().join("checkout");
        let url = gix::url::parse(parent_path.to_string_lossy().as_bytes().into()).unwrap();
        clone_git_commit(&url, &parent_commit.to_string(), &output, true).unwrap();

        assert_eq!(
            fs::read(output.join("deps/dependency/dependency.txt")).unwrap(),
            b"dependency\n"
        );
        let submodule = gix::open(output.join("deps/dependency")).unwrap();
        assert_eq!(submodule.head_id().unwrap().detach(), child_commit);
    }
}
