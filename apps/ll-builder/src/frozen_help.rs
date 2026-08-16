use std::borrow::Cow;
use std::ffi::OsString;

pub fn requested(arguments: &[OsString]) -> Option<Cow<'static, str>> {
    let end = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    let marker = arguments[..end]
        .iter()
        .position(|argument| matches!(argument.to_str(), Some("-h" | "--help" | "--help-all")))?;
    let expanded = arguments[marker] == "--help-all";
    let path = command_path(&arguments[..marker]);
    let key = help_key(&path, expanded);
    Some(render(key, help(key)))
}

pub fn expanded_root() -> Cow<'static, str> {
    render("help-all", help("help-all"))
}

fn render(key: &str, document: &'static str) -> Cow<'static, str> {
    let Some(language) = linyaps_i18n::language() else {
        return Cow::Borrowed(document);
    };
    if let Some(document) = crate::localized_help::get(language, key) {
        return Cow::Borrowed(document);
    }
    Cow::Owned(linyaps_i18n::translate_document(document))
}

fn help_key(path: &[&str], expanded: bool) -> &'static str {
    match (path, expanded) {
        ([], true) => "help-all",
        ([], false) => "root",
        (["repo"], true) => "repo_help-all",
        (["create"], _) => "create",
        (["build"], _) => "build",
        (["run"], _) => "run",
        (["list"], _) => "list",
        (["remove"], _) => "remove",
        (["export"], _) => "export",
        (["push"], _) => "push",
        (["import"], _) => "import",
        (["import-dir"], _) => "import-dir",
        (["extract"], _) => "extract",
        (["clean"], _) => "clean",
        (["repo"], _) => "repo",
        (["repo", "add"], _) => "repo_add",
        (["repo", "remove"], _) => "repo_remove",
        (["repo", "update"], _) => "repo_update",
        (["repo", "set-default"], _) => "repo_set-default",
        (["repo", "show"], _) => "repo_show",
        (["repo", "set-priority"], _) => "repo_set-priority",
        (["repo", "enable-mirror"], _) => "repo_enable-mirror",
        (["repo", "disable-mirror"], _) => "repo_disable-mirror",
        _ => "root",
    }
}

fn command_path(arguments: &[OsString]) -> Vec<&str> {
    const COMMANDS: &[&str] = &[
        "create",
        "build",
        "run",
        "list",
        "remove",
        "export",
        "push",
        "import",
        "import-dir",
        "extract",
        "clean",
        "repo",
    ];
    let Some((index, command)) = arguments.iter().enumerate().find_map(|(index, argument)| {
        let argument = argument.to_str()?;
        COMMANDS.contains(&argument).then_some((index, argument))
    }) else {
        return Vec::new();
    };
    let mut path = vec![command];
    if command == "repo"
        && let Some(child) = arguments[index + 1..]
            .iter()
            .filter_map(|argument| argument.to_str())
            .find(|argument| {
                matches!(
                    *argument,
                    "add"
                        | "remove"
                        | "update"
                        | "set-default"
                        | "show"
                        | "set-priority"
                        | "enable-mirror"
                        | "disable-mirror"
                )
            })
    {
        path.push(child);
    }
    path
}

fn help(key: &str) -> &'static str {
    match key {
        "help-all" => include_str!("../help/help-all.txt"),
        "root" => include_str!("../help/root.txt"),
        "repo_help-all" => include_str!("../help/repo_help-all.txt"),
        "create" => include_str!("../help/create.txt"),
        "build" => include_str!("../help/build.txt"),
        "run" => include_str!("../help/run.txt"),
        "list" => include_str!("../help/list.txt"),
        "remove" => include_str!("../help/remove.txt"),
        "export" => include_str!("../help/export.txt"),
        "push" => include_str!("../help/push.txt"),
        "import" => include_str!("../help/import.txt"),
        "import-dir" => include_str!("../help/import-dir.txt"),
        "extract" => include_str!("../help/extract.txt"),
        "clean" => include_str!("../help/clean.txt"),
        "repo" => include_str!("../help/repo.txt"),
        "repo_add" => include_str!("../help/repo_add.txt"),
        "repo_remove" => include_str!("../help/repo_remove.txt"),
        "repo_update" => include_str!("../help/repo_update.txt"),
        "repo_set-default" => include_str!("../help/repo_set-default.txt"),
        "repo_show" => include_str!("../help/repo_show.txt"),
        "repo_set-priority" => include_str!("../help/repo_set-priority.txt"),
        "repo_enable-mirror" => include_str!("../help/repo_enable-mirror.txt"),
        "repo_disable-mirror" => include_str!("../help/repo_disable-mirror.txt"),
        _ => include_str!("../help/root.txt"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn selects_nested_and_expanded_help() {
        assert!(
            requested(&args(&["--help"]))
                .unwrap()
                .starts_with("linyaps builder CLI ")
        );
        assert!(
            requested(&args(&["build", "--help"]))
                .unwrap()
                .starts_with("Build a linyaps project\n")
        );
        assert!(
            requested(&args(&["repo", "--help-all"]))
                .unwrap()
                .contains("PRIORITY INT REQUIRED")
        );
        assert!(requested(&args(&["build", "--", "--help"])).is_none());
    }
}
