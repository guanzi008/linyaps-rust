use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::postprocess::{
    check_runtime_dependencies_for_paths, strip_debug_symbols,
    validate_exported_configuration_for_app,
};
use crate::source::{fetch_archive_source, fetch_dsc_source, fetch_file_source, fetch_git_source};

pub async fn dispatch() -> Option<(i32, bool, Result<()>)> {
    let invocation = env::args_os()
        .next()
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(OsStr::to_str)?
        .to_string();
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let (failure_code, report_error, result) = match invocation.as_str() {
        "fetch-file-source" => (1, true, fetch_file(&arguments).await),
        "fetch-archive-source" => (1, true, fetch_archive(&arguments).await),
        "fetch-dsc-source" => (1, true, fetch_dsc(&arguments).await),
        "fetch-git-source" => (1, true, fetch_git(&arguments)),
        "config-check.sh" => {
            let (failure_code, result) = config_check();
            (failure_code, false, result)
        }
        "ldd-check.sh" => (255, true, ldd_check(&arguments)),
        "main-check.sh" => (1, true, main_check(&arguments)),
        "symbols-strip.sh" => (1, true, symbols_strip()),
        _ => return None,
    };
    Some((failure_code, report_error, result))
}

async fn fetch_file(arguments: &[OsString]) -> Result<()> {
    let [output, url, digest, cache] =
        exact_arguments(arguments, "fetch-file-source OUTPUT URL SHA256 CACHE")?;
    fetch_file_source(
        Path::new(output),
        utf8(url, "URL")?,
        utf8(digest, "SHA256")?,
        Path::new(cache),
    )
    .await
}

async fn fetch_archive(arguments: &[OsString]) -> Result<()> {
    let [output, url, digest, cache] =
        exact_arguments(arguments, "fetch-archive-source OUTPUT URL SHA256 CACHE")?;
    fetch_archive_source(
        Path::new(output),
        utf8(url, "URL")?,
        utf8(digest, "SHA256")?,
        Path::new(cache),
    )
    .await
}

async fn fetch_dsc(arguments: &[OsString]) -> Result<()> {
    let [output, url, digest, cache] =
        exact_arguments(arguments, "fetch-dsc-source OUTPUT URL SHA256 CACHE")?;
    let url = utf8(url, "URL")?;
    let file_name = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .context("DSC URL has no file name")?;
    fetch_dsc_source(
        Path::new(output),
        url,
        utf8(digest, "SHA256")?,
        Path::new(cache),
        file_name,
    )
    .await
}

fn fetch_git(arguments: &[OsString]) -> Result<()> {
    let [output, url, commit, _cache] =
        exact_arguments(arguments, "fetch-git-source OUTPUT URL COMMIT CACHE")?;
    let recurse_submodules = env::var_os("GIT_SUBMODULES").is_some_and(|value| !value.is_empty());
    fetch_git_source(
        Path::new(output),
        utf8(url, "URL")?,
        utf8(commit, "COMMIT")?,
        recurse_submodules,
    )
}

fn config_check() -> (i32, Result<()>) {
    let app_id = match env::var("LINGLONG_APPID") {
        Ok(app_id) => app_id,
        Err(error) => return (255, Err(error).context("LINGLONG_APPID is not set")),
    };
    let files = helper_application_files(&app_id);
    if !files.parent().is_some_and(Path::is_dir) {
        println!(
            "/opt/apps/{} is not exist.",
            env::var("APPID").unwrap_or_default()
        );
        return (
            255,
            Err(anyhow::anyhow!("application directory does not exist")),
        );
    }
    let invalid = match validate_exported_configuration_for_app(&app_id, &files) {
        Ok(invalid) => invalid,
        Err(error) => return (1, Err(error)),
    };
    if invalid.is_empty() {
        return (1, Ok(()));
    }
    println!("These files have invalid file names:\n");
    for path in invalid {
        println!("{}", path.display());
    }
    println!("\nWe prefer to use $LINGLONG_APPID as the prefix. Such as {app_id}.xxx.");
    (
        1,
        Err(anyhow::anyhow!("application configuration check failed")),
    )
}

fn ldd_check(arguments: &[OsString]) -> Result<()> {
    if arguments.is_empty() {
        println!("usage:\n\tldd-check.sh path\n\tldd-check.sh [path:...]");
        return Ok(());
    }
    if arguments.len() != 1 {
        bail!("usage: ldd-check.sh [path:...]");
    }
    if let Some(cache) = env::var_os("LINGLONG_LD_SO_CACHE").filter(|value| !value.is_empty()) {
        let status = Command::new("ldconfig").arg("-C").arg(&cache).status()?;
        if !status.success() {
            bail!("ldconfig failed with {status}");
        }
        println!(
            "Debug:updated ld.so.cache to {}\n",
            Path::new(&cache).display()
        );
    }
    let paths = arguments[0]
        .to_string_lossy()
        .split(':')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        bail!("No paths provided");
    }
    let runtime = Path::new("/runtime")
        .is_dir()
        .then_some(Path::new("/runtime"));
    let output = env::var_os("LINGLONG_DEPENDS_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/project/linglong/depends.yaml"));
    println!("Debug:start collecting library dependencies...\n");
    check_runtime_dependencies_for_paths(&paths, Path::new("/"), runtime, &output)?;
    println!("Debug:finish collecting library dependencies...\n");
    Ok(())
}

fn main_check(arguments: &[OsString]) -> Result<()> {
    let mut level = 1_u8;
    let mut skip_ldd = false;
    let mut skip_config = false;
    for argument in arguments {
        match argument.to_str() {
            Some("--skip-ldd-check") => skip_ldd = true,
            Some("--skip-config-check") => skip_config = true,
            Some(value) => level = value.parse().context("Invalid level.")?,
            None => bail!("Invalid level."),
        }
    }
    match level {
        0 => println!("The check level is 0, some checks failed will be ignored."),
        1 => println!("The check level is 1, some checks failed will be treated as error."),
        _ => bail!("Invalid level."),
    }
    if skip_ldd {
        println!("Skipping ldd check.");
    } else {
        println!("start ldd check");
        let app_id = env::var("LINGLONG_APPID").context("LINGLONG_APPID is not set")?;
        let files = helper_application_files(&app_id);
        if let Err(error) = ldd_check(&[files.into_os_string()]) {
            eprintln!("Error: ldd check failed.\n{error:#}");
            if level == 1 {
                return Err(error);
            }
        }
    }
    if skip_config {
        println!("Skipping application configure check.");
    } else {
        println!("start application configure check");
        let (_, result) = config_check();
        if result.is_err() {
            println!("Warning: application configure check failed.");
        }
    }
    Ok(())
}

fn symbols_strip() -> Result<()> {
    let prefix = env::var_os("PREFIX")
        .map(PathBuf::from)
        .context("PREFIX is not set")?;
    let install_prefix = prefix.to_string_lossy().into_owned();
    strip_debug_symbols(&prefix, &install_prefix)
}

fn helper_application_files(app_id: &str) -> PathBuf {
    env::var_os("LINGLONG_HELPER_FILES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/opt/apps/{app_id}/files")))
}

fn exact_arguments<'a, const N: usize>(
    arguments: &'a [OsString],
    usage: &str,
) -> Result<&'a [OsString; N]> {
    arguments
        .try_into()
        .map_err(|_| anyhow::anyhow!("usage: {usage}"))
}

fn utf8<'a>(value: &'a OsStr, name: &str) -> Result<&'a str> {
    value
        .to_str()
        .with_context(|| format!("{name} is not valid UTF-8"))
}
