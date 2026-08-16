use std::borrow::Cow;
use std::ffi::OsString;

const MINIMAL: &str = "linyaps CLI\nA CLI program to run application and manage application and runtime\n\nUsage: [OPTIONS]\n\nOptions:\n  -h,--help                   Print this help message and exit\n\n";

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

pub fn minimal() -> Cow<'static, str> {
    render("minimal", MINIMAL)
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
        (["analyze"], true) => "analyze_help-all",
        (["repo"], true) => "repo_help-all",
        (["run"], _) => "run",
        (["ps"], _) => "ps",
        (["enter"], _) => "enter",
        (["kill"], _) => "kill",
        (["install"], _) => "install",
        (["uninstall"], _) => "uninstall",
        (["upgrade"], _) => "upgrade",
        (["search"], _) => "search",
        (["list"], _) => "list",
        (["analyze"], _) => "analyze",
        (["analyze", "size"], _) => "analyze_size",
        (["analyze", "depends"], _) => "analyze_depends",
        (["repo"], _) => "repo",
        (["repo", "add"], _) => "repo_add",
        (["repo", "remove"], _) => "repo_remove",
        (["repo", "update"], _) => "repo_update",
        (["repo", "set-default"], _) => "repo_set-default",
        (["repo", "show"], _) => "repo_show",
        (["repo", "set-priority"], _) => "repo_set-priority",
        (["repo", "enable-mirror"], _) => "repo_enable-mirror",
        (["repo", "disable-mirror"], _) => "repo_disable-mirror",
        (["info"], _) => "info",
        (["content"], _) => "content",
        (["prune"], _) => "prune",
        (["inspect"], _) => "inspect",
        (["inspect", "dir"], _) => "inspect_dir",
        _ => "root",
    }
}

fn command_path(arguments: &[OsString]) -> Vec<&str> {
    const COMMANDS: &[&str] = &[
        "run",
        "ps",
        "enter",
        "kill",
        "install",
        "uninstall",
        "upgrade",
        "search",
        "list",
        "analyze",
        "repo",
        "info",
        "content",
        "prune",
        "inspect",
    ];
    let Some((index, command)) = arguments.iter().enumerate().find_map(|(index, argument)| {
        let argument = argument.to_str()?;
        COMMANDS.contains(&argument).then_some((index, argument))
    }) else {
        return Vec::new();
    };
    let mut path = vec![command];
    let children: &[&str] = match command {
        "analyze" => &["size", "depends"],
        "repo" => &[
            "add",
            "remove",
            "update",
            "set-default",
            "show",
            "set-priority",
            "enable-mirror",
            "disable-mirror",
        ],
        "inspect" => &["dir"],
        _ => &[],
    };
    if let Some(child) = arguments[index + 1..]
        .iter()
        .filter_map(|argument| argument.to_str())
        .find(|argument| children.contains(argument))
    {
        path.push(child);
    }
    path
}

fn help(key: &str) -> &'static str {
    match key {
        "help-all" => include_str!("../help/help-all.txt"),
        "root" => include_str!("../help/root.txt"),
        "analyze_help-all" => include_str!("../help/analyze_help-all.txt"),
        "repo_help-all" => include_str!("../help/repo_help-all.txt"),
        "run" => include_str!("../help/run.txt"),
        "ps" => include_str!("../help/ps.txt"),
        "enter" => include_str!("../help/enter.txt"),
        "kill" => include_str!("../help/kill.txt"),
        "install" => include_str!("../help/install.txt"),
        "uninstall" => include_str!("../help/uninstall.txt"),
        "upgrade" => include_str!("../help/upgrade.txt"),
        "search" => include_str!("../help/search.txt"),
        "list" => include_str!("../help/list.txt"),
        "analyze" => include_str!("../help/analyze.txt"),
        "analyze_size" => include_str!("../help/analyze_size.txt"),
        "analyze_depends" => include_str!("../help/analyze_depends.txt"),
        "repo" => include_str!("../help/repo.txt"),
        "repo_add" => include_str!("../help/repo_add.txt"),
        "repo_remove" => include_str!("../help/repo_remove.txt"),
        "repo_update" => include_str!("../help/repo_update.txt"),
        "repo_set-default" => include_str!("../help/repo_set-default.txt"),
        "repo_show" => include_str!("../help/repo_show.txt"),
        "repo_set-priority" => include_str!("../help/repo_set-priority.txt"),
        "repo_enable-mirror" => include_str!("../help/repo_enable-mirror.txt"),
        "repo_disable-mirror" => include_str!("../help/repo_disable-mirror.txt"),
        "info" => include_str!("../help/info.txt"),
        "content" => include_str!("../help/content.txt"),
        "prune" => include_str!("../help/prune.txt"),
        "inspect" => include_str!("../help/inspect.txt"),
        "inspect_dir" => include_str!("../help/inspect_dir.txt"),
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
                .starts_with("linyaps CLI\n")
        );
        assert!(
            requested(&args(&["run", "demo", "--help"]))
                .unwrap()
                .starts_with("Run an application\n")
        );
        assert!(
            requested(&args(&["repo", "add", "--help"]))
                .unwrap()
                .starts_with("Add a new repository\n")
        );
        assert!(
            requested(&args(&["analyze", "--help-all"]))
                .unwrap()
                .contains("Sort result by specify field")
        );
        assert!(requested(&args(&["run", "demo", "--", "--help"])).is_none());
    }
}
