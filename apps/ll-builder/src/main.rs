use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use linyaps_core::RepoOperation;
use linyaps_core::runtime_paths::{executable_path, oci_runtime_binary};

mod build;
mod cli11;
mod config;
mod container;
mod debian_source;
mod export;
mod frozen_help;
mod legacy;
mod localized_help;
mod postprocess;
mod project;
mod push;
mod repo_ops;
mod source;
mod uab_packaging;

#[derive(Debug, Parser)]
#[command(
    name = "ll-builder",
    about = "linyaps builder CLI \nA CLI program to build linyaps application\n",
    disable_help_subcommand = true,
    disable_version_flag = true
)]
struct Cli {
    #[arg(long)]
    version: bool,
    #[arg(long = "help-all", global = true)]
    help_all: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Create linyaps build template project")]
    Create(Create),
    #[command(about = "Build a linyaps project")]
    Build(Build),
    #[command(about = "Run built linyaps app")]
    Run(Run),
    #[command(about = "List built linyaps app")]
    List,
    #[command(about = "Remove built linyaps app")]
    Remove(Remove),
    #[command(about = "Export to linyaps layer or uab")]
    Export(Export),
    #[command(about = "Push linyaps app to remote repo")]
    Push(Push),
    #[command(about = "Import linyaps layer to build repo")]
    Import(Import),
    #[command(hide = true)]
    ImportDir(ImportDir),
    #[command(about = "Extract linyaps layer to dir")]
    Extract(Extract),
    #[command(about = "Clean build artifacts")]
    Clean(ProjectFile),
    #[command(about = "Managing remote repositories")]
    Repo(Repo),
}

#[derive(Debug, Args)]
struct Create {
    #[arg(value_name = "NAME")]
    name: String,
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
struct Build {
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    #[arg(long)]
    offline: bool,
    #[arg(long = "full-develop-module", hide = true)]
    full_develop_module: bool,
    #[arg(long = "skip-fetch-source")]
    skip_fetch_source: bool,
    #[arg(long = "skip-pull-depend")]
    skip_pull_depend: bool,
    #[arg(long = "skip-run-container")]
    skip_run_container: bool,
    #[arg(long = "skip-commit-output")]
    skip_commit_output: bool,
    #[arg(long = "skip-output-check")]
    skip_output_check: bool,
    #[arg(long = "skip-strip-symbols")]
    skip_strip_symbols: bool,
    #[arg(long = "isolate-network")]
    isolate_network: bool,
    #[arg(value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Debug, Args)]
#[command(trailing_var_arg = true)]
struct Run {
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    modules: Vec<String>,
    #[arg(long, value_name = "PATH")]
    workdir: Option<PathBuf>,
    #[arg(long)]
    debug: bool,
    #[arg(long, value_delimiter = ',', value_name = "REF")]
    extensions: Vec<String>,
    #[arg(value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct Remove {
    #[arg(long = "no-clean-objects")]
    no_clean_objects: bool,
    #[arg(value_name = "APP")]
    apps: Vec<String>,
}

#[derive(Debug, Args)]
struct Export {
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    #[arg(short = 'z', long, value_name = "X")]
    compressor: Option<String>,
    #[arg(long, value_name = "FILE", conflicts_with = "layer")]
    icon: Option<PathBuf>,
    #[arg(long)]
    layer: bool,
    #[arg(long, value_name = "FILE", conflicts_with = "layer")]
    loader: Option<PathBuf>,
    #[arg(long = "no-develop", requires = "layer")]
    no_develop: bool,
    #[arg(short = 'o', long, value_name = "FILE", conflicts_with = "layer")]
    output: Option<PathBuf>,
    #[arg(long = "ref", value_name = "REF", conflicts_with = "layer")]
    reference: Option<String>,
    #[arg(long, value_delimiter = ',', conflicts_with = "layer")]
    modules: Vec<String>,
}

#[derive(Debug, Args)]
struct Push {
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    #[arg(long = "repo-url", value_name = "URL")]
    repo_url: Option<String>,
    #[arg(long = "repo-name", value_name = "NAME")]
    repo_name: Option<String>,
    #[arg(long)]
    module: Option<String>,
}

#[derive(Debug, Args)]
struct Import {
    #[arg(value_name = "LAYER")]
    layer: PathBuf,
}

#[derive(Debug, Args)]
struct ImportDir {
    #[arg(value_name = "PATH")]
    path: PathBuf,
}

#[derive(Debug, Args)]
struct Extract {
    #[arg(value_name = "LAYER")]
    layer: PathBuf,
    #[arg(value_name = "DIR")]
    destination: PathBuf,
}

#[derive(Debug, Args)]
struct ProjectFile {
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
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
            RepoCommand::Add { name, url, alias } => Self::Add { name, url, alias },
            RepoCommand::Modify { .. } => Self::Modify,
            RepoCommand::Remove { alias } => Self::Remove { alias },
            RepoCommand::Update { alias, url } => Self::Update { alias, url },
            RepoCommand::SetDefault { alias } => Self::SetDefault { alias },
            RepoCommand::Show => Self::Show,
            RepoCommand::SetPriority { alias, priority } => Self::SetPriority { alias, priority },
            RepoCommand::EnableMirror { alias } => Self::EnableMirror { alias },
            RepoCommand::DisableMirror { alias } => Self::DisableMirror { alias },
        }
    }
}

#[tokio::main]
async fn main() {
    linyaps_core::tls::install_default_provider();
    if let Some((failure_code, report_error, result)) = legacy::dispatch().await {
        if let Err(error) = result {
            if report_error {
                eprintln!("{error:#}");
            }
            std::process::exit(failure_code);
        }
        return;
    }
    if let Err(error) = execute().await {
        eprintln!("{error:#}");
        std::process::exit(255);
    }
}

async fn execute() -> Result<()> {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if let Some(help) = frozen_help::requested(&arguments) {
        print!("{help}");
        return Ok(());
    }
    let cli = match cli11::parse(&arguments) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(error.code);
        }
    };
    if cli.version {
        println!(
            "{}{}",
            linyaps_i18n::gettext("linyaps build tool version "),
            linyaps_core::VERSION_FULL
        );
        return Ok(());
    }
    let _ = cli.help_all;
    let command = cli.command;
    let current_directory = env::current_dir()?;
    match command.as_ref() {
        Some(Command::Create(options)) => {
            let directory = project::create_project(&options.name, &current_directory)?;
            eprintln!(
                "Project {} created successfully at {}",
                options.name,
                directory.display()
            );
            return Ok(());
        }
        Some(Command::Extract(options)) => {
            repo_ops::extract_layer(&options.layer, &options.destination)?;
            eprintln!("Layer extraction completed successfully.");
            return Ok(());
        }
        _ => {}
    }

    let builder_config = config::load_builder_config()?;
    let mut repository = config::open_repository(&builder_config).await?;
    let Some(command) = command else {
        ensure_oci_runtime()?;
        let project_file = project::locate_project_file(&current_directory, None)
            .map_err(|_| anyhow::anyhow!("the project file is not found"))?;
        let _ = project::load_project(&project_file)?;
        print!("{}", frozen_help::expanded_root());
        return Ok(());
    };
    match command {
        Command::Repo(options) => {
            repo_ops::apply_repository_operation(&mut repository, options.command.into())?;
        }
        Command::Import(options) => {
            repo_ops::import_path(&mut repository, &options.layer).await?;
            eprintln!("Layer import completed successfully.");
        }
        Command::ImportDir(options) => {
            repo_ops::import_path(&mut repository, &options.path).await?;
            eprintln!("Layer directory import completed successfully.");
        }
        Command::List => repo_ops::list(&repository)?,
        Command::Remove(options) => {
            let _ = options.no_clean_objects;
            repo_ops::remove(&mut repository, &options.apps).await?;
        }
        Command::Clean(_) => {
            ensure_oci_runtime()?;
            repo_ops::clean(&current_directory)?;
            eprintln!("Clean completed successfully.");
        }
        Command::Export(options) if options.layer => {
            ensure_oci_runtime()?;
            let (_, project) = load_project(&current_directory, options.file.as_deref())?;
            let outputs = repo_ops::export_project_layers(
                &repository,
                &project,
                &current_directory,
                options.no_develop,
                options.compressor.as_deref(),
            )?;
            for output in outputs {
                eprintln!("Exported {}", output.display());
            }
        }
        Command::Build(options) => {
            ensure_oci_runtime()?;
            let (project_file, mut project) =
                load_project(&current_directory, options.file.as_deref())?;
            build::build(
                &mut repository,
                &builder_config,
                &mut project,
                &project_file,
                &current_directory,
                build::BuildOptions {
                    command: options.command,
                    offline: options.offline,
                    full_develop_module: options.full_develop_module,
                    skip_fetch_source: options.skip_fetch_source,
                    skip_pull_depend: options.skip_pull_depend,
                    skip_run_container: options.skip_run_container,
                    skip_commit_output: options.skip_commit_output,
                    skip_output_check: options.skip_output_check,
                    skip_strip_symbols: options.skip_strip_symbols,
                    isolate_network: options.isolate_network,
                },
            )
            .await?;
            eprintln!("Build completed successfully.");
        }
        Command::Run(options) => {
            ensure_oci_runtime()?;
            let (_, mut project) = load_project(&current_directory, options.file.as_deref())?;
            let code = build::run_built(
                &mut repository,
                &mut project,
                &current_directory,
                build::RunOptions {
                    command: options.command,
                    modules: options.modules,
                    debug: options.debug,
                    workdir: options.workdir,
                    extensions: options.extensions,
                },
            )
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            eprintln!("Run completed successfully.");
        }
        Command::Export(options) => {
            ensure_oci_runtime()?;
            let project = if options.reference.is_none() || options.file.is_some() {
                Some(load_project(&current_directory, options.file.as_deref())?.1)
            } else {
                project::locate_project_file(&current_directory, None)
                    .ok()
                    .map(|path| project::load_project(&path))
                    .transpose()?
            };
            let output = export::export_uab(
                &mut repository,
                project.as_ref(),
                &current_directory,
                export::ExportOptions {
                    compressor: options.compressor,
                    header: None,
                    icon: options.icon,
                    loader: options.loader,
                    output: options.output,
                    reference: options.reference,
                    modules: options.modules,
                },
            )
            .await?;
            eprintln!("Exported {}", output.display());
        }
        Command::Push(options) => {
            ensure_oci_runtime()?;
            let (_, project) = load_project(&current_directory, options.file.as_deref())?;
            push::push(
                &repository,
                &project,
                &current_directory,
                options.module,
                options.repo_url,
                options.repo_name,
            )
            .await?;
        }
        Command::Create(_) | Command::Extract(_) => unreachable!(),
    }
    Ok(())
}

fn ensure_oci_runtime() -> Result<()> {
    let runtime = oci_runtime_binary();
    if executable_path(&runtime, &[]).is_none() {
        anyhow::bail!("{} not found", runtime.to_string_lossy());
    }
    Ok(())
}

fn load_project(
    current_directory: &Path,
    requested: Option<&Path>,
) -> Result<(PathBuf, linyaps_api::BuilderProject)> {
    let path = project::locate_project_file(current_directory, requested)?;
    let project = project::load_project(&path)?;
    Ok((path, project))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_all_flattens_visible_commands_without_help_subcommands() {
        assert!(Cli::try_parse_from(["ll-builder", "help"]).is_err());
        assert!(Cli::try_parse_from(["ll-builder", "repo", "help"]).is_err());

        let help = include_str!("../help/help-all.txt");
        assert!(help.contains("Build a linyaps project"));
        assert!(help.contains("--skip-run-container"));
        assert!(!help.contains("Usage: build"));
        assert!(!help.contains("Print this message or the help of the given subcommand"));

        let build = include_str!("../help/build.txt");
        assert!(build.contains("Usage: ll-builder build"));
        assert!(!build.contains("Usage: ll-builder run"));
    }
}
