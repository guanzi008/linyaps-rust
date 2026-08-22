use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;

use async_lock::Mutex;
use clap::{Args, Parser, Subcommand, ValueEnum};
use linyaps_api::{
    CliContainer, CommonOptions, ContainerProcessStateInfo, LayerInfo,
    PackageInfoDisplay, PackageInfoV2, PackageManagerInstallPackage,
    PackageManagerInstallParameters, PackageManagerPackage,
    PackageManagerUninstallParameters, PackageManagerUpdateParameters, RepoConfigV2,
    UpgradeListResult,
};
use linyaps_core::repository::priority_grouped_repos;
use linyaps_core::runtime_paths::executable_path;
use linyaps_core::{
    Architecture, FuzzyReference, Reference, RepoOperation, RepoOperationResult, Version,
    apply_repo_operation,
};
use linyaps_repository::{
    LocalRepository, OperationContext, OperationResult, RemotePackages, RemoteRepositoryClient,
    operations, read_layer_info,
};

mod analysis;
mod cli11;
mod frozen_help;
mod localized_help;
mod namespace;
mod runtime;
mod run_context;
#[cfg(feature = "wayland-security-context")]
mod wayland_security;

#[derive(Debug, Parser)]
#[command(
    name = "ll-cli",
    about = "linyaps CLI\nA CLI program to run application and manage application and runtime",
    disable_help_subcommand = true,
    disable_version_flag = true
)]
struct Cli {
    #[arg(long = "help-all", global = true)]
    help_all: bool,
    #[arg(long, hide = true)]
    version: bool,
    #[arg(long)]
    json: bool,
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
    #[arg(long)]
    no_progress: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Run an application")]
    Run(Run),
    #[command(about = "List running applications")]
    Ps(Ps),
    #[command(about = "Enter the namespace where the application is running")]
    Enter(Enter),
    #[command(about = "Stop running applications")]
    Kill(Kill),
    #[command(about = "Installing an application or runtime")]
    Install(Install),
    #[command(about = "Uninstall the application or runtimes")]
    Uninstall(Uninstall),
    #[command(about = "Upgrade the application or runtimes")]
    Upgrade(Upgrade),
    #[command(about = "Search applications and runtimes from remote repositories")]
    Search(Search),
    #[command(about = "List installed applications, bases or runtimes")]
    List(List),
    #[command(about = "Analyze installed applications")]
    Analyze(Analyze),
    #[command(about = "Display or modify repository information")]
    Repo(Repo),
    #[command(about = "Display information about installed applications or runtimes")]
    Info(App),
    #[command(about = "Display exported files of an installed application")]
    Content(App),
    #[command(about = "Remove unused bases or runtimes")]
    Prune,
    #[command(about = "Inspect installation paths", hide = true)]
    Inspect(Inspect),
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
struct Run {
    app: String,
    #[arg(long = "file", num_args = 0..)]
    files: Vec<String>,
    #[arg(long = "url", num_args = 0..)]
    urls: Vec<String>,
    #[arg(long = "env", value_parser = environment)]
    environment: Vec<String>,
    #[arg(long)]
    base: Option<String>,
    #[arg(long)]
    runtime: Option<String>,
    #[arg(long)]
    workdir: Option<String>,
    #[arg(long, value_delimiter = ',')]
    extensions: Vec<String>,
    #[arg(long, overrides_with = "disable_xdp")]
    enable_xdp: bool,
    #[arg(long, overrides_with = "enable_xdp")]
    disable_xdp: bool,
    #[arg(long)]
    enable_pipewire: Option<bool>,
    #[arg(long)]
    enable_atspi: Option<bool>,
    #[arg(long, hide = true)]
    run_context: Option<String>,
    #[arg(long, hide = true)]
    privileged: bool,
    #[arg(long, value_delimiter = ',', hide = true)]
    caps_add: Vec<String>,
    #[arg(
        long,
        value_delimiter = ',',
        default_values_t = default_cdi_directories()
    )]
    cdi_spec_dir: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    device: Vec<String>,
    #[arg(long, value_delimiter = ',', value_enum)]
    device_mode: Vec<DeviceMode>,
    #[arg(long)]
    instance: Option<String>,
    #[arg(long)]
    debug: bool,
    #[arg(long, default_value = "127.0.0.1:2345", requires = "debug")]
    debug_listen: String,
    #[arg(long, requires = "debug")]
    debug_debuginfod: Option<String>,
    #[arg(long, requires = "debug")]
    debug_symbol_dir: Option<String>,
    command: Vec<String>,
}

#[derive(Clone, Debug, ValueEnum)]
enum DeviceMode {
    Passthru,
}

#[derive(Debug, Args)]
struct Ps {
    #[arg(long)]
    no_truncated: bool,
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
struct Enter {
    instance: String,
    #[arg(long = "working-directory")]
    working_directory: Option<String>,
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct Kill {
    #[arg(short = 's', long = "signal", default_value = "SIGTERM")]
    signal: String,
    app: String,
}

#[derive(Debug, Args)]
struct Install {
    app: String,
    #[arg(long)]
    module: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    force: bool,
    #[arg(short = 'y')]
    confirm: bool,
    #[arg(long)]
    no_auto_prune: bool,
}

#[derive(Debug, Args)]
struct Uninstall {
    app: String,
    #[arg(long)]
    module: Option<String>,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    no_auto_prune: bool,
    #[arg(long, hide = true)]
    prune: bool,
    #[arg(long, hide = true)]
    all: bool,
}

#[derive(Debug, Args)]
struct Upgrade {
    app: Option<String>,
    #[arg(long)]
    deps_only: bool,
    #[arg(long)]
    no_auto_prune: bool,
}

#[derive(Debug, Args)]
struct Search {
    keywords: String,
    #[arg(long = "type", default_value = "all")]
    package_type: String,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    dev: bool,
    #[arg(long)]
    show_all_version: bool,
}

#[derive(Debug, Args)]
struct List {
    #[arg(long = "type", default_value = "all")]
    package_type: String,
    #[arg(long)]
    upgradable: bool,
}

#[derive(Debug, Args)]
#[command(disable_help_subcommand = true)]
struct Analyze {
    #[command(subcommand)]
    command: AnalyzeCommand,
}

#[derive(Debug, Subcommand)]
enum AnalyzeCommand {
    Size {
        #[arg(long = "sort", value_enum, default_value_t = SortField::Actual)]
        sort: SortField,
        #[arg(long)]
        asc: bool,
    },
    Depends {
        app: Option<String>,
    },
}

#[derive(Clone, Debug, ValueEnum)]
enum SortField {
    Actual,
    Logical,
    Exclusive,
    Shared,
    Id,
}

#[derive(Debug, Args)]
struct App {
    app: String,
}

#[derive(Debug, Args)]
#[command(disable_help_subcommand = true)]
struct Inspect {
    #[command(subcommand)]
    command: InspectCommand,
}

#[derive(Debug, Subcommand)]
enum InspectCommand {
    Dir {
        app: String,
        #[arg(short = 't', long = "type", default_value = "layer")]
        directory_type: String,
        #[arg(short = 'm', long)]
        module: Option<String>,
    },
}

#[derive(Debug, Args)]
#[command(disable_help_subcommand = true)]
struct Repo {
    #[command(subcommand)]
    command: RepoCommand,
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    Add {
        name: String,
        url: String,
        #[arg(long)]
        alias: Option<String>,
    },
    #[command(hide = true)]
    Modify {
        url: String,
        #[arg(long)]
        name: Option<String>,
    },
    Remove {
        alias: String,
    },
    Update {
        alias: String,
        url: String,
    },
    SetDefault {
        alias: String,
    },
    Show,
    SetPriority {
        alias: String,
        priority: i64,
    },
    EnableMirror {
        alias: String,
    },
    DisableMirror {
        alias: String,
    },
}

impl From<RepoCommand> for RepoOperation {
    fn from(command: RepoCommand) -> Self {
        match command {
            RepoCommand::Add { name, url, alias } => RepoOperation::Add { name, url, alias },
            RepoCommand::Modify { .. } => RepoOperation::Modify,
            RepoCommand::Remove { alias } => RepoOperation::Remove { alias },
            RepoCommand::Update { alias, url } => RepoOperation::Update { alias, url },
            RepoCommand::SetDefault { alias } => RepoOperation::SetDefault { alias },
            RepoCommand::Show => RepoOperation::Show,
            RepoCommand::SetPriority { alias, priority } => {
                RepoOperation::SetPriority { alias, priority }
            }
            RepoCommand::EnableMirror { alias } => RepoOperation::EnableMirror { alias },
            RepoCommand::DisableMirror { alias } => RepoOperation::DisableMirror { alias },
        }
    }
}

fn environment(value: &str) -> Result<String, String> {
    value
        .contains('=')
        .then(|| value.to_string())
        .ok_or_else(|| {
            linyaps_i18n::gettext(
                "Input parameter is invalid, please input valid parameter instead",
            )
            .into_owned()
        })
}

fn default_cdi_directories() -> Vec<String> {
    ["/etc/linglong/cdi", "/etc/cdi", "/var/run/cdi"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[tokio::main]
async fn main() {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let no_arguments = arguments.is_empty();
    if let Some(help) = frozen_help::requested(&arguments) {
        print!("{help}");
        return;
    }
    let cli = match cli11::parse(&arguments) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(error.code);
        }
    };
    let _ = cli.help_all;
    if cli.version {
        if cli.json {
            println!(
                "{}",
                serde_json::json!({ "version": linyaps_core::VERSION_FULL })
            );
        } else {
            println!(
                "{}{}",
                linyaps_i18n::gettext("linyaps CLI version "),
                linyaps_core::VERSION_FULL
            );
        }
        return;
    }
    if no_arguments {
        print!("{}", frozen_help::minimal());
        return;
    }
    let runtime = oci_runtime_binary();
    if executable_path(&runtime, &[PathBuf::from("/usr/bin")]).is_none() {
        std::process::exit(-1);
    }
    let Some(command) = cli.command else {
        print!("{}", frozen_help::expanded_root());
        std::process::exit(-1);
    };
    let result = match command {
        Command::Repo(repo) => run_repo(repo.command, cli.json).await,
        Command::Search(search) => run_search(search, cli.json).await,
        Command::Run(run) => runtime::run(run).await,
        Command::Install(install) => run_install(install, cli.json).await,
        Command::Uninstall(uninstall) => run_uninstall(uninstall, cli.json).await,
        Command::Upgrade(upgrade) => run_upgrade(upgrade, cli.json).await,
        Command::List(list) => run_list(list, cli.json).await,
        Command::Info(app) => run_info(app, cli.json).await,
        Command::Content(app) => run_content(app, cli.json).await,
        Command::Inspect(inspect) => run_inspect(inspect).await,
        Command::Ps(options) => run_ps(options, cli.json),
        Command::Enter(options) => run_enter(options),
        Command::Kill(options) => run_kill(options),
        Command::Prune => run_prune(cli.json).await,
        Command::Analyze(analyze) => analysis::run(analyze, cli.json).await,
    };
    if let Err(error) = result {
        if cli.json {
            println!("{}", serde_json::json!({ "code": -1, "message": error }));
        } else {
            eprintln!("{error}");
        }
        std::process::exit(-1);
    }
}

async fn run_prune(json: bool) -> Result<(), String> {
    let repository = Arc::new(Mutex::new(open_local_repository().await?));
    let packages = operations::prune(repository)
        .await
        .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&packages).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if packages.is_empty() {
        println!("No unused base or runtime.");
        return Ok(());
    }
    println!("Unused base or runtime:");
    for package in &packages {
        println!(
            "{}",
            linyaps_repository::reference_from_info(package).map_err(|error| error.to_string())?
        );
    }
    println!(
        "{} unused base or runtime have been removed.",
        packages.len()
    );
    Ok(())
}

async fn run_repo(command: RepoCommand, json: bool) -> Result<(), String> {
    let mut repository = open_local_repository().await?;
    let mut config = repository.config().clone();
    match apply_repo_operation(&mut config, command.into()).map_err(|error| error.to_string())? {
        RepoOperationResult::Unchanged => Ok(()),
        RepoOperationResult::Changed => repository
            .update_config(config)
            .map_err(|error| error.to_string()),
        RepoOperationResult::Show(config) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&config).map_err(|error| error.to_string())?
                );
            } else {
                print!("{}", format_repo_config(&config));
            }
            Ok(())
        }
    }
}

async fn run_install(options: Install, json: bool) -> Result<(), String> {
    let path = Path::new(&options.app);
    if path.exists() {
        if !path.is_file() {
            return Err(format!(
                "{} is not a regular file; expected a .layer or .uab file.",
                options.app
            ));
        }
        let file_type = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        if !matches!(file_type, "layer" | "uab") {
            return Err(format!(
                "Unsupported file format .{file_type}; expected a .layer or .uab file."
            ));
        }
        let file = std::fs::File::open(path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        let repository = Arc::new(Mutex::new(open_local_repository().await?));
        let context = OperationContext::new();
        let result = operations::install_file(
            repository,
            file,
            file_type.to_string(),
            CommonOptions {
                force: options.force,
                no_auto_prune: Some(options.no_auto_prune),
                skip_interaction: options.confirm,
            },
            context,
        )
        .await;
        return print_operation_result(result, json);
    }
    if path.is_absolute() || options.app.starts_with("./") || options.app.starts_with("../") {
        return Err(format!("Package file {} does not exist.", options.app));
    }
    let fuzzy = options
        .app
        .parse::<FuzzyReference>()
        .map_err(|error| error.to_string())?;
    let modules = options
        .module
        .map(|module| vec![module])
        .unwrap_or_else(auto_module_list);
    let parameters = PackageManagerInstallParameters {
        options: CommonOptions {
            force: options.force,
            no_auto_prune: Some(options.no_auto_prune),
            skip_interaction: options.confirm,
        },
        package: PackageManagerInstallPackage {
            channel: fuzzy.channel,
            id: fuzzy.id,
            modules: Some(modules),
            version: fuzzy.version,
        },
        repo: options.repo,
    };
    let repository = Arc::new(Mutex::new(open_local_repository().await?));
    let context = OperationContext::new();
    let result = operations::install(repository, parameters, context).await;
    print_operation_result(result, json)
}

async fn run_uninstall(options: Uninstall, json: bool) -> Result<(), String> {
    let _legacy_compatibility_flags = (options.prune, options.all);
    let fuzzy = options
        .app
        .parse::<FuzzyReference>()
        .map_err(|error| error.to_string())?;
    let parameters = PackageManagerUninstallParameters {
        options: CommonOptions {
            force: options.force,
            no_auto_prune: Some(options.no_auto_prune),
            skip_interaction: false,
        },
        package: PackageManagerPackage {
            channel: fuzzy.channel,
            id: fuzzy.id,
            module: options.module,
            version: fuzzy.version,
        },
    };
    let repository = Arc::new(Mutex::new(open_local_repository().await?));
    let context = OperationContext::new();
    let result = operations::uninstall(repository, parameters, context).await;
    print_operation_result(result, json)
}

async fn run_upgrade(options: Upgrade, json: bool) -> Result<(), String> {
    let packages = if let Some(app) = options.app {
        let fuzzy = app
            .parse::<FuzzyReference>()
            .map_err(|error| error.to_string())?;
        let repository = open_local_repository().await?;
        let reference = repository
            .resolve_local(&fuzzy, false)
            .map_err(|_| linyaps_i18n::format("Application {} is not installed.", &[&app]))?;
        let item = repository
            .layer_item(&reference, "binary")
            .map_err(|error| error.to_string())?;
        if item.info.kind != "app" {
            return Err(linyaps_i18n::format("{} is not an application.", &[&app]));
        }
        vec![PackageManagerPackage {
            channel: Some(reference.channel),
            id: reference.id,
            module: None,
            version: Some(reference.version.to_string()),
        }]
    } else {
        Vec::new()
    };
    let repository = Arc::new(Mutex::new(open_local_repository().await?));
    let context = OperationContext::new();
    let result = operations::update(
        repository,
        PackageManagerUpdateParameters {
            deps_only: options.deps_only,
            no_auto_prune: Some(options.no_auto_prune),
            packages,
        },
        context,
    )
    .await;
    print_operation_result(result, json)
}

fn auto_module_list() -> Vec<String> {
    let variables = [
        "LANG",
        "LC_ADDRESS",
        "LC_ALL",
        "LC_IDENTIFICATION",
        "LC_MEASUREMENT",
        "LC_MESSAGES",
        "LC_MONETARY",
        "LC_NAME",
        "LC_NUMERIC",
        "LC_PAPER",
        "LC_TELEPHONE",
        "LC_TIME",
    ];
    let mut modules = vec!["binary".to_string()];
    for language in variables
        .into_iter()
        .filter_map(|variable| env::var(variable).ok())
    {
        modules.extend(language_modules(&language));
    }
    modules.sort();
    modules.dedup();
    modules
}

fn language_modules(language: &str) -> Vec<String> {
    let bytes = language.as_bytes();
    if bytes.len() < 2 || !bytes[..2].iter().all(u8::is_ascii_lowercase) {
        return Vec::new();
    }
    let mut modules = vec![format!("lang_{}", &language[..2])];
    if bytes.len() == 2 || matches!(bytes.get(2), Some(b'.' | b'@')) {
        return modules;
    }
    if bytes.get(2) != Some(&b'_')
        || bytes.len() < 5
        || !bytes[3..5].iter().all(u8::is_ascii_alphabetic)
    {
        return Vec::new();
    }
    modules.push(format!("lang_{}", &language[..5]));
    if bytes.len() == 5 || matches!(bytes.get(5), Some(b'.' | b'@')) {
        modules
    } else {
        Vec::new()
    }
}

fn print_operation_result(result: Result<OperationResult, String>, json: bool) -> Result<(), String> {
    let result = result?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&result).map_err(|error| error.to_string())?
        );
    } else if !result.message.is_empty() {
        println!("{}", result.message);
    }
    Ok(())
}

async fn open_local_repository() -> Result<LocalRepository, String> {
    LocalRepository::open(repository_root())
        .await
        .map_err(|error| error.to_string())
}

fn repository_root() -> PathBuf {
    env::var_os("LINGLONG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let data_dir = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    let home = std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("/"));
                    home.join(".local").join("share")
                });
            data_dir.join("linglong")
        })
}

async fn run_search(options: Search, json: bool) -> Result<(), String> {
    let repository = open_local_repository().await?;
    let config = repository.config().clone();
    let mut repos = config.repos;
    repos.sort_by_key(|repo| Reverse(repo.priority));
    if repos.is_empty() {
        return Err("no repo found".to_string());
    }
    if let Some(alias) = options.repo.as_deref() {
        repos.retain(|repo| repo.effective_name() == alias);
        if repos.is_empty() {
            return Err(format!("repo {alias} not found"));
        }
    }
    FuzzyReference::new(None, &options.keywords, None, None).map_err(|error| error.to_string())?;
    let mut packages = BTreeMap::new();
    for repo in &repos {
        let client = RemoteRepositoryClient::new(&repo.url)
            .map_err(|error| error.to_string())?;
        let fuzzy = FuzzyReference::new(None, &options.keywords, None, None)
            .map_err(|error| error.to_string())?;
        match client.search_packages(&fuzzy, repo, true).await {
            Ok(found) if !found.is_empty() => {
                packages.insert(repo.effective_name().to_string(), found);
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("failed to search {}: {error}", options.keywords);
            }
        }
    }
    print_search_results(&mut packages, &options, json)
}

fn print_search_results(
    packages: &mut BTreeMap<String, Vec<PackageInfoV2>>,
    options: &Search,
    json: bool,
) -> Result<(), String> {
    filter_search_results(packages, options);
    sort_search_results(packages);
    if json {
        println!(
            "{}",
            serde_json::to_string(&packages).map_err(|error| error.to_string())?
        );
    } else if packages.is_empty() {
        eprintln!(
            "{}",
            linyaps_i18n::gettext("No packages found in the remote repo.")
        );
    } else {
        print!("{}", format_search_table(packages));
    }
    Ok(())
}

fn filter_search_results(packages: &mut BTreeMap<String, Vec<PackageInfoV2>>, options: &Search) {
    if !options.dev {
        for found in packages.values_mut() {
            found.retain(|package| package.module != "develop");
        }
    }
    if options.package_type != "all" {
        packages.retain(|_, found| {
            found.retain(|package| package.kind == options.package_type);
            !found.is_empty()
        });
    }
    if options.show_all_version {
        return;
    }
    for found in packages.values_mut() {
        let mut latest = BTreeMap::<(String, String), PackageInfoV2>::new();
        for package in std::mem::take(found) {
            let key = (package.id.clone(), package.module.clone());
            match latest.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(package);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let replace = match (
                        Version::parse(&entry.get().version),
                        Version::parse(&package.version),
                    ) {
                        (Ok(current), Ok(candidate)) => current < candidate,
                        _ => false,
                    };
                    if replace {
                        entry.insert(package);
                    }
                }
            }
        }
        *found = latest.into_values().collect();
    }
}

fn sort_search_results(packages: &mut BTreeMap<String, Vec<PackageInfoV2>>) {
    for found in packages.values_mut() {
        found.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.channel.cmp(&right.channel))
                .then_with(|| left.module.cmp(&right.module))
                .then_with(|| compare_versions_desc(&left.version, &right.version))
        });
    }
}

fn compare_versions_desc(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => right.partial_cmp(&left).unwrap_or(Ordering::Equal),
        _ => Ordering::Equal,
    }
}

fn format_search_table(packages: &BTreeMap<String, Vec<PackageInfoV2>>) -> String {
    let mut output = format!(
        "\x1b[38;5;214m{}{}{}{}{}{}{}\x1b[0m\n",
        display_column(&linyaps_i18n::gettext("ID"), 43),
        display_column(&linyaps_i18n::gettext("Name"), 33),
        display_column(&linyaps_i18n::gettext("Version"), 16),
        display_column(&linyaps_i18n::gettext("Channel"), 16),
        display_column(&linyaps_i18n::gettext("Module"), 12),
        display_column(&linyaps_i18n::gettext("Repo"), 10),
        linyaps_i18n::gettext("Description"),
    );
    for (repo, found) in packages {
        for package in found {
            let name = truncate_display(&simplify(&package.name), 29, 33);
            let description = truncate_display(
                &simplify(package.description.as_deref().unwrap_or_default()),
                53,
                56,
            );
            output.push_str(&format!(
                "{}{}{}{}{}{}{}\n",
                display_column_with_space(&package.id, 43),
                display_column_with_space(&name, 33),
                display_column_with_space(&package.version, 16),
                display_column_with_space(&package.channel, 16),
                display_column_with_space(&package.module, 12),
                display_column_with_space(repo, 10),
                description,
            ));
        }
    }
    output
}

async fn run_list(options: List, json: bool) -> Result<(), String> {
    let repository = open_local_repository().await?;
    if options.upgradable {
        return run_list_upgradable(&repository, json).await;
    }
    let mut packages = repository
        .list_layer_items()
        .into_iter()
        .map(|item| {
            let install_time = repository.layer_create_time(&item).ok().flatten();
            let mut package = PackageInfoDisplay::from(item.info);
            package.install_time = install_time;
            package
        })
        .collect::<Vec<_>>();
    if options.package_type != "all" {
        packages.retain(|package| package.kind == options.package_type);
    }
    packages.sort_by(|left, right| left.id.cmp(&right.id));

    if json {
        println!(
            "{}",
            serde_json::to_string(&packages).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", format_package_table(&packages));
    }
    Ok(())
}

async fn run_list_upgradable(repository: &LocalRepository, json: bool) -> Result<(), String> {
    let architecture = Architecture::current().map_err(|error| error.to_string())?;
    let mut upgrades = Vec::new();
    for local in latest_local_apps(repository.list_layer_items()) {
        let fuzzy = match FuzzyReference::new(
            Some(local.channel.clone()),
            &local.id,
            None,
            Some(architecture),
        ) {
            Ok(fuzzy) => fuzzy,
            Err(_) => continue,
        };
        let mut candidates = RemotePackages::default();
        let mut any_success = false;
        for group in priority_grouped_repos(repository.config()) {
            for repo in group {
                let Ok(client) = RemoteRepositoryClient::new(&repo.url) else {
                    continue;
                };
                let Ok(packages) = client.search_packages(&fuzzy, &repo, true).await else {
                    continue;
                };
                any_success = true;
                if !packages.is_empty() {
                    candidates.add_packages(repo, packages);
                }
            }
            if !candidates.is_empty() {
                break;
            }
        }
        if !any_success {
            continue;
        }
        let Ok((_, remote)) = candidates.latest_package() else {
            continue;
        };
        let Ok(local_reference) = reference_from_package(&local) else {
            continue;
        };
        let Ok(remote_reference) = reference_from_package(&remote) else {
            continue;
        };
        if remote_reference.version > local_reference.version {
            upgrades.push(UpgradeListResult {
                id: local_reference.id,
                new_version: remote_reference.version.to_string(),
                old_version: local_reference.version.to_string(),
            });
        }
    }
    upgrades.sort_by(|left, right| left.id.cmp(&right.id));
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&upgrades).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", format_upgrade_table(&upgrades));
    }
    Ok(())
}

fn latest_local_apps(items: Vec<linyaps_api::RepositoryCacheLayersItem>) -> Vec<PackageInfoV2> {
    let mut packages = items
        .into_iter()
        .map(|item| item.info)
        .filter(|package| package.kind == "app")
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.channel.cmp(&right.channel))
            .then_with(|| compare_valid_versions_desc(&left.version, &right.version))
    });
    packages.dedup_by(|right, left| right.id == left.id && right.channel == left.channel);
    packages
}

fn compare_valid_versions_desc(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => right.partial_cmp(&left).unwrap_or(Ordering::Equal),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => Ordering::Equal,
    }
}

fn reference_from_package(package: &PackageInfoV2) -> Result<Reference, String> {
    let architecture = package
        .arch
        .first()
        .ok_or_else(|| "package has no architecture".to_string())?
        .parse::<Architecture>()
        .map_err(|error| error.to_string())?;
    Reference::new(
        &package.channel,
        &package.id,
        Version::parse(&package.version).map_err(|error| error.to_string())?,
        architecture,
    )
    .map_err(|error| error.to_string())
}

fn format_upgrade_table(upgrades: &[UpgradeListResult]) -> String {
    if upgrades.is_empty() {
        return format!(
            "{}\n",
            linyaps_i18n::gettext("No apps available for update.")
        );
    }
    let mut id_width = 0;
    let mut installed_width = 0;
    for upgrade in upgrades {
        id_width = id_width.max(upgrade.id.len()) + 2;
        installed_width = installed_width.max(upgrade.old_version.len()) + 2;
    }
    let mut output = format!(
        "\x1b[38;5;214m{}{}{}\x1b[0m\n",
        display_column(&linyaps_i18n::gettext("ID"), id_width),
        display_column(&linyaps_i18n::gettext("Installed"), installed_width),
        linyaps_i18n::gettext("New"),
    );
    for upgrade in upgrades {
        output.push_str(&format!(
            "{}{}{}\n",
            display_column(&upgrade.id, id_width),
            display_column(&upgrade.old_version, installed_width),
            upgrade.new_version,
        ));
    }
    output
}

fn format_package_table(packages: &[PackageInfoDisplay]) -> String {
    let mut output = format!(
        "\x1b[38;5;214m{}{}{}{}{}{}\x1b[0m\n",
        display_column(&linyaps_i18n::gettext("ID"), 43),
        display_column(&linyaps_i18n::gettext("Name"), 33),
        display_column(&linyaps_i18n::gettext("Version"), 16),
        display_column(&linyaps_i18n::gettext("Channel"), 16),
        display_column(&linyaps_i18n::gettext("Module"), 12),
        linyaps_i18n::gettext("Description"),
    );
    for package in packages {
        let name = truncate_display(&simplify(&package.name), 29, 33);
        let description = truncate_display(
            &simplify(package.description.as_deref().unwrap_or_default()),
            53,
            56,
        );
        output.push_str(&format!(
            "{}{}{}{}{}{}\n",
            display_column_with_space(&package.id, 43),
            display_column_with_space(&name, 33),
            display_column_with_space(&package.version, 16),
            display_column_with_space(&package.channel, 16),
            display_column_with_space(&package.module, 12),
            description,
        ));
    }
    output
}

async fn run_info(options: App, json: bool) -> Result<(), String> {
    let path = Path::new(&options.app);
    if path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension == "layer")
    {
        let info = read_layer_info(path).map_err(|error| error.to_string())?;
        print_layer_info(&info, json)?;
        return Ok(());
    }

    let fuzzy = options
        .app
        .parse::<FuzzyReference>()
        .map_err(|error| error.to_string())?;
    let repository = open_local_repository().await?;
    let reference = repository
        .resolve_local(&fuzzy, false)
        .map_err(|error| error.to_string())?;
    let info = repository
        .read_layer_info(&reference, "binary")
        .map_err(|error| error.to_string())?;
    print_package_info(&info, json)
}

fn print_package_info(info: &PackageInfoV2, json: bool) -> Result<(), String> {
    let serialized = if json {
        serde_json::to_string(info)
    } else {
        serde_json::to_string_pretty(info)
    }
    .map_err(|error| error.to_string())?;
    println!("{serialized}");
    Ok(())
}

fn print_layer_info(info: &LayerInfo, json: bool) -> Result<(), String> {
    let serialized = if json {
        serde_json::to_string(info)
    } else {
        serde_json::to_string_pretty(&info.info)
    }
    .map_err(|error| error.to_string())?;
    println!("{serialized}");
    Ok(())
}

async fn run_content(options: App, json: bool) -> Result<(), String> {
    let fuzzy = options
        .app
        .parse::<FuzzyReference>()
        .map_err(|error| error.to_string())?;
    let repository = open_local_repository().await?;
    let reference = repository
        .resolve_local(&fuzzy, false)
        .map_err(|error| error.to_string())?;
    let item = repository
        .layer_item(&reference, "binary")
        .map_err(|error| error.to_string())?;
    if item.info.kind != "app" {
        return Err("Only supports viewing app content".to_string());
    }
    let entries = repository
        .layer_path_for_item(&item)
        .map_err(|error| error.to_string())?
        .join("entries");
    if !entries.is_dir() {
        return Err("no entries found".to_string());
    }

    let content =
        collect_exported_content(repository.root(), &entries).map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "content": content }))
                .map_err(|error| error.to_string())?
        );
    } else {
        for path in content {
            println!("{path}");
        }
    }
    Ok(())
}

fn collect_exported_content(root: &Path, entries: &Path) -> Result<Vec<String>, std::io::Error> {
    let prefer_lib_systemd_user = entries.join("lib/systemd/user").exists();
    let mut relative_paths = Vec::new();
    let mut visited = HashSet::new();
    collect_entry_paths(entries, entries, &mut visited, &mut relative_paths)?;

    Ok(relative_paths
        .into_iter()
        .filter_map(|relative| {
            let exported = resolve_entry_export_path(root, &relative, prefer_lib_systemd_user)?;
            fs::metadata(&exported)
                .is_ok_and(|metadata| metadata.is_file())
                .then(|| exported.to_string_lossy().into_owned())
        })
        .collect())
}

fn collect_entry_paths(
    root: &Path,
    directory: &Path,
    visited: &mut HashSet<PathBuf>,
    paths: &mut Vec<PathBuf>,
) -> Result<(), std::io::Error> {
    let canonical = fs::canonicalize(directory)?;
    if !visited.insert(canonical) {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if let Ok(relative) = path.strip_prefix(root) {
            paths.push(relative.to_path_buf());
        }
        if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            collect_entry_paths(root, &path, visited, paths)?;
        }
    }
    Ok(())
}

fn resolve_entry_export_path(
    root: &Path,
    relative: &Path,
    prefer_lib_systemd_user: bool,
) -> Option<PathBuf> {
    let relative_string = relative.to_string_lossy();
    if relative_string.is_empty() {
        return None;
    }
    let entries = root.join("entries");
    if relative_string.starts_with("share/applications/")
        && relative
            .extension()
            .is_some_and(|extension| extension == "desktop")
    {
        let from_share = relative.strip_prefix("share").ok()?;
        let default = entries.join("share").join(from_share);
        let overlay_share = env::var_os("LINGLONG_EXPORT_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("share"));
        let overlay = entries.join(overlay_share).join(from_share);
        if overlay != default && overlay.exists() {
            return Some(overlay);
        }
        return Some(if default.exists() { default } else { overlay });
    }

    if relative == Path::new("share/systemd/user") || relative.starts_with("share/systemd/user") {
        if prefer_lib_systemd_user {
            return None;
        }
        let suffix = relative.strip_prefix("share/systemd/user").ok()?;
        return Some(entries.join("lib/systemd/user").join(suffix));
    }
    Some(entries.join(relative))
}

async fn run_inspect(options: Inspect) -> Result<(), String> {
    match options.command {
        InspectCommand::Dir {
            app,
            directory_type,
            module,
        } => match directory_type.as_str() {
            "layer" => {
                let fuzzy = app
                    .parse::<FuzzyReference>()
                    .map_err(|error| error.to_string())?;
                let repository = open_local_repository().await?;
                let reference = repository
                    .resolve_local(&fuzzy, false)
                    .map_err(|error| error.to_string())?;
                let path = repository
                    .layer_path(&reference, module.as_deref().unwrap_or("binary"))
                    .map_err(|error| error.to_string())?;
                println!("{}", path.display());
                Ok(())
            }
            "bundle" => {
                let containers = running_container_ids(&app)?;
                let container = containers
                    .first()
                    .ok_or_else(|| "Can not find the running application.".to_string())?;
                println!(
                    "{}",
                    xdg_runtime_dir().join("linglong").join(container).display()
                );
                Ok(())
            }
            value => Err(format!(
                "Invalid type: {value}, type must be layer or bundle"
            )),
        },
    }
}

fn run_ps(options: Ps, json: bool) -> Result<(), String> {
    let mut containers = current_containers()?;
    if !options.no_truncated {
        for container in &mut containers {
            container.id.truncate(12);
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string(&containers).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", format_container_table(&containers));
    }
    Ok(())
}

fn run_enter(options: Enter) -> Result<(), String> {
    let containers = running_container_ids(&options.instance)?;
    let container = containers
        .first()
        .ok_or_else(|| "no container found".to_string())?;
    let commands = if options.command.is_empty() {
        vec![
            "/bin/bash".to_string(),
            "--noprofile".to_string(),
            "--norc".to_string(),
            "-c".to_string(),
            "source /etc/profile; bash --norc".to_string(),
        ]
    } else {
        options.command
    };
    let user = format!(
        "{}:{}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
    );
    let status = ProcessCommand::new(oci_runtime_binary())
        .args(["exec", "--user", &user, container])
        .args(&commands)
        .status()
        .map_err(|error| format!("failed to execute OCI runtime: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("OCI runtime exec failed with {status}"))
}

fn run_kill(options: Kill) -> Result<(), String> {
    let containers = running_container_ids(&options.app)?;
    if containers.is_empty() {
        return Err("no container found".to_string());
    }
    let mut failed = false;
    for container in containers {
        let status = ProcessCommand::new(oci_runtime_binary())
            .args(["kill", &container, &options.signal])
            .status()
            .map_err(|error| format!("failed to execute OCI runtime: {error}"))?;
        failed |= !status.success();
    }
    (!failed)
        .then_some(())
        .ok_or_else(|| "OCI runtime kill failed".to_string())
}

fn oci_runtime_binary() -> std::ffi::OsString {
    linyaps_core::runtime_paths::oci_runtime_binary()
}

fn current_containers() -> Result<Vec<CliContainer>, String> {
    current_containers_from_paths(&box_root(), &process_state_root(), Path::new("/proc"))
}

fn current_containers_from_paths(
    box_root: &Path,
    state_root: &Path,
    proc_root: &Path,
) -> Result<Vec<CliContainer>, String> {
    let statuses = read_box_statuses(box_root)?;
    let entries = match fs::read_dir(state_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("failed to list {}: {error}", state_root.display())),
    };
    let mut containers = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_name = entry.file_name();
        if !proc_root.join(&file_name).exists() {
            continue;
        }
        if entry.metadata().map_err(|error| error.to_string())?.len() == 0 {
            continue;
        }
        let content = match fs::read(entry.path()) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let info = match serde_json::from_slice::<ContainerProcessStateInfo>(&content) {
            Ok(info) => info,
            Err(_) => continue,
        };
        let Some(pid) = statuses.get(&info.container_id) else {
            continue;
        };
        let package = if !info.app.is_empty() {
            info.app
        } else if info
            .runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.is_empty())
        {
            info.runtime.unwrap_or_default()
        } else {
            info.base
        };
        containers.push(CliContainer {
            id: info.container_id,
            package,
            pid: *pid,
        });
    }
    Ok(containers)
}

fn read_box_statuses(root: &Path) -> Result<BTreeMap<String, i64>, String> {
    fs::create_dir_all(root)
        .map_err(|error| format!("failed to create {}: {error}", root.display()))?;
    let mut statuses = BTreeMap::new();
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let status_path = entry.path().join("status.json");
        if !status_path.exists() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(&status_path)
                .map_err(|error| format!("failed to read {}: {error}", status_path.display()))?,
        )
        .map_err(|error| format!("failed to parse {}: {error}", status_path.display()))?;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{} has no container id", status_path.display()))?;
        let pid = value
            .get("pid")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| format!("{} has no container pid", status_path.display()))?;
        statuses.insert(id.to_string(), pid);
    }
    Ok(statuses)
}

fn running_container_ids(identifier: &str) -> Result<Vec<String>, String> {
    matching_container_ids(&current_containers()?, identifier)
}

fn matching_container_ids(
    containers: &[CliContainer],
    identifier: &str,
) -> Result<Vec<String>, String> {
    let mut matches = Vec::new();
    for container in containers {
        if container.id == identifier
            || (identifier.len() >= 12 && container.id.starts_with(identifier))
        {
            matches.push(container.id.clone());
            continue;
        }
        let Ok(reference) = container.package.parse::<Reference>() else {
            continue;
        };
        if reference.id == identifier || reference.to_string() == identifier {
            matches.push(container.id.clone());
        }
    }
    if matches.len() > 1 {
        return Err(format!(
            "multiple running containers match the specified identifier '{identifier}': {matches:?}. Please specify a more specific identifier."
        ));
    }
    Ok(matches)
}

fn format_container_table(containers: &[CliContainer]) -> String {
    if containers.is_empty() {
        return format!("{}\n", linyaps_i18n::gettext("No containers are running."));
    }
    let app_header = linyaps_i18n::gettext("App");
    let id_header = linyaps_i18n::gettext("ContainerID");
    let pid_header = linyaps_i18n::gettext("Pid");
    let package_names = containers
        .iter()
        .map(|container| package_display_name(&container.package))
        .collect::<Vec<_>>();
    let package_width = package_names
        .iter()
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max(app_header.len())
        + 2;
    let id_width = containers
        .iter()
        .map(|container| container.id.len())
        .max()
        .unwrap_or(0)
        .max(id_header.len())
        + 2;
    let pid_width = containers
        .iter()
        .map(|container| container.pid.to_string().len())
        .max()
        .unwrap_or(0)
        .max(pid_header.len())
        + 2;
    let mut output = format!(
        "\x1b[38;5;214m{}{}{}\x1b[0m\n",
        display_column(&app_header, package_width),
        display_column(&id_header, id_width),
        display_column(&pid_header, pid_width),
    );
    for (container, package) in containers.iter().zip(package_names) {
        output.push_str(&format!(
            "{}{}{}\n",
            display_column(&package, package_width),
            display_column(&container.id, id_width),
            display_column(&container.pid.to_string(), pid_width),
        ));
    }
    output
}

fn package_display_name(package: &str) -> String {
    match (package.find(':'), package.find('/')) {
        (Some(colon), Some(slash)) if colon < slash => package[colon + 1..slash].to_string(),
        _ => package.to_string(),
    }
}

fn box_root() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/run/user").join(rustix::process::geteuid().as_raw().to_string())
        })
        .join("linglong/box")
}

fn process_state_root() -> PathBuf {
    linyaps_core::runtime_paths::user_process_state_root(rustix::process::getuid().as_raw())
}

fn xdg_runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/tmp").join(format!(
                "linglong-runtime-{}",
                rustix::process::getuid().as_raw()
            ))
        })
}

fn simplify(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_display(value: &str, content_width: usize, threshold: usize) -> String {
    if display_width(value) <= threshold {
        return value.to_string();
    }
    let mut output = String::new();
    let mut width = 0;
    for character in value.chars() {
        let character_width = character_display_width(character);
        if width + character_width > content_width {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push_str("...");
    output
}

fn display_column(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(value));
    format!("{value}{}", " ".repeat(padding))
}

fn display_column_with_space(value: &str, width: usize) -> String {
    display_column(&format!("{value} "), width)
}

fn display_width(value: &str) -> usize {
    value.chars().map(character_display_width).sum()
}

fn character_display_width(character: char) -> usize {
    let value = character as u32;
    if character.is_control()
        || matches!(
            value,
            0x0300..=0x036f
                | 0x1ab0..=0x1aff
                | 0x1dc0..=0x1dff
                | 0x20d0..=0x20ff
                | 0xfe20..=0xfe2f
        )
    {
        return 0;
    }
    if matches!(
        value,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1faff
            | 0x20000..=0x3fffd
    ) {
        2
    } else {
        1
    }
}

#[cfg(test)]
fn default_config() -> linyaps_api::RepoConfigV2 {
    linyaps_api::RepoConfigV2 {
        default_repo: "stable".to_string(),
        repos: vec![linyaps_api::Repo {
            alias: None,
            mirror_enabled: None,
            name: "stable".to_string(),
            priority: 0,
            url: "https://mirror-repo-linglong.deepin.com".to_string(),
        }],
        version: 2,
    }
}

fn format_repo_config(config: &RepoConfigV2) -> String {
    const MAX_URL_LENGTH: usize = 100;
    let name_width = config
        .repos
        .iter()
        .map(|repo| repo.name.len())
        .max()
        .unwrap_or(0)
        + 2;
    let url_width = config
        .repos
        .iter()
        .map(|repo| repo.url.len())
        .max()
        .unwrap_or(0)
        .min(MAX_URL_LENGTH)
        + 2;
    let alias_width = config
        .repos
        .iter()
        .map(|repo| repo.effective_name().len())
        .max()
        .unwrap_or(0)
        + 2;
    let mut output = format!("Default: {}\n", config.default_repo);
    output.push_str(&format!(
        "\x1b[38;5;214m{}{}{}{}\x1b[0m\n",
        display_column(&linyaps_i18n::gettext("Name"), name_width),
        display_column(&linyaps_i18n::gettext("Url"), url_width),
        display_column(&linyaps_i18n::gettext("Alias"), alias_width),
        display_column(&linyaps_i18n::gettext("Priority"), 10),
    ));
    let mut repos = config.repos.clone();
    repos.sort_by_key(|repo| Reverse(repo.priority));
    for repo in repos {
        let url = if repo.url.len() > MAX_URL_LENGTH {
            format!("{}...", &repo.url[..97])
        } else {
            repo.url
        };
        output.push_str(&format!(
            "{:<name_width$}{:<url_width$}{:<alias_width$}{:<10}\n",
            repo.name,
            url,
            repo.alias.as_deref().unwrap_or(&repo.name),
            repo.priority
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use linyaps_api::Repo as ApiRepo;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn help_all_flattens_visible_commands_without_adding_help_commands() {
        assert!(Cli::try_parse_from(["ll-cli", "help"]).is_err());
        assert!(Cli::try_parse_from(["ll-cli", "repo", "help"]).is_err());

        let help = include_str!("../help/help-all.txt");
        assert!(help.contains("Run an application"));
        assert!(help.contains("--device-mode"));
        assert!(!help.contains("Usage: run"));
        assert!(!help.contains("Print this message or the help of the given subcommand"));

        let run = include_str!("../help/run.txt");
        assert!(run.contains("Usage: ll-cli run"));
        assert!(!run.contains("Usage: ll-cli ps"));
    }

    #[test]
    fn parses_complete_run_surface() {
        let cli = Cli::try_parse_from([
            "ll-cli",
            "run",
            "--env",
            "A=B",
            "--extensions",
            "one,two",
            "--device-mode",
            "passthru",
            "--debug",
            "--debug-listen",
            "127.0.0.1:9999",
            "org.deepin.demo",
            "bash",
        ])
        .unwrap();
        let Some(Command::Run(run)) = cli.command else {
            panic!("unexpected command");
        };
        assert_eq!(run.extensions, ["one", "two"]);
        assert_eq!(run.command, ["bash"]);
    }

    #[test]
    fn parses_every_repository_operation() {
        for arguments in [
            vec!["ll-cli", "repo", "add", "main", "https://example.invalid"],
            vec!["ll-cli", "repo", "remove", "main"],
            vec![
                "ll-cli",
                "repo",
                "update",
                "main",
                "https://example.invalid",
            ],
            vec!["ll-cli", "repo", "set-default", "main"],
            vec!["ll-cli", "repo", "show"],
            vec!["ll-cli", "repo", "set-priority", "main", "100"],
            vec!["ll-cli", "repo", "enable-mirror", "main"],
            vec!["ll-cli", "repo", "disable-mirror", "main"],
        ] {
            Cli::try_parse_from(arguments).unwrap();
        }
    }

    #[test]
    fn converts_repository_commands_to_core_operations() {
        assert_eq!(
            RepoOperation::from(RepoCommand::SetPriority {
                alias: "stable".to_string(),
                priority: 42,
            }),
            RepoOperation::SetPriority {
                alias: "stable".to_string(),
                priority: 42,
            }
        );
        assert_eq!(
            RepoOperation::from(RepoCommand::EnableMirror {
                alias: "stable".to_string(),
            }),
            RepoOperation::EnableMirror {
                alias: "stable".to_string(),
            }
        );
    }

    #[test]
    fn formats_repository_table_in_stable_priority_order() {
        let config = RepoConfigV2 {
            default_repo: "beta".to_string(),
            repos: vec![
                ApiRepo {
                    alias: None,
                    mirror_enabled: None,
                    name: "stable".to_string(),
                    priority: 10,
                    url: "https://stable.example".to_string(),
                },
                ApiRepo {
                    alias: Some("beta".to_string()),
                    mirror_enabled: Some(true),
                    name: "testing".to_string(),
                    priority: 20,
                    url: "https://beta.example".to_string(),
                },
            ],
            version: 2,
        };
        let output = format_repo_config(&config);
        assert!(output.starts_with("Default: beta\n\x1b[38;5;214m"));
        assert!(output.find("testing").unwrap() < output.find("stable").unwrap());
        assert!(output.contains("beta"));
    }

    #[test]
    fn discovers_only_live_processes_known_to_the_runtime() {
        let temporary = tempdir().unwrap();
        let box_root = temporary.path().join("box");
        let state_root = temporary.path().join("state");
        let proc_root = temporary.path().join("proc");
        fs::create_dir_all(box_root.join("container-a")).unwrap();
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(proc_root.join("321")).unwrap();
        fs::write(
            box_root.join("container-a/status.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "container-a",
                "pid": 999
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            state_root.join("321"),
            serde_json::to_vec(&ContainerProcessStateInfo {
                app: "main:org.example.demo/1.0.0.0/x86_64".to_string(),
                base: "main:org.deepin.base/23.1.0/x86_64".to_string(),
                container_id: "container-a".to_string(),
                extensions: None,
                runtime: None,
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            state_root.join("dead"),
            serde_json::to_vec(&ContainerProcessStateInfo {
                app: "main:org.example.dead/1.0.0.0/x86_64".to_string(),
                base: String::new(),
                container_id: "container-a".to_string(),
                extensions: None,
                runtime: None,
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            current_containers_from_paths(&box_root, &state_root, &proc_root).unwrap(),
            vec![CliContainer {
                id: "container-a".to_string(),
                package: "main:org.example.demo/1.0.0.0/x86_64".to_string(),
                pid: 999,
            }]
        );
    }

    #[test]
    fn matches_container_full_short_and_package_identifiers() {
        let containers = vec![CliContainer {
            id: "123456789012abcdef".to_string(),
            package: "main:org.example.demo/1.0.0.0/x86_64".to_string(),
            pid: 10,
        }];
        for identifier in [
            "123456789012abcdef",
            "123456789012",
            "org.example.demo",
            "main:org.example.demo/1.0.0.0/x86_64",
        ] {
            assert_eq!(
                matching_container_ids(&containers, identifier).unwrap(),
                ["123456789012abcdef"]
            );
        }
        assert!(
            matching_container_ids(&containers, "1234")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_ambiguous_application_container_match() {
        let containers = [
            CliContainer {
                id: "first-container".to_string(),
                package: "main:org.example.demo/1.0.0.0/x86_64".to_string(),
                pid: 10,
            },
            CliContainer {
                id: "second-container".to_string(),
                package: "main:org.example.demo/1.0.0.0/x86_64".to_string(),
                pid: 11,
            },
        ];
        assert!(matching_container_ids(&containers, "org.example.demo").is_err());
    }

    #[test]
    fn formats_container_table_with_package_id_only() {
        let output = format_container_table(&[CliContainer {
            id: "123456789012".to_string(),
            package: "main:org.example.demo/1.0.0.0/x86_64".to_string(),
            pid: 42,
        }]);
        assert!(output.starts_with("\x1b[38;5;214mApp"));
        assert!(output.contains("org.example.demo"));
        assert!(!output.contains("main:org.example.demo/"));
        assert_eq!(format_container_table(&[]), "No containers are running.\n");
    }

    #[test]
    fn search_filter_keeps_latest_non_develop_package() {
        let package = |version: &str, module: &str| PackageInfoV2 {
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
            module: module.to_string(),
            name: "Demo".to_string(),
            permissions: None,
            runtime: None,
            schema_version: String::new(),
            size: 0,
            uuid: None,
            version: version.to_string(),
        };
        let mut packages = BTreeMap::from([(
            "stable".to_string(),
            vec![
                package("1.0.0.0", "binary"),
                package("2.0.0.0", "binary"),
                package("2.0.0.0", "develop"),
            ],
        )]);
        filter_search_results(
            &mut packages,
            &Search {
                keywords: "demo".to_string(),
                package_type: "all".to_string(),
                repo: None,
                dev: false,
                show_all_version: false,
            },
        );
        assert_eq!(packages["stable"].len(), 1);
        assert_eq!(packages["stable"][0].version, "2.0.0.0");
        assert_eq!(packages["stable"][0].module, "binary");
    }

    #[test]
    fn local_upgrade_candidates_keep_latest_per_id_and_channel() {
        let package = |channel: &str, version: &str| PackageInfoV2 {
            arch: vec!["x86_64".to_string()],
            base: String::new(),
            channel: channel.to_string(),
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
            schema_version: String::new(),
            size: 0,
            uuid: None,
            version: version.to_string(),
        };
        let item = |commit: &str, info: PackageInfoV2| linyaps_api::RepositoryCacheLayersItem {
            commit: commit.to_string(),
            deleted: None,
            info,
            repo: "local".to_string(),
        };
        let packages = latest_local_apps(vec![
            item("old", package("main", "1.0.0.0")),
            item("new", package("main", "2.0.0.0")),
            item("stable", package("stable", "1.5.0.0")),
        ]);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].channel, "main");
        assert_eq!(packages[0].version, "2.0.0.0");
        assert_eq!(packages[1].channel, "stable");
    }

    #[test]
    fn formats_upgradable_list_like_upstream() {
        let upgrades = [UpgradeListResult {
            id: "org.example.demo".to_string(),
            new_version: "2.0.0.0".to_string(),
            old_version: "1.0.0.0".to_string(),
        }];
        let output = format_upgrade_table(&upgrades);
        assert!(output.starts_with("\x1b[38;5;214mID"));
        assert!(output.contains("Installed"));
        assert!(output.contains("org.example.demo"));
        assert_eq!(format_upgrade_table(&[]), "No apps available for update.\n");
    }
}
