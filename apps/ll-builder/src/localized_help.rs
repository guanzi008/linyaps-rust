pub fn get(language: &str, key: &str) -> Option<&'static str> {
    Some(match (language, key) {
        ("ca", "clean") => include_str!("../help/localized/ca/clean.txt"),
        ("ca", "import-dir") => include_str!("../help/localized/ca/import-dir.txt"),
        ("es", "clean") => include_str!("../help/localized/es/clean.txt"),
        ("fi", "clean") => include_str!("../help/localized/fi/clean.txt"),
        ("fr", "repo_add") => include_str!("../help/localized/fr/repo_add.txt"),
        ("fr", "repo_disable-mirror") => {
            include_str!("../help/localized/fr/repo_disable-mirror.txt")
        }
        ("fr", "repo_enable-mirror") => include_str!("../help/localized/fr/repo_enable-mirror.txt"),
        ("fr", "repo_help-all") => include_str!("../help/localized/fr/repo_help-all.txt"),
        ("fr", "repo_remove") => include_str!("../help/localized/fr/repo_remove.txt"),
        ("fr", "repo_set-default") => include_str!("../help/localized/fr/repo_set-default.txt"),
        ("fr", "repo_set-priority") => include_str!("../help/localized/fr/repo_set-priority.txt"),
        ("fr", "repo_update") => include_str!("../help/localized/fr/repo_update.txt"),
        ("it", "clean") => include_str!("../help/localized/it/clean.txt"),
        ("pl", "clean") => include_str!("../help/localized/pl/clean.txt"),
        ("pt_BR", "clean") => include_str!("../help/localized/pt_BR/clean.txt"),
        ("ru", "clean") => include_str!("../help/localized/ru/clean.txt"),
        ("sq", "export") => include_str!("../help/localized/sq/export.txt"),
        ("sq", "help-all") => include_str!("../help/localized/sq/help-all.txt"),
        ("zh_CN", "clean") => include_str!("../help/localized/zh_CN/clean.txt"),
        ("zh_HK", "build") => include_str!("../help/localized/zh_HK/build.txt"),
        ("zh_HK", "clean") => include_str!("../help/localized/zh_HK/clean.txt"),
        ("zh_HK", "create") => include_str!("../help/localized/zh_HK/create.txt"),
        ("zh_HK", "help-all") => include_str!("../help/localized/zh_HK/help-all.txt"),
        ("zh_HK", "root") => include_str!("../help/localized/zh_HK/root.txt"),
        ("zh_TW", "clean") => include_str!("../help/localized/zh_TW/clean.txt"),
        _ => return None,
    })
}
