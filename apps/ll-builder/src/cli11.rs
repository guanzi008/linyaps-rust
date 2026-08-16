use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{
    Build, Cli, Command, Create, Export, Extract, Import, ImportDir, ProjectFile, Push, Remove,
    Repo, RepoCommand, Run,
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
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut cli = Cli {
        version: false,
        help_all: false,
        command: None,
    };
    let mut commands = Vec::new();
    let mut index = 0;
    let mut root_separator = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if token == "--" {
            root_separator = true;
            index += 1;
            continue;
        }
        let (name, attached) = split_long(token);
        if !root_separator && name == "--version" {
            cli.version = parse_bool_flag(name, attached)?;
            index += 1;
            continue;
        }
        if !is_top_command(token) {
            if root_separator {
                let mut values = arguments[index..].to_vec();
                values.push("--".to_string());
                return Err(unexpected_many(&values));
            }
            return Err(unexpected(token));
        }
        let end = command_end(&arguments, index + 1);
        let rest = &arguments[index + 1..end];
        commands.push(match token.as_str() {
            "create" => parse_create(rest)?,
            "build" => parse_build(rest)?,
            "run" => parse_run(rest)?,
            "list" => {
                reject_all(rest)?;
                Command::List
            }
            "remove" => parse_remove(rest)?,
            "export" => parse_export(rest)?,
            "push" => parse_push(rest)?,
            "import" => parse_import(rest)?,
            "import-dir" => parse_import_dir(rest)?,
            "extract" => parse_extract(rest)?,
            "clean" => parse_clean(rest)?,
            "repo" => parse_repo(rest)?,
            _ => unreachable!(),
        });
        index = end;
    }
    cli.command = commands.into_iter().min_by_key(command_priority);
    Ok(cli)
}

fn command_end(arguments: &[String], start: usize) -> usize {
    for (index, token) in arguments.iter().enumerate().skip(start) {
        if token != "--" && is_top_command(token) {
            return index;
        }
    }
    arguments.len()
}

fn is_top_command(value: &str) -> bool {
    matches!(
        value,
        "create"
            | "build"
            | "run"
            | "list"
            | "remove"
            | "export"
            | "push"
            | "import"
            | "import-dir"
            | "extract"
            | "clean"
            | "repo"
    )
}

fn command_priority(command: &Command) -> usize {
    match command {
        Command::Create(_) => 0,
        Command::Extract(_) => 1,
        Command::Repo(_) => 2,
        Command::Import(_) => 3,
        Command::ImportDir(_) => 4,
        Command::List => 5,
        Command::Remove(_) => 6,
        Command::Export(_) => 7,
        Command::Push(_) => 8,
        Command::Clean(_) => 9,
        Command::Build(_) => 10,
        Command::Run(_) => 11,
    }
}

fn parse_create(arguments: &[String]) -> Result<Command, ParseError> {
    let (positionals, unknown) = positionals_and_unknown(arguments);
    let name = required_at(&positionals, 0, "NAME")?;
    reject_extra_with_separator(&positionals, 1, arguments.contains(&"--".to_string()))?;
    reject_unknown(&unknown)?;
    Ok(Command::Create(Create {
        name: validate_nonempty("NAME", name)?,
    }))
}

fn parse_build(arguments: &[String]) -> Result<Command, ParseError> {
    let mut build = Build {
        file: None,
        offline: false,
        full_develop_module: false,
        skip_fetch_source: false,
        skip_pull_depend: false,
        skip_run_container: false,
        skip_commit_output: false,
        skip_output_check: false,
        skip_strip_symbols: false,
        isolate_network: false,
        command: Vec::new(),
    };
    let mut index = 0;
    let mut separated = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if separated {
            build.command.push(arguments[index].clone());
            index += 1;
            continue;
        }
        let token = &arguments[index];
        if let Some(attached) = option(token, "--file", Some("-f")) {
            let (value, consumed) =
                required_option_value(arguments, index, "--file", "FILE:FILE", attached)?;
            validate_existing_file("--file", &value)?;
            build.file = Some(PathBuf::from(value));
            index += consumed;
            continue;
        }
        let (name, attached) = split_long(token);
        let target = match name {
            "--offline" => Some(&mut build.offline),
            "--full-develop-module" => Some(&mut build.full_develop_module),
            "--skip-fetch-source" => Some(&mut build.skip_fetch_source),
            "--skip-pull-depend" => Some(&mut build.skip_pull_depend),
            "--skip-run-container" => Some(&mut build.skip_run_container),
            "--skip-commit-output" => Some(&mut build.skip_commit_output),
            "--skip-output-check" => Some(&mut build.skip_output_check),
            "--skip-strip-symbols" => Some(&mut build.skip_strip_symbols),
            "--isolate-network" => Some(&mut build.isolate_network),
            _ => None,
        };
        if let Some(target) = target {
            let display_name = if name == "--full-develop-module" {
                ""
            } else {
                name
            };
            *target = parse_bool_flag(display_name, attached)?;
        } else if token.starts_with('-') {
            return Err(unexpected(token));
        } else {
            build.command.push(token.clone());
        }
        index += 1;
    }
    Ok(Command::Build(build))
}

fn parse_run(arguments: &[String]) -> Result<Command, ParseError> {
    let mut run = Run {
        file: None,
        modules: Vec::new(),
        workdir: None,
        debug: false,
        extensions: Vec::new(),
        command: Vec::new(),
    };
    let mut index = 0;
    let mut separated = false;
    while index < arguments.len() {
        if !separated && arguments[index] == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if separated {
            run.command.push(arguments[index].clone());
            index += 1;
            continue;
        }
        let token = &arguments[index];
        if let Some(attached) = option(token, "--file", Some("-f")) {
            let (value, consumed) =
                required_option_value(arguments, index, "--file", "FILE:FILE", attached)?;
            validate_existing_file("--file", &value)?;
            run.file = Some(PathBuf::from(value));
            index += consumed;
            continue;
        }
        let (name, attached) = split_long(token);
        if matches!(name, "--modules" | "--workdir" | "--extensions") {
            let type_name = match name {
                "--modules" => "modules",
                "--workdir" => "PATH",
                _ => "REF",
            };
            let (value, consumed) =
                required_option_value(arguments, index, name, type_name, attached)?;
            if name == "--modules" {
                run.modules.extend(split_delimited(value));
            } else if name == "--workdir" {
                run.workdir = Some(PathBuf::from(value));
            } else {
                validate_nonempty(name, value.clone())?;
                run.extensions.extend(split_delimited(value));
            }
            index += consumed;
            continue;
        }
        if name == "--debug" {
            run.debug = parse_bool_flag(name, attached)?;
        } else if token.starts_with('-') {
            return Err(unexpected(token));
        } else {
            run.command.push(token.clone());
        }
        index += 1;
    }
    Ok(Command::Run(run))
}

fn parse_remove(arguments: &[String]) -> Result<Command, ParseError> {
    let mut remove = Remove {
        no_clean_objects: false,
        apps: Vec::new(),
    };
    let mut separated = false;
    for token in arguments {
        if !separated && token == "--" {
            separated = true;
            continue;
        }
        let (name, attached) = split_long(token);
        if !separated && name == "--no-clean-objects" {
            remove.no_clean_objects = parse_bool_flag(name, attached)?;
        } else if !separated && token.starts_with('-') {
            return Err(unexpected(token));
        } else {
            remove.apps.push(token.clone());
        }
    }
    Ok(Command::Remove(remove))
}

fn parse_export(arguments: &[String]) -> Result<Command, ParseError> {
    let mut export = Export {
        file: None,
        compressor: None,
        icon: None,
        layer: false,
        loader: None,
        no_develop: false,
        output: None,
        reference: None,
        modules: Vec::new(),
    };
    let mut layer_present = false;
    let mut icon_present = false;
    let mut loader_present = false;
    let mut output_present = false;
    let mut reference_present = false;
    let mut modules_present = false;
    let mut no_develop_present = false;
    let mut index = 0;
    let mut separated = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if !separated && token == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if separated {
            return Err(unexpected(token));
        }
        if let Some(attached) = option(token, "--file", Some("-f")) {
            let (value, consumed) =
                required_option_value(arguments, index, "--file", "FILE:FILE", attached)?;
            validate_existing_file("--file", &value)?;
            export.file = Some(PathBuf::from(value));
            index += consumed;
            continue;
        }
        if let Some(attached) = option(token, "--compressor", Some("-z")) {
            let (value, consumed) =
                required_option_value(arguments, index, "--compressor", "X", attached)?;
            export.compressor = Some(value);
            index += consumed;
            continue;
        }
        if let Some(attached) = option(token, "--output", Some("-o")) {
            let (value, consumed) =
                required_option_value(arguments, index, "--output", "FILE", attached)?;
            export.output = Some(PathBuf::from(value));
            output_present = true;
            index += consumed;
            continue;
        }
        let (name, attached) = split_long(token);
        if matches!(name, "--icon" | "--loader" | "--ref" | "--modules") {
            let type_name = match name {
                "--icon" | "--loader" => "FILE:FILE",
                "--ref" => "REF",
                _ => "MODULES",
            };
            let (value, consumed) =
                required_option_value(arguments, index, name, type_name, attached)?;
            match name {
                "--icon" => {
                    validate_existing_file(name, &value)?;
                    export.icon = Some(PathBuf::from(value));
                    icon_present = true;
                }
                "--loader" => {
                    validate_existing_file(name, &value)?;
                    export.loader = Some(PathBuf::from(value));
                    loader_present = true;
                }
                "--ref" => {
                    export.reference = Some(validate_nonempty(name, value)?);
                    reference_present = true;
                }
                _ => {
                    validate_nonempty(name, value.clone())?;
                    export.modules.extend(split_delimited(value));
                    modules_present = true;
                }
            }
            index += consumed;
            continue;
        }
        match name {
            "--layer" => {
                export.layer = parse_bool_flag(name, attached)?;
                layer_present = true;
            }
            "--no-develop" => {
                export.no_develop = parse_bool_flag(name, attached)?;
                no_develop_present = true;
            }
            _ => return Err(unexpected(token)),
        }
        index += 1;
    }
    if icon_present && layer_present {
        return Err(excludes("--icon", "--layer"));
    }
    if loader_present && layer_present {
        return Err(excludes("--layer", "--loader"));
    }
    if output_present && layer_present {
        return Err(excludes("--layer", "--output"));
    }
    if reference_present && layer_present {
        return Err(excludes("--layer", "--ref"));
    }
    if modules_present && layer_present {
        return Err(excludes("--layer", "--modules"));
    }
    if no_develop_present && !layer_present {
        return Err(requires("--no-develop", "--layer"));
    }
    Ok(Command::Export(export))
}

fn parse_push(arguments: &[String]) -> Result<Command, ParseError> {
    let mut push = Push {
        file: None,
        repo_url: None,
        repo_name: None,
        module: None,
    };
    let mut index = 0;
    let mut seen = std::collections::BTreeSet::new();
    let mut separated = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if !separated && token == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if separated {
            return Err(unexpected(token));
        }
        if let Some(attached) = option(token, "--file", Some("-f")) {
            let (value, consumed) =
                required_option_value(arguments, index, "--file", "FILE:FILE", attached)?;
            validate_existing_file("--file", &value)?;
            if !seen.insert("--file") {
                return Err(at_most_one("--file"));
            }
            push.file = Some(PathBuf::from(value));
            index += consumed;
            continue;
        }
        let (name, attached) = split_long(token);
        if matches!(name, "--repo-url" | "--repo-name" | "--module") {
            let type_name = match name {
                "--repo-url" => "URL",
                "--repo-name" => "NAME",
                _ => "TEXT",
            };
            let (value, consumed) =
                required_option_value(arguments, index, name, type_name, attached)?;
            let value = validate_nonempty(name, value)?;
            if !seen.insert(name) {
                return Err(at_most_one(name));
            }
            match name {
                "--repo-url" => push.repo_url = Some(value),
                "--repo-name" => push.repo_name = Some(value),
                _ => push.module = Some(value),
            }
            index += consumed;
            continue;
        }
        return Err(unexpected(token));
    }
    Ok(Command::Push(push))
}

fn parse_import(arguments: &[String]) -> Result<Command, ParseError> {
    let (positionals, unknown) = positionals_and_unknown(arguments);
    let layer = required_at(&positionals, 0, "LAYER")?;
    reject_extra_with_separator(&positionals, 1, arguments.contains(&"--".to_string()))?;
    reject_unknown(&unknown)?;
    validate_existing_file("LAYER", &layer)?;
    Ok(Command::Import(Import {
        layer: PathBuf::from(layer),
    }))
}

fn parse_import_dir(arguments: &[String]) -> Result<Command, ParseError> {
    let (positionals, unknown) = positionals_and_unknown(arguments);
    let path = required_at(&positionals, 0, "PATH")?;
    reject_extra_with_separator(&positionals, 1, arguments.contains(&"--".to_string()))?;
    reject_unknown(&unknown)?;
    Ok(Command::ImportDir(ImportDir {
        path: PathBuf::from(path),
    }))
}

fn parse_extract(arguments: &[String]) -> Result<Command, ParseError> {
    let (positionals, unknown) = positionals_and_unknown(arguments);
    let layer = required_at(&positionals, 0, "LAYER")?;
    let destination = required_at(&positionals, 1, "DIR")?;
    reject_extra_with_separator(&positionals, 2, arguments.contains(&"--".to_string()))?;
    reject_unknown(&unknown)?;
    validate_existing_file("LAYER", &layer)?;
    Ok(Command::Extract(Extract {
        layer: PathBuf::from(layer),
        destination: PathBuf::from(destination),
    }))
}

fn parse_clean(arguments: &[String]) -> Result<Command, ParseError> {
    let mut file = None;
    let mut index = 0;
    let mut separated = false;
    while index < arguments.len() {
        let token = &arguments[index];
        if !separated && token == "--" {
            separated = true;
            index += 1;
            continue;
        }
        if separated {
            return Err(unexpected(token));
        }
        if let Some(attached) = option(token, "--file", Some("-f")) {
            let (value, consumed) =
                required_option_value(arguments, index, "--file", "FILE:FILE", attached)?;
            validate_existing_file("--file", &value)?;
            file = Some(PathBuf::from(value));
            index += consumed;
        } else {
            return Err(unexpected(token));
        }
    }
    Ok(Command::Clean(ProjectFile { file }))
}

fn parse_repo(arguments: &[String]) -> Result<Command, ParseError> {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return Err(subcommand_required());
    };
    let command = match subcommand.as_str() {
        "add" => {
            let (positionals, options) = repo_values(rest, &["--alias"])?;
            let name = validate_nonempty("NAME", required_at(&positionals, 0, "NAME")?)?;
            let url = validate_nonempty("URL", required_at(&positionals, 1, "URL")?)?;
            reject_extra(&positionals, 2)?;
            RepoCommand::Add {
                name,
                url,
                alias: options.get("--alias").cloned(),
            }
        }
        "modify" => {
            let (positionals, options) = repo_values(rest, &["--name"])?;
            let url = validate_nonempty("URL", required_at(&positionals, 0, "URL")?)?;
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
            reject_all(rest)?;
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

fn option<'a>(token: &'a str, long: &str, short: Option<&str>) -> Option<Option<&'a str>> {
    if token == long || short.is_some_and(|short| token == short) {
        return Some(None);
    }
    if let Some(value) = token
        .strip_prefix(long)
        .and_then(|value| value.strip_prefix('='))
    {
        return Some(Some(value));
    }
    let short = short?;
    let value = token.strip_prefix(short)?;
    if value.is_empty() {
        Some(None)
    } else {
        Some(Some(value))
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

fn parse_bool_flag(option: &str, value: Option<&str>) -> Result<bool, ParseError> {
    let Some(value) = value else {
        return Ok(true);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        _ => value
            .parse::<i64>()
            .map(|value| value != 0)
            .map_err(|_| conversion(option, value)),
    }
}

fn split_delimited(value: String) -> Vec<String> {
    value
        .split(',')
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn positionals_and_unknown(arguments: &[String]) -> (Vec<String>, Vec<String>) {
    let mut positionals = Vec::new();
    let mut unknown = Vec::new();
    let mut separated = false;
    for token in arguments {
        if !separated && token == "--" {
            separated = true;
        } else if !separated && token.starts_with('-') {
            unknown.push(token.clone());
        } else {
            positionals.push(token.clone());
        }
    }
    (positionals, unknown)
}

fn exact_positionals(arguments: &[String]) -> Result<Vec<String>, ParseError> {
    let (positionals, unknown) = positionals_and_unknown(arguments);
    reject_unknown(&unknown)?;
    Ok(positionals)
}

fn exact_required(arguments: &[String], label: &str) -> Result<String, ParseError> {
    let values = exact_positionals(arguments)?;
    let value = required_at(&values, 0, label)?;
    reject_extra(&values, 1)?;
    validate_nonempty(label, value)
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
            options.insert(name.to_string(), validate_nonempty(name, value)?);
            index += consumed;
        } else if !separated && arguments[index].starts_with('-') {
            return Err(unexpected(&arguments[index]));
        } else {
            positionals.push(arguments[index].clone());
            index += 1;
        }
    }
    Ok((positionals, options))
}

fn required_at(values: &[String], index: usize, label: &str) -> Result<String, ParseError> {
    values.get(index).cloned().ok_or_else(|| required(label))
}

fn reject_extra(values: &[String], expected: usize) -> Result<(), ParseError> {
    if let Some(value) = values.get(expected) {
        return Err(unexpected(value));
    }
    Ok(())
}

fn reject_extra_with_separator(
    values: &[String],
    expected: usize,
    separator: bool,
) -> Result<(), ParseError> {
    if let Some(value) = values.get(expected) {
        if separator {
            return Err(unexpected_many(&[value.clone(), "--".to_string()]));
        }
        return Err(unexpected(value));
    }
    Ok(())
}

fn priority_positionals(arguments: &[String]) -> Result<Vec<String>, ParseError> {
    let mut values = Vec::new();
    let mut separated = false;
    for value in arguments {
        if !separated && value == "--" {
            separated = true;
        } else if !separated && value.starts_with('-') && values.len() != 1 {
            return Err(unexpected(value));
        } else {
            values.push(value.clone());
        }
    }
    Ok(values)
}

fn reject_unknown(values: &[String]) -> Result<(), ParseError> {
    if let Some(value) = values.first() {
        return Err(unexpected(value));
    }
    Ok(())
}

fn reject_all(arguments: &[String]) -> Result<(), ParseError> {
    if let Some(value) = arguments.iter().find(|value| value.as_str() != "--") {
        return Err(unexpected(value));
    }
    Ok(())
}

fn validate_nonempty(name: &str, value: String) -> Result<String, ParseError> {
    if value.is_empty() {
        return Err(validation(
            name,
            linyaps_i18n::gettext("Input parameter is empty, please input valid parameter instead"),
        ));
    }
    Ok(value)
}

fn validate_existing_file(name: &str, value: &str) -> Result<(), ParseError> {
    if !Path::new(value).is_file() {
        return Err(validation(name, format!("File does not exist: {value}")));
    }
    Ok(())
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

fn validation(name: &str, message: impl std::fmt::Display) -> ParseError {
    ParseError {
        code: 105,
        message: format!("{name}: {message}"),
    }
}

fn conversion(name: &str, value: &str) -> ParseError {
    ParseError {
        code: 104,
        message: format!("Could not convert: {name} = {value}"),
    }
}

fn excludes(option: &str, excluded: &str) -> ParseError {
    ParseError {
        code: 108,
        message: format!("{option} excludes {excluded}"),
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

    fn values(arguments: &[&str]) -> Vec<OsString> {
        arguments.iter().map(OsString::from).collect()
    }

    #[test]
    fn accepts_cli11_boolean_values() {
        let cli = parse(&values(&["build", "--offline=1", "--skip-fetch-source=0"])).unwrap();
        let Some(Command::Build(build)) = cli.command else {
            panic!("build command expected");
        };
        assert!(build.offline);
        assert!(!build.skip_fetch_source);
    }

    #[test]
    fn preserves_delimiters_and_multiple_subcommands() {
        let cli = parse(&values(&["run", "--modules=a,,b", "--extensions=x,,y"])).unwrap();
        let Some(Command::Run(run)) = cli.command else {
            panic!("run command expected");
        };
        assert_eq!(run.modules, ["a", "b"]);
        assert_eq!(run.extensions, ["x", "y"]);

        let cli = parse(&values(&["build", "echo", "--", "list"])).unwrap();
        assert!(matches!(cli.command, Some(Command::List)));
    }

    #[test]
    fn reports_cli11_errors() {
        let error = parse(&values(&["create"])).unwrap_err();
        assert_eq!(error.code, 106);
        assert_eq!(
            error.to_string(),
            "NAME is required\nRun with --help or --help-all for more information."
        );
        let error = parse(&values(&["export", "--no-develop"])).unwrap_err();
        assert_eq!(error.code, 107);
        assert!(
            error
                .to_string()
                .starts_with("--no-develop requires --layer\n")
        );
    }
}
