use std::env;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_lock::Mutex;
use linyaps_api::{Repo, RepoConfigV2};
use linyaps_core::FuzzyReference;
use linyaps_dbus::{PACKAGE_MANAGER_PATH, PACKAGE_MANAGER_SERVICE, PackageManagerService};
use linyaps_repository::{LocalRepository, RemoteRepositoryClient};

mod install_hooks;
mod operations;
mod polkit;
mod run_context;

const LINGLONG_ROOT: &str = "/var/lib/linglong";
const LINGLONG_USERNAME: &str = "deepin-linglong";
const DEFAULT_OCI_RUNTIME: &str = "ll-box";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Options {
    no_dbus: bool,
    peer_socket: Option<String>,
    init_run_context: Option<String>,
    container_id: Option<String>,
}

enum ParseOutcome {
    Run(Options),
    Error { code: i32, message: String },
}

fn parse_bool_flag(name: &str, value: &str) -> Result<bool, String> {
    if value.is_empty() {
        return Ok(true);
    }
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => Err(format!("Could not convert: --{name} = {value}")),
    }
}

fn print_help(program: &str) {
    println!(
        "linyaps package manager\nUsage: {program} [OPTIONS]\n\nOptions:\n  -h,--help                   Print this help message and exit\n  --init-run TEXT Needs: --id Excludes: \n                              json string of RunContextConfig\n  --id TEXT Needs: --init-run Excludes: \n                              container id\n"
    );
}

fn parse_options_from(arguments: impl IntoIterator<Item = OsString>) -> ParseOutcome {
    let mut arguments = arguments.into_iter();
    let program = arguments
        .next()
        .unwrap_or_else(|| OsString::from("ll-package-manager"))
        .to_string_lossy()
        .into_owned();
    let arguments = arguments.collect::<Vec<_>>();
    let mut options = Options::default();
    let mut no_dbus_present = false;
    let mut peer_socket_present = false;
    let mut init_run_present = false;
    let mut container_id_present = false;
    let mut unexpected = Vec::new();
    let mut conversion_error = None;
    let mut missing_value = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if argument == "-h" || argument == "--help" {
            print_help(&program);
            return ParseOutcome::Run(options);
        }
        if argument == "--" {
            if index + 1 < arguments.len() {
                unexpected.push(argument.into_owned());
                unexpected.extend(
                    arguments[index + 1..]
                        .iter()
                        .map(|value| value.to_string_lossy().into_owned()),
                );
            }
            break;
        }
        if let Some(long) = argument.strip_prefix("--") {
            let (name, inline_value) = long
                .split_once('=')
                .map_or((long, None), |(name, value)| (name, Some(value)));
            if name == "no-dbus" {
                no_dbus_present = true;
                match inline_value.map_or(Ok(true), |value| parse_bool_flag(name, value)) {
                    Ok(value) => options.no_dbus = value,
                    Err(error) if conversion_error.is_none() => conversion_error = Some(error),
                    Err(_) => {}
                }
                index += 1;
                continue;
            }
            let target = match name {
                "peer-socket" => {
                    peer_socket_present = true;
                    Some((&mut options.peer_socket, ""))
                }
                "init-run" => {
                    init_run_present = true;
                    Some((&mut options.init_run_context, "--init-run"))
                }
                "id" => {
                    container_id_present = true;
                    Some((&mut options.container_id, "--id"))
                }
                _ => None,
            };
            if let Some((target, display_name)) = target {
                if let Some(value) = inline_value {
                    *target = Some(value.to_string());
                } else if let Some(value) = arguments.get(index + 1).filter(|value| {
                    let value = value.to_string_lossy();
                    value == "-" || !value.starts_with('-')
                }) {
                    *target = Some(value.to_string_lossy().into_owned());
                    index += 1;
                } else if missing_value.is_none() {
                    missing_value = Some(format!("{display_name}: 1 required TEXT missing"));
                }
            } else {
                unexpected.push(argument.into_owned());
            }
            index += 1;
            continue;
        }
        if let Some(shorts) = argument.strip_prefix('-').filter(|value| !value.is_empty()) {
            let invalid = if let Some((position, short)) = shorts.char_indices().next() {
                if short == 'h' {
                    print_help(&program);
                    return ParseOutcome::Run(options);
                }
                Some(format!("-{}", &shorts[position..]))
            } else {
                None
            };
            if let Some(invalid) = invalid {
                unexpected.push(invalid);
            }
        } else {
            unexpected.push(argument.into_owned());
        }
        index += 1;
    }
    if let Some(message) = conversion_error {
        return ParseOutcome::Error { code: 104, message };
    }
    if let Some(message) = missing_value {
        return ParseOutcome::Error { code: 114, message };
    }
    if !unexpected.is_empty() {
        unexpected.reverse();
        let message = if unexpected.len() == 1 {
            format!("The following argument was not expected: {}", unexpected[0])
        } else {
            format!(
                "The following arguments were not expected: {}",
                unexpected.join(" ")
            )
        };
        return ParseOutcome::Error { code: 109, message };
    }
    if no_dbus_present != peer_socket_present {
        return ParseOutcome::Error {
            code: 107,
            message: " requires ".to_string(),
        };
    }
    if init_run_present && !container_id_present {
        return ParseOutcome::Error {
            code: 107,
            message: "--init-run requires --id".to_string(),
        };
    }
    if container_id_present && !init_run_present {
        return ParseOutcome::Error {
            code: 107,
            message: "--id requires --init-run".to_string(),
        };
    }
    if no_dbus_present && init_run_present {
        return ParseOutcome::Error {
            code: 108,
            message: " excludes --init-run".to_string(),
        };
    }
    ParseOutcome::Run(options)
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn find_executable(command: &OsStr) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 || path.is_absolute() {
        return is_executable(path).then(|| path.to_path_buf());
    }
    env::var_os("PATH")
        .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"))
        .to_string_lossy()
        .split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| Path::new(directory).join(command))
        .find(|candidate| is_executable(candidate))
}

fn startup_environment_is_valid() -> bool {
    let runtime = env::var_os("LINGLONG_OCI_RUNTIME")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_OCI_RUNTIME));
    if find_executable(&runtime).is_none() {
        return false;
    }
    env::var_os("USER").as_deref() == Some(OsStr::new(LINGLONG_USERNAME))
}

#[tokio::main]
async fn main() {
    let options = match parse_options_from(env::args_os()) {
        ParseOutcome::Run(options) => options,
        ParseOutcome::Error { code, message } => {
            eprintln!("{message}\nRun with --help for more information.");
            std::process::exit(code);
        }
    };
    if !startup_environment_is_valid() {
        std::process::exit(-1);
    }
    if let Err(error) = run(options).await {
        eprintln!("{error:#}");
        std::process::exit(-1);
    }
}

async fn run(options: Options) -> Result<()> {
    let root = env::var_os("LINGLONG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(LINGLONG_ROOT));
    if options.no_dbus {
        return serve_peer(
            &root,
            Path::new(
                options
                    .peer_socket
                    .as_deref()
                    .expect("clap requires peer socket"),
            ),
        )
        .await;
    }
    if options.init_run_context.is_some() {
        let repository = LocalRepository::open(&root)
            .await
            .context("failed to load repository")?;
        return run_context::initialize(
            &repository,
            options
                .init_run_context
                .as_deref()
                .expect("clap requires run context"),
            options
                .container_id
                .as_deref()
                .expect("clap requires container id"),
        );
    }
    serve_repository(&root).await
}

async fn serve_repository(root: &Path) -> Result<()> {
    let service = create_service(root, true).await?;
    let _connection = zbus::connection::Builder::system()?
        .name(PACKAGE_MANAGER_SERVICE)?
        .serve_at(PACKAGE_MANAGER_PATH, service)?
        .build()
        .await?;
    std::future::pending::<()>().await;
    Ok(())
}

async fn serve_peer(root: &Path, socket: &Path) -> Result<()> {
    let service = create_service(root, false).await?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create peer socket directory {}",
                parent.display()
            )
        })?;
    }
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to remove stale peer socket {}", socket.display())
            });
        }
    }
    let _cleanup = PeerSocketCleanup(socket.to_path_buf());
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("failed to listen on peer socket {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("failed to configure peer socket")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                return Ok(());
            }
            Err(error) => return Err(error).context("failed to accept peer connection"),
        }
    };
    drop(listener);
    stream
        .set_nonblocking(true)
        .context("failed to configure accepted peer socket")?;
    let stream =
        tokio::net::UnixStream::from_std(stream).context("failed to adopt accepted peer socket")?;
    let connection = zbus::connection::Builder::unix_stream(stream)
        .server(zbus::Guid::generate())?
        .p2p()
        .serve_at(PACKAGE_MANAGER_PATH, service)?
        .build()
        .await?;
    connection.closed().await;
    Ok(())
}

struct PeerSocketCleanup(PathBuf);

impl Drop for PeerSocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn create_service(root: &Path, use_polkit: bool) -> Result<PackageManagerService> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("failed to create repository root {}", root.display()))?;
    let repository = LocalRepository::create(root, default_config())
        .await
        .context("failed to create repository")?;
    let configuration = Arc::new(RwLock::new(repository.config().clone()));
    let repository = Arc::new(Mutex::new(repository));
    start_deferred_uninstall(repository.clone());
    let getter_configuration = configuration.clone();
    let setter_repository = repository.clone();
    let setter_configuration = configuration.clone();
    let search_configuration = configuration;
    let install_repository = repository.clone();
    let install_file_repository = repository.clone();
    let uninstall_repository = repository.clone();
    let update_repository = repository.clone();
    let init_run_repository = repository.clone();
    let prune_repository = repository;
    let service = PackageManagerService::new(
        move || {
            let configuration = getter_configuration.clone();
            async move {
                configuration
                    .read()
                    .map(|configuration| configuration.clone())
                    .map_err(|error| error.to_string())
            }
        },
        move |config| {
            let repository = setter_repository.clone();
            let configuration = setter_configuration.clone();
            async move {
                repository
                    .lock()
                    .await
                    .update_config(config.clone())
                    .map_err(|error| error.to_string())?;
                *configuration.write().map_err(|error| error.to_string())? = config;
                Ok(())
            }
        },
    )
    .with_search(move |parameters, context| {
        let configuration = search_configuration.clone();
        async move {
            let fuzzy = parameters
                .id
                .parse::<FuzzyReference>()
                .map_err(|error| error.to_string())?;
            let config = configuration
                .read()
                .map(|configuration| configuration.clone())
                .map_err(|error| error.to_string())?;
            let mut packages = std::collections::BTreeMap::new();
            for alias in parameters.repos {
                context
                    .update_state_message(format!("searching {} from {alias}", parameters.id))
                    .await
                    .map_err(|error| error.to_string())?;
                let Some(repo) = config
                    .repos
                    .iter()
                    .find(|repo| repo.effective_name() == alias)
                    .cloned()
                else {
                    let message = linyaps_i18n::format("repo {} not found", &[&alias]);
                    context
                        .send_message(message)
                        .await
                        .map_err(|error| error.to_string())?;
                    continue;
                };
                let client = match RemoteRepositoryClient::new(&repo.url) {
                    Ok(client) => client,
                    Err(error) => {
                        let message = linyaps_i18n::format(
                            "failed to search {} from {}: {}",
                            &[&parameters.id, &alias, &error],
                        );
                        context
                            .send_message(message)
                            .await
                            .map_err(|error| error.to_string())?;
                        continue;
                    }
                };
                match client.search_packages(&fuzzy, &repo, false).await {
                    Ok(found) if !found.is_empty() => {
                        packages.insert(repo.effective_name().to_string(), found);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let message = linyaps_i18n::format(
                            "failed to search {} from {}: {}",
                            &[&parameters.id, &alias, &error],
                        );
                        context
                            .send_message(message)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
            Ok(packages)
        }
    })
    .with_install(move |parameters, context| {
        operations::install(install_repository.clone(), parameters, context)
    })
    .with_install_file(move |file, file_type, options, context| {
        operations::install_file(
            install_file_repository.clone(),
            file,
            file_type,
            options,
            context,
        )
    })
    .with_uninstall(move |parameters, context| {
        operations::uninstall(uninstall_repository.clone(), parameters, context)
    })
    .with_update(move |parameters, context| {
        operations::update(update_repository.clone(), parameters, context)
    })
    .with_prune(move || operations::prune(prune_repository.clone()))
    .with_init_run_context(move |config, container_id| {
        let repository = init_run_repository.clone();
        async move {
            let repository = repository.lock().await;
            run_context::initialize(&repository, &config, &container_id)
                .map_err(|error| format!("{error:#}"))
        }
    });
    Ok(if use_polkit {
        service.with_authorizer(|action, sender| async move {
            polkit::authorize(&action, &sender).await
        })
    } else {
        service
    })
}

fn start_deferred_uninstall(repository: operations::SharedRepository) {
    let timeout = match env::var("LINGLONG_DEFERRED_TIMEOUT") {
        Ok(value) => match value.parse::<u64>() {
            Ok(seconds) => Duration::from_secs(seconds),
            Err(error) => {
                eprintln!("warning: failed to parse LINGLONG_DEFERRED_TIMEOUT[{value}]: {error}");
                Duration::from_secs(3600)
            }
        },
        Err(_) => Duration::from_secs(3600),
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(timeout).await;
            if let Err(error) = operations::deferred_uninstall(repository.clone()).await {
                eprintln!("warning: deferred uninstall failed: {error}");
            }
        }
    });
}

fn default_config() -> RepoConfigV2 {
    RepoConfigV2 {
        default_repo: "stable".to_string(),
        repos: vec![Repo {
            alias: None,
            mirror_enabled: None,
            name: "stable".to_string(),
            priority: 0,
            url: "https://mirror-repo-linglong.deepin.com".to_string(),
        }],
        version: 2,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::os::fd::AsFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use super::*;
    use linyaps_api::{PackageInfoV2, PackageManagerUpdateParameters};
    use linyaps_core::Architecture;
    use linyaps_dbus::PackageManagerAsyncClient;
    use ostrya::{
        Checksum, CommitModifier, CommitModifierFlags, CommitOptions, CreateOptions, MutableTree,
        RepoMode,
    };

    struct RepoLockEnv(Option<OsString>);

    impl RepoLockEnv {
        fn set(path: &Path) -> Self {
            let previous = env::var_os(linyaps_core::repo_lock::REPO_LOCK_ENV);
            unsafe { env::set_var(linyaps_core::repo_lock::REPO_LOCK_ENV, path) };
            Self(previous)
        }
    }

    impl Drop for RepoLockEnv {
        fn drop(&mut self) {
            if let Some(previous) = &self.0 {
                unsafe { env::set_var(linyaps_core::repo_lock::REPO_LOCK_ENV, previous) };
            } else {
                unsafe { env::remove_var(linyaps_core::repo_lock::REPO_LOCK_ENV) };
            }
        }
    }

    struct UpdateServer {
        address: SocketAddr,
        stop: std::sync::Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl UpdateServer {
        fn start(response: String, archive: PathBuf, commit: Checksum, reference: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let ref_path = format!("/repos/stable/refs/heads/{reference}");
            let worker = thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_http_request(&mut stream);
                            let path = request
                                .lines()
                                .next()
                                .and_then(|line| line.split_whitespace().nth(1))
                                .unwrap_or("/");
                            if path == "/api/v0/apps/fuzzysearchapp" {
                                write_http_response(&mut stream, 200, response.as_bytes());
                            } else if path == ref_path {
                                write_http_response(
                                    &mut stream,
                                    200,
                                    format!("{}\n", commit.to_hex()).as_bytes(),
                                );
                            } else if let Some(relative) =
                                path.strip_prefix("/repos/stable/objects/")
                            {
                                match std::fs::read(archive.join("objects").join(relative)) {
                                    Ok(bytes) => write_http_response(&mut stream, 200, &bytes),
                                    Err(_) => write_http_response(&mut stream, 404, b"not found"),
                                }
                            } else {
                                write_http_response(&mut stream, 404, b"not found");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                address,
                stop,
                worker: Some(worker),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.address)
        }
    }

    impl Drop for UpdateServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Not Found" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    }

    async fn create_archive_layer(source: &Path, archive: &Path) -> Checksum {
        let repo = ostrya::Repo::create(archive, CreateOptions::new(RepoMode::Archive))
            .await
            .unwrap();
        let transaction = repo.transaction().await.unwrap();
        let mut tree = MutableTree::new();
        let mut modifier = CommitModifier::new(
            CommitModifierFlags::CANONICAL_PERMISSIONS | CommitModifierFlags::GENERATE_SIZES,
        );
        let source = std::fs::File::open(source).unwrap();
        transaction
            .write_dfd_to_mtree(
                source.as_fd(),
                Path::new("."),
                &mut tree,
                Some(&mut modifier),
            )
            .await
            .unwrap();
        let root = transaction.write_mtree(&mut tree).await.unwrap();
        let commit = transaction
            .write_commit(CommitOptions::default(), &root)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        commit
    }

    fn package_info(id: &str, kind: &str, version: &str) -> PackageInfoV2 {
        PackageInfoV2 {
            arch: vec![Architecture::current().unwrap().to_string()],
            base: String::new(),
            channel: "main".to_string(),
            command: None,
            compatible_version: None,
            description: None,
            extension_implementation: None,
            extensions: None,
            id: id.to_string(),
            kind: kind.to_string(),
            module: "binary".to_string(),
            name: id.to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 0,
            uuid: None,
            version: version.to_string(),
        }
    }

    fn write_layer(path: &Path, info: &PackageInfoV2) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("info.json"), serde_json::to_vec(info).unwrap()).unwrap();
        std::fs::write(path.join("payload"), &info.version).unwrap();
    }

    #[test]
    fn parses_original_daemon_modes() {
        let ParseOutcome::Run(options) = parse_options_from(
            [
                "ll-package-manager",
                "--no-dbus",
                "--peer-socket",
                "/tmp/package-manager.socket",
            ]
            .map(OsString::from),
        ) else {
            panic!("peer options should parse");
        };
        assert!(options.no_dbus);
        assert_eq!(
            options.peer_socket.as_deref(),
            Some("/tmp/package-manager.socket")
        );

        let ParseOutcome::Run(options) = parse_options_from(
            [
                "ll-package-manager",
                "--init-run",
                "{}",
                "--id",
                "container",
            ]
            .map(OsString::from),
        ) else {
            panic!("init-run options should parse");
        };
        assert_eq!(options.container_id.as_deref(), Some("container"));
    }

    #[test]
    fn preserves_original_parse_error_codes() {
        let ParseOutcome::Error { code, message } =
            parse_options_from(["ll-package-manager", "--init-run", "{}"].map(OsString::from))
        else {
            panic!("missing id should fail");
        };
        assert_eq!(code, 107);
        assert_eq!(message, "--init-run requires --id");

        let ParseOutcome::Error { code, message } =
            parse_options_from(["ll-package-manager", "--peer-socket"].map(OsString::from))
        else {
            panic!("missing peer socket value should fail");
        };
        assert_eq!(code, 114);
        assert_eq!(message, ": 1 required TEXT missing");
    }

    #[test]
    fn default_config_matches_installed_upstream_config() {
        let config = default_config();
        assert_eq!(config.default_repo, "stable");
        assert_eq!(config.repos[0].name, "stable");
        assert_eq!(
            config.repos[0].url,
            "https://mirror-repo-linglong.deepin.com"
        );
    }

    #[tokio::test]
    async fn peer_service_round_trips_configuration_and_exits_on_disconnect() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        let socket = temporary.path().join("package-manager.socket");
        let server_root = root.clone();
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { serve_peer(&server_root, &server_socket).await });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists());

        let client = PackageManagerAsyncClient::peer(&socket).await.unwrap();
        let mut config = client.configuration().await.unwrap();
        assert_eq!(config.default_repo, "stable");
        config.default_repo = "testing".to_string();
        config.repos.push(Repo {
            alias: None,
            mirror_enabled: None,
            name: "testing".to_string(),
            priority: 10,
            url: "https://example.invalid/repo".to_string(),
        });
        client.set_configuration(config.clone()).await.unwrap();
        assert_eq!(client.configuration().await.unwrap(), config);

        drop(client);
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("peer server did not exit after disconnect")
            .unwrap()
            .unwrap();
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn update_without_auto_prune_keeps_old_dependency() {
        let _environment_lock = operations::TEST_REPO_LOCK_ENV_MUTEX.lock().await;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        let socket = temporary.path().join("package-manager.socket");
        let app_dir = temporary.path().join("app");
        let old_base_dir = temporary.path().join("base-old");
        let new_base_dir = temporary.path().join("base-new");
        let remote_archive = temporary.path().join("remote-archive");
        let repo_lock = temporary.path().join("repository.lock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&repo_lock, []).unwrap();
        let _repo_lock_env = RepoLockEnv::set(&repo_lock);

        let old_base = package_info("base.id", "base", "23.0.0.1");
        let new_base = package_info("base.id", "base", "23.0.0.2");
        let mut app = package_info("app.id", "app", "1.0.0.0");
        app.base = "base.id/23.0.0".to_string();
        write_layer(&old_base_dir, &old_base);
        write_layer(&new_base_dir, &new_base);
        write_layer(&app_dir, &app);

        let response = serde_json::json!({
            "code": 200,
            "data": [{
                "appId": new_base.id.clone(),
                "arch": new_base.arch[0].clone(),
                "base": "",
                "channel": new_base.channel.clone(),
                "kind": new_base.kind.clone(),
                "module": new_base.module.clone(),
                "name": new_base.name.clone(),
                "version": new_base.version.clone(),
            }]
        })
        .to_string();
        let remote_commit = create_archive_layer(&new_base_dir, &remote_archive).await;
        let reference = format!(
            "{}/{}/{}/{}/{}",
            new_base.channel, new_base.id, new_base.version, new_base.arch[0], new_base.module
        );
        let remote_server = UpdateServer::start(response, remote_archive, remote_commit, reference);
        let remote_url = remote_server.url();
        let config = RepoConfigV2 {
            default_repo: "stable".to_string(),
            repos: vec![Repo {
                alias: None,
                mirror_enabled: None,
                name: "stable".to_string(),
                priority: 0,
                url: remote_url,
            }],
            version: 2,
        };
        let mut repository = LocalRepository::create(&root, config).await.unwrap();
        repository
            .import_layer_dir(&old_base_dir, &[], None)
            .await
            .unwrap();
        repository
            .import_layer_dir(&new_base_dir, &[], None)
            .await
            .unwrap();
        let new_base_reference = linyaps_repository::reference_from_info(&new_base).unwrap();
        assert!(
            repository
                .mark_layer_deleted(&new_base_reference, "binary")
                .unwrap()
        );
        repository
            .import_layer_dir(&app_dir, &[], None)
            .await
            .unwrap();
        drop(repository);

        let server_root = root.clone();
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { serve_peer(&server_root, &server_socket).await });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = PackageManagerAsyncClient::peer(&socket).await.unwrap();
        let result = client
            .update(PackageManagerUpdateParameters {
                deps_only: true,
                no_auto_prune: Some(true),
                packages: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(result.code, 0);
        drop(client);
        server.await.unwrap().unwrap();

        let repository = LocalRepository::open(&root).await.unwrap();
        let versions = repository
            .list_layer_items()
            .into_iter()
            .filter(|item| item.info.id == "base.id")
            .map(|item| item.info.version)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            versions,
            BTreeSet::from(["23.0.0.1".to_string(), "23.0.0.2".to_string()])
        );
        assert!(repository.list_deleted_layer_items().is_empty());
    }
}
