use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod app_config;
mod cache;
mod install;
mod systemd;
mod xdg;

#[derive(Debug, Parser)]
#[command(name = "ll-system-helper", about = "Linyaps system integration helper")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    XdgValue,
    XdgEnvironment,
    UserSystemdGenerator {
        normal: PathBuf,
        early: PathBuf,
        late: PathBuf,
    },
    FontCache {
        cache_root: PathBuf,
        app_id: String,
    },
    LdCache {
        cache_root: PathBuf,
        app_id: String,
        triplet: String,
    },
    AppConf {
        app_id: String,
        files: PathBuf,
    },
    Install(install::InstallOptions),
}

fn main() {
    if let Err(error) = execute() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn execute() -> Result<()> {
    let invocation = env::args_os()
        .next()
        .and_then(|path| PathBuf::from(path).file_name().map(|name| name.to_owned()))
        .and_then(|name| name.into_string().ok())
        .unwrap_or_default();
    let positional = env::args_os().skip(1).collect::<Vec<_>>();
    match invocation.as_str() {
        "61-linglong" => return xdg::print_environment(),
        "linglong-user-systemd-generator" => {
            return systemd::run_legacy(&positional);
        }
        "font-cache-generator" => return cache::run_font_legacy(&positional),
        "ld-cache-generator" => return cache::run_ld_legacy(&positional),
        "app-conf-generator" => return app_config::run_legacy(&positional),
        _ => {}
    }
    match Cli::parse().command {
        Command::XdgValue => println!("{}", xdg::data_dirs()),
        Command::XdgEnvironment => xdg::print_environment()?,
        Command::UserSystemdGenerator {
            normal,
            early,
            late,
        } => {
            let _ = normal;
            let _ = early;
            systemd::generate(&late)?;
        }
        Command::FontCache { cache_root, app_id } => {
            cache::generate_font_cache(&cache_root, &app_id)?;
        }
        Command::LdCache {
            cache_root,
            app_id,
            triplet,
        } => cache::generate_ld_cache(&cache_root, &app_id, &triplet)?,
        Command::AppConf { app_id, files } => app_config::rewrite(&app_id, &files)?,
        Command::Install(options) => install::run(options)?,
    }
    Ok(())
}
