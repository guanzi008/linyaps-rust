use std::ffi::OsString;
use std::path::Path;

use super::{
    Analyze, AnalyzeCommand, App, Cli, Command, DeviceMode, Enter, Inspect, InspectCommand,
    Install, Kill, List, Ps, Repo, RepoCommand, Run, Search, SortField, Uninstall, Upgrade,
    default_cdi_directories,
};

const MORE_INFORMATION: &str = "Run with --help or --help-all for more information.";

#[derive(Debug)]
pub struct ParseError {
    pub code: i32,
    message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}\n{}", self.message, MORE_INFORMATION)
    }
}

pub fn parse(arguments: &[OsString]) -> Result<Cli, ParseError> {
    let arguments = arguments
        .iter()
        .map(|argument| {
            let value = argument.to_string_lossy().into_owned();
            if value == "--exec" {
                "--".to_string()
            } else {
                value
            }
        })
        .collect::<Vec<_>>();
    let mut cli = Cli {
        help_all: false,
        version: false,
        json: false,
        no_dbus: false,
        verbose: 0,
        no_progress: false,
        command: None,
    };
    let mut index = 0;
    let mut root_separator = false;
    let mut commands = Vec::new();
    while index < arguments.len() {
        if arguments[index] == "--" {
            root_separator = true;
            index += 1;
            continue;
        }
        if !root_separator && let Some(consumed) = parse_global(&arguments, index, &mut cli)? {
            index += consumed;
            continue;
        }
        if !is_top_command(&arguments[index]) {
            if root_separator {
                let mut values = arguments[index..].to_vec();
                values.push("--".to_string());
                return Err(unexpected_many(&values));
            }
            return Err(unexpected(&arguments[index]));
        }
        let end = command_end(&arguments, index);
        let filtered_arguments;
        let command_arguments = if root_separator {
            filtered_arguments = arguments[index + 1..end]
                .iter()
                .filter(|argument| !matches!(argument.as_str(), "-h" | "--help" | "--help-all"))
                .cloned()
                .collect::<Vec<_>>();
            filtered_arguments.as_slice()
        } else {
            &arguments[index + 1..end]
        };
        let command = match arguments[index].as_str() {
            "run" => parse_run(command_arguments, &mut cli)?,
            "ps" => parse_ps(command_arguments, &mut cli)?,
            "enter" => parse_enter(command_arguments, &mut cli)?,
            "kill" => parse_kill(command_arguments, &mut cli)?,
            "install" => parse_install(command_arguments, &mut cli)?,
            "uninstall" => parse_uninstall(command_arguments, &mut cli)?,
            "upgrade" => parse_upgrade(command_arguments, &mut cli)?,
            "search" => parse_search(command_arguments, &mut cli)?,
            "list" => parse_list(command_arguments, &mut cli)?,
            "analyze" => parse_analyze(command_arguments, &mut cli)?,
            "repo" => parse_repo(command_arguments)?,
            "info" => Command::Info(App {
                app: one_required(command_arguments, "APP")?,
            }),
            "content" => Command::Content(App {
                app: one_required(command_arguments, "APP")?,
            }),
            "prune" => {
                no_positionals(command_arguments)?;
                Command::Prune
            }
            "inspect" => parse_inspect(command_arguments)?,
            _ => unreachable!(),
        };
        commands.push(command);
        index = end;
    }
    cli.command = commands.into_iter().min_by_key(command_priority);
    Ok(cli)
}

fn command_end(arguments: &[String], command_index: usize) -> usize {
    let command = arguments[command_index].as_str();
    let mut required_positionals = match command {
        "ps" | "list" | "prune" => 0,
        "run" | "enter" | "kill" | "install" | "uninstall" | "upgrade" | "search" | "info"
        | "content" => 1,
        "analyze" | "repo" | "inspect" => 0,
        _ => return arguments.len(),
    };
    let mut index = command_index + 1;
    if matches!(command, "analyze" | "repo" | "inspect") {
        let Some(subcommand) = arguments.get(index) else {
            return arguments.len();
        };
        required_positionals = match (command, subcommand.as_str()) {
            ("analyze", "size") => 0,
            ("analyze", "depends") => 1,
            ("repo", "add" | "update" | "set-priority") => 2,
            ("repo", "modify" | "remove" | "set-default" | "enable-mirror" | "disable-mirror")
            | ("inspect", "dir") => 1,
            ("repo", "show") => 0,
            _ => return arguments.len(),
        };
        index += 1;
    }
    let mut positionals = 0;
    let mut separated = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if !separated && token == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if !separated && option_takes_value(command, token) {
            if !token.contains('=') && index + 1 < arguments.len() {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if !separated && token.starts_with('-') {
            index += 1;
            continue;
        }
        if !separated && is_top_command(token) && positionals >= required_positionals {
            return index;
        }
        positionals += 1;
        index += 1;
    }
    arguments.len()
}

fn option_takes_value(command: &str, token: &str) -> bool {
    let (name, _) = split_long(token);
    match command {
        "run" => matches!(
            name,
            "--env"
                | "--base"
                | "--runtime"
                | "--workdir"
                | "--extensions"
                | "--run-context"
                | "--caps-add"
                | "--cdi-spec-dir"
                | "--device"
                | "--device-mode"
                | "--instance"
                | "--debug-listen"
                | "--debug-debuginfod"
                | "--debug-symbol-dir"
        ),
        "enter" => name == "--working-directory",
        "kill" => name == "--signal" || token == "-s" || token.starts_with("-s"),
        "install" => matches!(name, "--module" | "--repo"),
        "uninstall" => name == "--module",
        "search" => matches!(name, "--type" | "--repo"),
        "analyze" => name == "--sort",
        "repo" => matches!(name, "--alias" | "--name"),
        "inspect" => matches!(name, "--type" | "--module") || matches!(token, "-t" | "-m"),
        _ => false,
    }
}

fn command_priority(command: &Command) -> usize {
    match command {
        Command::Run(_) => 0,
        Command::Ps(_) => 1,
        Command::Enter(_) => 2,
        Command::Kill(_) => 3,
        Command::Install(_) => 4,
        Command::Uninstall(_) => 5,
        Command::Upgrade(_) => 6,
        Command::Search(_) => 7,
        Command::List(_) => 8,
        Command::Analyze(_) => 9,
        Command::Repo(_) => 10,
        Command::Info(_) => 11,
        Command::Content(_) => 12,
        Command::Prune => 13,
        Command::Inspect(_) => 14,
    }
}

fn parse_global(
    arguments: &[String],
    index: usize,
    cli: &mut Cli,
) -> Result<Option<usize>, ParseError> {
    let token = &arguments[index];
    let (name, attached) = split_long(token);
    match name {
        "--version" => {
            cli.version = true;
            return Ok(Some(1));
        }
        "--json" => {
            cli.json = true;
            return Ok(Some(1));
        }
        "--no-dbus" => {
            cli.no_dbus = true;
            return Ok(Some(1));
        }
        "--no-progress" => {
            cli.no_progress = parse_bool_flag(name, attached)?;
            return Ok(Some(1));
        }
        "--verbose" => {
            cli.verbose = match attached {
                Some(value) => value.parse::<u8>().map_err(|_| conversion(name, value))?,
                None => cli.verbose.saturating_add(1),
            };
            return Ok(Some(1));
        }
        _ => {}
    }
    if token.starts_with('-') && !token.starts_with("--") && token.len() > 1 {
        if !token[1..].starts_with('v') {
            return Ok(None);
        }
        for (offset, character) in token[1..].char_indices() {
            if character == 'v' {
                cli.verbose = cli.verbose.saturating_add(1);
            } else {
                return Err(unexpected(&format!("-{}", &token[offset + 1..])));
            }
        }
        return Ok(Some(1));
    }
    Ok(None)
}

fn parse_run(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut run = Run {
        app: String::new(),
        files: Vec::new(),
        urls: Vec::new(),
        environment: Vec::new(),
        base: None,
        runtime: None,
        workdir: None,
        extensions: Vec::new(),
        enable_xdp: false,
        disable_xdp: false,
        enable_pipewire: None,
        enable_atspi: None,
        run_context: None,
        privileged: false,
        caps_add: Vec::new(),
        cdi_spec_dir: default_cdi_directories(),
        device: Vec::new(),
        device_mode: Vec::new(),
        instance: None,
        debug: false,
        debug_listen: "127.0.0.1:2345".to_string(),
        debug_debuginfod: None,
        debug_symbol_dir: None,
        command: Vec::new(),
    };
    let mut index = 0;
    let mut separated = false;
    let mut debug_listen_set = false;
    let mut debug_debuginfod_set = false;
    let mut debug_symbol_dir_set = false;
    let mut single_options = std::collections::BTreeSet::new();
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if !separated {
            if let Some(consumed) = parse_global(arguments, index, cli)? {
                index += consumed;
                continue;
            }
            let token = &arguments[index];
            let (name, attached) = split_long(token);
            let value_option = match name {
                "--env" => Some(("ENV", ValueTarget::Environment)),
                "--base" => Some(("REF", ValueTarget::Base)),
                "--runtime" => Some(("REF", ValueTarget::Runtime)),
                "--workdir" => Some(("PATH", ValueTarget::Workdir)),
                "--extensions" => Some(("REF", ValueTarget::Extensions)),
                "--run-context" => Some(("TEXT", ValueTarget::RunContext)),
                "--caps-add" => Some(("TEXT", ValueTarget::Caps)),
                "--cdi-spec-dir" => Some(("TEXT", ValueTarget::CdiSpec)),
                "--device" => Some(("TEXT", ValueTarget::Device)),
                "--device-mode" => Some(("ENUM", ValueTarget::DeviceMode)),
                "--instance" => Some(("NAME", ValueTarget::Instance)),
                "--debug-listen" => Some(("ADDR", ValueTarget::DebugListen)),
                "--debug-debuginfod" => Some(("URLS", ValueTarget::DebugDebuginfod)),
                "--debug-symbol-dir" => Some(("DIR", ValueTarget::DebugSymbolDir)),
                _ => None,
            };
            if let Some((type_name, target)) = value_option {
                let (value, consumed) =
                    required_option_value(arguments, index, name, type_name, attached)?;
                if matches!(
                    target,
                    ValueTarget::Base
                        | ValueTarget::Runtime
                        | ValueTarget::Workdir
                        | ValueTarget::RunContext
                        | ValueTarget::Instance
                        | ValueTarget::DebugListen
                        | ValueTarget::DebugDebuginfod
                        | ValueTarget::DebugSymbolDir
                ) && !single_options.insert(name)
                {
                    return Err(at_most_one(name));
                }
                match target {
                    ValueTarget::Environment => {
                        if !value.contains('=') {
                            return Err(validation(
                                name,
                                linyaps_i18n::gettext(
                                    "Input parameter is invalid, please input valid parameter instead",
                                ),
                            ));
                        }
                        run.environment.push(value);
                    }
                    ValueTarget::Base => run.base = Some(valid_string(name, value)?),
                    ValueTarget::Runtime => run.runtime = Some(valid_string(name, value)?),
                    ValueTarget::Workdir => run.workdir = Some(value),
                    ValueTarget::Extensions => {
                        run.extensions.extend(split_nonempty(name, value)?);
                    }
                    ValueTarget::RunContext => run.run_context = Some(value),
                    ValueTarget::Caps => run.caps_add.extend(split_nonempty(name, value)?),
                    ValueTarget::CdiSpec => {
                        run.cdi_spec_dir.extend(split_nonempty(name, value)?);
                    }
                    ValueTarget::Device => run.device.extend(split_nonempty(name, value)?),
                    ValueTarget::DeviceMode => {
                        for value in split_nonempty(name, value)? {
                            if value.eq_ignore_ascii_case("passthru") || value == "0" {
                                run.device_mode.push(DeviceMode::Passthru);
                            } else {
                                return Err(validation(
                                    name,
                                    format!(
                                        "Check {value} value in {{passthru->0}} OR {{0}} FAILED"
                                    ),
                                ));
                            }
                        }
                    }
                    ValueTarget::Instance => run.instance = Some(valid_string(name, value)?),
                    ValueTarget::DebugListen => {
                        run.debug_listen = valid_string(name, value)?;
                        debug_listen_set = true;
                    }
                    ValueTarget::DebugDebuginfod => {
                        run.debug_debuginfod = Some(valid_string(name, value)?);
                        debug_debuginfod_set = true;
                    }
                    ValueTarget::DebugSymbolDir => {
                        run.debug_symbol_dir = Some(valid_string(name, value)?);
                        debug_symbol_dir_set = true;
                    }
                }
                index += consumed;
                continue;
            }
            match name {
                "--file" | "--url" => {
                    let values =
                        optional_vector_values(arguments, index, attached, run.app.is_empty());
                    let consumed = values.1;
                    if name == "--file" {
                        run.files.extend(values.0);
                    } else {
                        run.urls.extend(values.0);
                    }
                    index += consumed;
                    continue;
                }
                "--enable-xdp" => {
                    let value = parse_bool_flag(name, attached)?;
                    run.enable_xdp = value;
                    run.disable_xdp = !value;
                }
                "--disable-xdp" => {
                    let value = parse_bool_flag("--enable-xdp", attached)?;
                    run.disable_xdp = value;
                    run.enable_xdp = !value;
                }
                "--enable-pipewire" => {
                    run.enable_pipewire = Some(parse_bool_flag(name, attached)?);
                }
                "--enable-atspi" => {
                    run.enable_atspi = Some(parse_bool_flag(name, attached)?);
                }
                "--privileged" => {
                    run.privileged = parse_bool_flag("", attached)?;
                }
                "--debug" => run.debug = parse_bool_flag(name, attached)?,
                _ if token.starts_with('-') => return Err(unexpected_option(token)),
                _ => {
                    if run.app.is_empty() {
                        run.app = valid_positional("APP", token.clone())?;
                    } else {
                        run.command.push(token.clone());
                    }
                    index += 1;
                    continue;
                }
            }
            index += 1;
            continue;
        }
        if run.app.is_empty() {
            run.app = valid_positional("APP", arguments[index].clone())?;
        } else {
            run.command.push(arguments[index].clone());
        }
        index += 1;
    }
    if debug_listen_set && !run.debug {
        return Err(requires("--debug-listen", "--debug"));
    }
    if debug_debuginfod_set && !run.debug {
        return Err(requires("--debug-debuginfod", "--debug"));
    }
    if debug_symbol_dir_set && !run.debug {
        return Err(requires("--debug-symbol-dir", "--debug"));
    }
    require_positional(&run.app, "APP")?;
    Ok(Command::Run(run))
}

#[derive(Clone, Copy)]
enum ValueTarget {
    Environment,
    Base,
    Runtime,
    Workdir,
    Extensions,
    RunContext,
    Caps,
    CdiSpec,
    Device,
    DeviceMode,
    Instance,
    DebugListen,
    DebugDebuginfod,
    DebugSymbolDir,
}

fn parse_ps(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut no_truncated = false;
    parse_flag_only(arguments, cli, |name, attached| match name {
        "--no-truncated" => {
            no_truncated = parse_bool_flag(name, attached)?;
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(Command::Ps(Ps { no_truncated }))
}

fn parse_enter(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut instance = String::new();
    let mut working_directory = None;
    let mut command = Vec::new();
    let mut index = 0;
    let mut separated = false;
    let mut working_directory_set = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if !separated {
            if let Some(consumed) = parse_global(arguments, index, cli)? {
                index += consumed;
                continue;
            }
            let (name, attached) = split_long(&arguments[index]);
            if name == "--working-directory" {
                let (value, consumed) =
                    required_option_value(arguments, index, name, "PATH:DIR", attached)?;
                if !Path::new(&value).is_dir() {
                    return Err(validation(
                        name,
                        format!("Directory does not exist: {value}"),
                    ));
                }
                if working_directory_set {
                    return Err(at_most_one(name));
                }
                working_directory_set = true;
                working_directory = Some(value);
                index += consumed;
                continue;
            }
            if arguments[index].starts_with('-') {
                return Err(unexpected_option(&arguments[index]));
            }
        }
        if instance.is_empty() {
            instance = valid_positional("INSTANCE", arguments[index].clone())?;
        } else {
            command.push(arguments[index].clone());
        }
        index += 1;
    }
    require_positional(&instance, "INSTANCE")?;
    Ok(Command::Enter(Enter {
        instance,
        working_directory,
        command,
    }))
}

fn parse_kill(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut signal = "SIGTERM".to_string();
    let mut app = String::new();
    let mut index = 0;
    let mut separated = false;
    let mut signal_set = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if !separated && let Some(consumed) = parse_global(arguments, index, cli)? {
            index += consumed;
            continue;
        }
        let token = &arguments[index];
        let (name, attached) = split_long(token);
        if !separated && (name == "--signal" || token == "-s" || token.starts_with("-s")) {
            let short_attached = token.strip_prefix("-s").filter(|value| !value.is_empty());
            let (value, consumed) = required_option_value(
                arguments,
                index,
                if name == "--signal" { name } else { "--signal" },
                "TEXT",
                attached.or(short_attached),
            )?;
            if signal_set {
                return Err(at_most_one("--signal"));
            }
            signal_set = true;
            signal = value;
            index += consumed;
            continue;
        }
        if !separated && token.starts_with('-') {
            return Err(unexpected_option(token));
        }
        if app.is_empty() {
            app = valid_positional("APP", token.clone())?;
        } else {
            return Err(unexpected(token));
        }
        index += 1;
    }
    require_positional(&app, "APP")?;
    Ok(Command::Kill(Kill { signal, app }))
}

fn parse_install(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut install = Install {
        app: String::new(),
        module: None,
        repo: None,
        force: false,
        confirm: false,
        no_auto_prune: false,
    };
    let mut index = 0;
    let mut separated = false;
    let mut module_set = false;
    let mut repo_set = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        let token = &arguments[index];
        if !separated
            && token.starts_with('-')
            && !token.starts_with("--")
            && token[1..]
                .chars()
                .all(|character| matches!(character, 'v' | 'y'))
        {
            for character in token[1..].chars() {
                if character == 'y' {
                    install.confirm = true;
                } else {
                    cli.verbose = cli.verbose.saturating_add(1);
                }
            }
            index += 1;
            continue;
        }
        if !separated && let Some(consumed) = parse_global(arguments, index, cli)? {
            index += consumed;
            continue;
        }
        let (name, attached) = split_long(token);
        if !separated && matches!(name, "--module" | "--repo") {
            let type_name = if name == "--module" { "MODULE" } else { "REPO" };
            let (value, consumed) =
                required_option_value(arguments, index, name, type_name, attached)?;
            let value = valid_string(name, value)?;
            if name == "--module" {
                if module_set {
                    return Err(at_most_one(name));
                }
                module_set = true;
                install.module = Some(value);
            } else {
                if repo_set {
                    return Err(at_most_one(name));
                }
                repo_set = true;
                install.repo = Some(value);
            }
            index += consumed;
            continue;
        }
        match name {
            _ if separated && install.app.is_empty() => {
                install.app = valid_positional("APP", token.clone())?;
            }
            _ if separated => return Err(unexpected(token)),
            "--force" => install.force = parse_bool_flag(name, attached)?,
            "--no-auto-prune" => install.no_auto_prune = parse_bool_flag(name, attached)?,
            _ if token.starts_with('-') && !token.starts_with("--") => {
                for character in token[1..].chars() {
                    if character == 'y' {
                        install.confirm = true;
                    } else if character == 'v' {
                        cli.verbose = cli.verbose.saturating_add(1);
                    } else {
                        return Err(unexpected_option(&format!("-{character}")));
                    }
                }
            }
            _ if token.starts_with('-') => return Err(unexpected_option(token)),
            _ if install.app.is_empty() => {
                install.app = valid_positional("APP", token.clone())?;
            }
            _ => return Err(unexpected(token)),
        }
        index += 1;
    }
    require_positional(&install.app, "APP")?;
    Ok(Command::Install(install))
}

fn parse_uninstall(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut value = Uninstall {
        app: String::new(),
        module: None,
        force: false,
        no_auto_prune: false,
        prune: false,
        all: false,
    };
    let mut index = 0;
    let mut separated = false;
    let mut module_set = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if !separated && let Some(consumed) = parse_global(arguments, index, cli)? {
            index += consumed;
            continue;
        }
        let token = &arguments[index];
        let (name, attached) = split_long(token);
        if !separated && name == "--module" {
            let (module, consumed) =
                required_option_value(arguments, index, name, "MODULE", attached)?;
            if module_set {
                return Err(at_most_one(name));
            }
            module_set = true;
            value.module = Some(valid_string(name, module)?);
            index += consumed;
            continue;
        }
        match name {
            _ if separated && value.app.is_empty() => {
                value.app = valid_positional("APP", token.clone())?;
            }
            _ if separated => return Err(unexpected(token)),
            "--force" => value.force = parse_bool_flag(name, attached)?,
            "--no-auto-prune" => value.no_auto_prune = parse_bool_flag(name, attached)?,
            "--prune" => value.prune = true,
            "--all" => value.all = true,
            _ if token.starts_with('-') => return Err(unexpected_option(token)),
            _ if value.app.is_empty() => value.app = valid_positional("APP", token.clone())?,
            _ => return Err(unexpected(token)),
        }
        index += 1;
    }
    require_positional(&value.app, "APP")?;
    Ok(Command::Uninstall(value))
}

fn parse_upgrade(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut value = Upgrade {
        app: None,
        deps_only: false,
        no_auto_prune: false,
    };
    parse_options_and_optional_positional(
        arguments,
        cli,
        "APP",
        |token, attached| match token {
            "--deps-only" => {
                value.deps_only = parse_bool_flag(token, attached)?;
                Ok(true)
            }
            "--no-auto-prune" => {
                value.no_auto_prune = parse_bool_flag(token, attached)?;
                Ok(true)
            }
            _ => Ok(false),
        },
        &mut value.app,
    )?;
    Ok(Command::Upgrade(value))
}

fn parse_search(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut value = Search {
        keywords: String::new(),
        package_type: "all".to_string(),
        repo: None,
        dev: false,
        show_all_version: false,
    };
    let mut index = 0;
    let mut separated = false;
    let mut type_set = false;
    let mut repo_set = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if !separated && let Some(consumed) = parse_global(arguments, index, cli)? {
            index += consumed;
            continue;
        }
        let token = &arguments[index];
        let (name, attached) = split_long(token);
        if !separated && matches!(name, "--type" | "--repo") {
            let type_name = if name == "--type" { "TYPE" } else { "REPO" };
            let (option, consumed) =
                required_option_value(arguments, index, name, type_name, attached)?;
            let option = valid_string(name, option)?;
            if name == "--type" {
                if type_set {
                    return Err(at_most_one(name));
                }
                type_set = true;
                value.package_type = option;
            } else {
                if repo_set {
                    return Err(at_most_one(name));
                }
                repo_set = true;
                value.repo = Some(option);
            }
            index += consumed;
            continue;
        }
        match name {
            _ if separated && value.keywords.is_empty() => {
                value.keywords = valid_positional("KEYWORDS", token.clone())?;
            }
            _ if separated => return Err(unexpected(token)),
            "--dev" => value.dev = parse_bool_flag(name, attached)?,
            "--show-all-version" => value.show_all_version = parse_bool_flag(name, attached)?,
            _ if token.starts_with('-') => return Err(unexpected_option(token)),
            _ if value.keywords.is_empty() => {
                value.keywords = valid_positional("KEYWORDS", token.clone())?
            }
            _ => return Err(unexpected(token)),
        }
        index += 1;
    }
    require_positional(&value.keywords, "KEYWORDS")?;
    Ok(Command::Search(value))
}

fn parse_list(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let mut value = List {
        package_type: "all".to_string(),
        upgradable: false,
    };
    let mut index = 0;
    let mut type_set = false;
    while index < arguments.len() {
        if arguments[index] == "--" {
            index += 1;
            if index < arguments.len() {
                return Err(unexpected(&arguments[index]));
            }
            continue;
        }
        if let Some(consumed) = parse_global(arguments, index, cli)? {
            index += consumed;
            continue;
        }
        let token = &arguments[index];
        let (name, attached) = split_long(token);
        if name == "--type" {
            let (option, consumed) =
                required_option_value(arguments, index, name, "TYPE", attached)?;
            let option = valid_string(name, option)?;
            if type_set {
                return Err(at_most_one(name));
            }
            type_set = true;
            value.package_type = option;
            index += consumed;
            continue;
        }
        match name {
            "--upgradable" => value.upgradable = parse_bool_flag(name, attached)?,
            _ if is_top_command(token) => {
                index += 1;
                while index < arguments.len() && !is_top_command(&arguments[index]) {
                    index += 1;
                }
                continue;
            }
            _ if token.starts_with('-') => return Err(unexpected_option(token)),
            _ => return Err(unexpected(token)),
        }
        index += 1;
    }
    Ok(Command::List(value))
}

fn parse_analyze(arguments: &[String], cli: &mut Cli) -> Result<Command, ParseError> {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return Err(subcommand_required());
    };
    let command = match subcommand.as_str() {
        "size" => {
            let mut sort = SortField::Actual;
            let mut asc = false;
            let mut index = 0;
            let mut sort_set = false;
            while index < rest.len() {
                if rest[index] == "--" {
                    index += 1;
                    if index < rest.len() {
                        return Err(unexpected(&rest[index]));
                    }
                    continue;
                }
                if let Some(consumed) = parse_global(rest, index, cli)? {
                    index += consumed;
                    continue;
                }
                let token = &rest[index];
                let (name, attached) = split_long(token);
                if name == "--sort" {
                    let (value, consumed) =
                        required_option_value(rest, index, name, "FIELD", attached)?;
                    if sort_set {
                        return Err(at_most_one(name));
                    }
                    sort_set = true;
                    sort = match value.as_str() {
                        "actual" => SortField::Actual,
                        "logical" => SortField::Logical,
                        "exclusive" => SortField::Exclusive,
                        "shared" => SortField::Shared,
                        "id" => SortField::Id,
                        _ => {
                            return Err(validation(
                                name,
                                format!("{value} not in {{actual,logical,exclusive,shared,id}}"),
                            ));
                        }
                    };
                    index += consumed;
                    continue;
                }
                if name == "--asc" {
                    asc = parse_bool_flag(name, attached)?;
                } else if token.starts_with('-') {
                    return Err(unexpected_option(token));
                } else {
                    return Err(unexpected(token));
                }
                index += 1;
            }
            AnalyzeCommand::Size { sort, asc }
        }
        "depends" => {
            let app = optional_one(rest)?;
            AnalyzeCommand::Depends { app }
        }
        _ => return Err(subcommand_required()),
    };
    Ok(Command::Analyze(Analyze { command }))
}

fn parse_repo(arguments: &[String]) -> Result<Command, ParseError> {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return Err(subcommand_required());
    };
    let command = match subcommand.as_str() {
        "add" => {
            let (positionals, options) = repo_values(rest, &["--alias"])?;
            let name = required_at(&positionals, 0, "NAME")?;
            let url = required_at(&positionals, 1, "URL")?;
            reject_extra(&positionals, 2)?;
            RepoCommand::Add {
                name,
                url,
                alias: options.get("--alias").cloned(),
            }
        }
        "modify" => {
            let (positionals, options) = repo_values(rest, &["--name"])?;
            let url = required_at(&positionals, 0, "URL")?;
            reject_extra(&positionals, 1)?;
            RepoCommand::Modify {
                url,
                name: options.get("--name").cloned(),
            }
        }
        "remove" => RepoCommand::Remove {
            alias: exact_required(rest, "ALIAS")?,
        },
        "update" => {
            let values = exact_positionals(rest)?;
            let alias = required_at(&values, 0, "ALIAS")?;
            let url = required_at(&values, 1, "URL")?;
            reject_extra(&values, 2)?;
            RepoCommand::Update { alias, url }
        }
        "set-default" => RepoCommand::SetDefault {
            alias: exact_required(rest, "ALIAS")?,
        },
        "show" => {
            no_positionals(rest)?;
            RepoCommand::Show
        }
        "set-priority" => {
            let values = priority_positionals(rest)?;
            let alias = required_at(&values, 0, "ALIAS")?;
            let raw = required_at(&values, 1, "PRIORITY")?;
            reject_extra(&values, 2)?;
            let priority = raw
                .parse::<i64>()
                .map_err(|_| conversion("PRIORITY", &raw))?;
            RepoCommand::SetPriority { alias, priority }
        }
        "enable-mirror" => RepoCommand::EnableMirror {
            alias: exact_required(rest, "ALIAS")?,
        },
        "disable-mirror" => RepoCommand::DisableMirror {
            alias: exact_required(rest, "ALIAS")?,
        },
        _ => return Err(subcommand_required()),
    };
    Ok(Command::Repo(Repo { command }))
}

fn parse_inspect(arguments: &[String]) -> Result<Command, ParseError> {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return Err(subcommand_required());
    };
    if subcommand != "dir" {
        return Err(subcommand_required());
    }
    let mut app = String::new();
    let mut directory_type = "layer".to_string();
    let mut module = None;
    let mut index = 0;
    let mut separated = false;
    let mut type_set = false;
    let mut module_set = false;
    while index < rest.len() {
        if !separated && rest[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        let token = &rest[index];
        let (long, attached) = split_long(token);
        let (name, type_name, short_attached) = if long == "--type" || token == "-t" {
            ("--type", "TYPE", None)
        } else if long == "--module" || token == "-m" {
            ("--module", "TEXT", None)
        } else if let Some(value) = token.strip_prefix("-t=") {
            ("--type", "TYPE", Some(value))
        } else if let Some(value) = token.strip_prefix("-m=") {
            ("--module", "TEXT", Some(value))
        } else {
            ("", "", None)
        };
        if !separated && !name.is_empty() {
            let (value, consumed) =
                required_option_value(rest, index, name, type_name, attached.or(short_attached))?;
            let value = valid_string(name, value)?;
            if name == "--type" {
                if type_set {
                    return Err(at_most_one(name));
                }
                type_set = true;
                directory_type = value;
            } else {
                if module_set {
                    return Err(at_most_one(name));
                }
                module_set = true;
                module = Some(value);
            }
            index += consumed;
            continue;
        }
        if !separated && token.starts_with('-') {
            return Err(unexpected_option(token));
        }
        if app.is_empty() {
            app = valid_positional("APP", token.clone())?;
        } else {
            return Err(unexpected(token));
        }
        index += 1;
    }
    require_positional(&app, "APP")?;
    Ok(Command::Inspect(Inspect {
        command: InspectCommand::Dir {
            app,
            directory_type,
            module,
        },
    }))
}

fn parse_flag_only<F>(arguments: &[String], cli: &mut Cli, mut option: F) -> Result<(), ParseError>
where
    F: FnMut(&str, Option<&str>) -> Result<bool, ParseError>,
{
    let mut index = 0;
    let mut separated = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if separated {
            return Err(unexpected(&arguments[index]));
        }
        if let Some(consumed) = parse_global(arguments, index, cli)? {
            index += consumed;
            continue;
        }
        let (name, attached) = split_long(&arguments[index]);
        if option(name, attached)? {
            index += 1;
        } else {
            return Err(if arguments[index].starts_with('-') {
                unexpected_option(&arguments[index])
            } else {
                unexpected(&arguments[index])
            });
        }
    }
    Ok(())
}

fn parse_options_and_optional_positional<F>(
    arguments: &[String],
    cli: &mut Cli,
    label: &str,
    mut option: F,
    positional: &mut Option<String>,
) -> Result<(), ParseError>
where
    F: FnMut(&str, Option<&str>) -> Result<bool, ParseError>,
{
    let mut index = 0;
    let mut separated = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if !separated && let Some(consumed) = parse_global(arguments, index, cli)? {
            index += consumed;
            continue;
        }
        let token = &arguments[index];
        let (name, attached) = split_long(token);
        if !separated && option(name, attached)? {
            index += 1;
        } else if !separated && token.starts_with('-') {
            return Err(unexpected_option(token));
        } else if positional.is_none() {
            *positional = Some(valid_positional(label, token.clone())?);
            index += 1;
        } else {
            return Err(unexpected(token));
        }
    }
    Ok(())
}

fn one_required(arguments: &[String], label: &str) -> Result<String, ParseError> {
    let values = exact_positionals(arguments)?;
    let value = required_at(&values, 0, label)?;
    reject_extra(&values, 1)?;
    Ok(value)
}

fn optional_one(arguments: &[String]) -> Result<Option<String>, ParseError> {
    let values = exact_positionals(arguments)?;
    reject_extra(&values, 1)?;
    values
        .first()
        .cloned()
        .map(|value| valid_positional("APP", value))
        .transpose()
}

fn no_positionals(arguments: &[String]) -> Result<(), ParseError> {
    let values = exact_positionals(arguments)?;
    reject_extra(&values, 0)
}

fn exact_required(arguments: &[String], label: &str) -> Result<String, ParseError> {
    let values = exact_positionals(arguments)?;
    let value = required_at(&values, 0, label)?;
    reject_extra(&values, 1)?;
    Ok(value)
}

fn exact_positionals(arguments: &[String]) -> Result<Vec<String>, ParseError> {
    let mut values = Vec::new();
    let mut separated = false;
    for value in arguments {
        if !separated && value == "--" {
            separated = true;
            continue;
        }
        if !separated && value.starts_with('-') {
            return Err(unexpected_option(value));
        }
        values.push(value.clone());
    }
    Ok(values)
}

fn priority_positionals(arguments: &[String]) -> Result<Vec<String>, ParseError> {
    let mut values = Vec::new();
    let mut separated = false;
    for value in arguments {
        if !separated && value == "--" {
            separated = true;
        } else if !separated && value.starts_with('-') && values.len() != 1 {
            return Err(unexpected_option(value));
        } else {
            values.push(value.clone());
        }
    }
    Ok(values)
}

fn repo_values(
    arguments: &[String],
    option_names: &[&str],
) -> Result<(Vec<String>, std::collections::BTreeMap<String, String>), ParseError> {
    let mut positionals = Vec::new();
    let mut options = std::collections::BTreeMap::new();
    let mut index = 0;
    let mut separated = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        let (name, attached) = split_long(&arguments[index]);
        if !separated && option_names.contains(&name) {
            let type_name = if name == "--alias" { "ALIAS" } else { "REPO" };
            let (value, consumed) =
                required_option_value(arguments, index, name, type_name, attached)?;
            let value = valid_string(name, value)?;
            if options.insert(name.to_string(), value).is_some() {
                return Err(at_most_one(name));
            }
            index += consumed;
        } else if !separated && arguments[index].starts_with('-') {
            return Err(unexpected_option(&arguments[index]));
        } else {
            positionals.push(arguments[index].clone());
            index += 1;
        }
    }
    Ok((positionals, options))
}

fn required_at(values: &[String], index: usize, label: &str) -> Result<String, ParseError> {
    values
        .get(index)
        .cloned()
        .ok_or_else(|| required(label))
        .and_then(|value| valid_positional(label, value))
}

fn reject_extra(values: &[String], expected: usize) -> Result<(), ParseError> {
    if let Some(value) = values.get(expected) {
        return Err(unexpected(value));
    }
    Ok(())
}

fn require_positional(value: &str, label: &str) -> Result<(), ParseError> {
    if value.is_empty() {
        return Err(required(label));
    }
    Ok(())
}

fn valid_positional(label: &str, value: String) -> Result<String, ParseError> {
    if value.is_empty() {
        return Err(validation(
            label,
            linyaps_i18n::gettext("Input parameter is empty, please input valid parameter instead"),
        ));
    }
    Ok(value)
}

fn valid_string(option: &str, value: String) -> Result<String, ParseError> {
    if value.is_empty() {
        return Err(validation(
            option,
            linyaps_i18n::gettext("Input parameter is empty, please input valid parameter instead"),
        ));
    }
    Ok(value)
}

fn split_nonempty(option: &str, value: String) -> Result<Vec<String>, ParseError> {
    valid_string(option, value.clone())?;
    Ok(value
        .split(',')
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect())
}

fn optional_vector_values(
    arguments: &[String],
    index: usize,
    attached: Option<&str>,
    reserve_positional: bool,
) -> (Vec<String>, usize) {
    if let Some(value) = attached {
        return (vec![value.to_string()], 1);
    }
    let mut end = index + 1;
    while end < arguments.len() && arguments[end] != "--" && !arguments[end].starts_with('-') {
        end += 1;
    }
    if reserve_positional && end > index + 1 {
        end -= 1;
    }
    let values = arguments[index + 1..end].to_vec();
    if values.is_empty() {
        (vec![String::new()], 1)
    } else {
        let consumed = values.len() + 1;
        (values, consumed)
    }
}

fn required_option_value(
    arguments: &[String],
    index: usize,
    option: &str,
    type_name: &str,
    attached: Option<&str>,
) -> Result<(String, usize), ParseError> {
    if let Some(value) = attached.filter(|value| !value.is_empty()) {
        return Ok((value.to_string(), 1));
    }
    let Some(value) = arguments.get(index + 1) else {
        return Err(missing(option, type_name));
    };
    if value == "--" || value.starts_with("--") {
        return Err(missing(option, type_name));
    }
    Ok((value.clone(), 2))
}

fn split_long(token: &str) -> (&str, Option<&str>) {
    if !token.starts_with("--") {
        return (token, None);
    }
    token
        .split_once('=')
        .map_or((token, None), |(name, value)| (name, Some(value)))
}

fn parse_bool(option: &str, value: &str) -> Result<bool, ParseError> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        _ => value
            .parse::<i64>()
            .map(|value| value != 0)
            .map_err(|_| conversion(option, value)),
    }
}

fn parse_bool_flag(option: &str, value: Option<&str>) -> Result<bool, ParseError> {
    value
        .map(|value| parse_bool(option, value))
        .transpose()
        .map(|value| value.unwrap_or(true))
}

fn is_top_command(value: &str) -> bool {
    matches!(
        value,
        "run"
            | "ps"
            | "enter"
            | "kill"
            | "install"
            | "uninstall"
            | "upgrade"
            | "search"
            | "list"
            | "analyze"
            | "repo"
            | "info"
            | "content"
            | "prune"
            | "inspect"
    )
}

fn unexpected(value: &str) -> ParseError {
    ParseError {
        code: 109,
        message: format!("The following argument was not expected: {value}"),
    }
}

fn unexpected_many(values: &[String]) -> ParseError {
    ParseError {
        code: 109,
        message: format!(
            "The following arguments were not expected: {}",
            values.join(" ")
        ),
    }
}

fn unexpected_option(value: &str) -> ParseError {
    let value = if value.starts_with('-') && !value.starts_with("--") && value.len() > 2 {
        format!("-{}", value.chars().nth(1).unwrap_or_default())
    } else {
        value.to_string()
    };
    unexpected(&value)
}

fn required(name: &str) -> ParseError {
    ParseError {
        code: 106,
        message: format!("{name} is required"),
    }
}

fn subcommand_required() -> ParseError {
    ParseError {
        code: 106,
        message: "A subcommand is required".to_string(),
    }
}

fn missing(option: &str, type_name: &str) -> ParseError {
    ParseError {
        code: 114,
        message: format!("{option}: 1 required {type_name} missing"),
    }
}

fn validation(option: &str, message: impl std::fmt::Display) -> ParseError {
    ParseError {
        code: 105,
        message: format!("{option}: {message}"),
    }
}

fn conversion(name: &str, value: &str) -> ParseError {
    ParseError {
        code: 104,
        message: format!("Could not convert: {name} = {value}"),
    }
}

fn requires(option: &str, required: &str) -> ParseError {
    ParseError {
        code: 107,
        message: format!("{option} requires {required}"),
    }
}

fn at_most_one(option: &str) -> ParseError {
    ParseError {
        code: 114,
        message: format!("{option}: At Most 1 required but received 2"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_values(values: &[&str]) -> Result<Cli, ParseError> {
        parse(&values.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_cli11_flag_values_and_numeric_enum() {
        let cli = parse_values(&[
            "--json=ignored",
            "--verbose=2",
            "run",
            "--device-mode",
            "0",
            "--enable-pipewire=false",
            "demo",
        ])
        .unwrap();
        assert!(cli.json);
        assert_eq!(cli.verbose, 2);
        let Some(Command::Run(run)) = cli.command else {
            panic!("run command expected");
        };
        assert_eq!(run.device_mode.len(), 1);
        assert_eq!(run.enable_pipewire, Some(false));
    }

    #[test]
    fn preserves_cli11_vector_and_subcommand_semantics() {
        let cli = parse_values(&[
            "run",
            "demo",
            "--file",
            "one",
            "two",
            "--extensions=a,,b",
            "list",
        ])
        .unwrap();
        let Some(Command::Run(run)) = cli.command else {
            panic!("run command expected");
        };
        assert_eq!(run.files, ["one", "two"]);
        assert_eq!(run.extensions, ["a", "b"]);
        assert!(run.command.is_empty());

        let error = parse_values(&["list", "run"]).unwrap_err();
        assert_eq!(error.code, 106);
        assert!(error.to_string().starts_with("APP is required\n"));

        let cli = parse_values(&["kill", "app", "-s=TERM"]).unwrap();
        let Some(Command::Kill(kill)) = cli.command else {
            panic!("kill command expected");
        };
        assert_eq!(kill.signal, "=TERM");
    }

    #[test]
    fn errors_use_cli11_codes_and_messages() {
        let error = parse_values(&["run"]).unwrap_err();
        assert_eq!(error.code, 106);
        assert_eq!(
            error.to_string(),
            "APP is required\nRun with --help or --help-all for more information."
        );
        let error = parse_values(&["repo", "set-priority", "stable", "bad"]).unwrap_err();
        assert_eq!(error.code, 104);
        assert!(
            error
                .to_string()
                .starts_with("Could not convert: PRIORITY = bad\n")
        );
    }
}
