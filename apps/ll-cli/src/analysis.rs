use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use linyaps_api::PackageInfoV2;
use linyaps_core::{Architecture, FuzzyReference, Reference, Version};
use linyaps_repository::LocalRepository;
use serde_json::{Value, json};

use super::{Analyze, AnalyzeCommand, SortField};

#[derive(Clone, Debug)]
struct ModuleSizeInfo {
    id: String,
    name: String,
    version: String,
    channel: String,
    module: String,
    exclusive_size: u64,
    shared_size: u64,
    logical_size: u64,
    actual_size: u64,
}

#[derive(Clone, Debug, Default)]
struct ModuleSize {
    exclusive_size: u64,
    shared_size: u64,
    logical_size: u64,
    actual_size: u64,
}

#[derive(Debug, Default)]
struct InodeUsage {
    disk_usage: u64,
    modules: BTreeSet<usize>,
}

#[derive(Clone, Debug)]
struct DependsNode {
    reference: String,
    kind: String,
    children: Vec<DependsNode>,
}

pub(super) async fn run(options: Analyze, json_output: bool) -> Result<(), String> {
    match options.command {
        AnalyzeCommand::Size { sort, asc } => run_size(sort, asc, json_output).await,
        AnalyzeCommand::Depends { app } => run_depends(app, json_output).await,
    }
}

async fn run_size(sort_field: SortField, ascending: bool, json_output: bool) -> Result<(), String> {
    let repository = super::open_local_repository().await?;
    let mut modules = Vec::new();
    let mut paths = Vec::new();
    for item in repository.list_layer_items() {
        let reference = linyaps_repository::reference_from_info(&item.info)
            .map_err(|error| error.to_string())?;
        let path = repository
            .layer_path_for_item(&item)
            .map_err(|error| format!("failed to resolve {reference}: {error}"))?;
        modules.push(ModuleSizeInfo {
            id: item.info.id,
            name: item.info.name,
            version: item.info.version,
            channel: item.info.channel,
            module: item.info.module,
            exclusive_size: 0,
            shared_size: 0,
            logical_size: 0,
            actual_size: 0,
        });
        paths.push(path);
    }
    let (sizes, actual_total_size) = calculate_module_sizes(&paths)?;
    for (module, size) in modules.iter_mut().zip(sizes) {
        module.exclusive_size = size.exclusive_size;
        module.shared_size = size.shared_size;
        module.logical_size = size.logical_size;
        module.actual_size = size.actual_size;
    }
    modules.sort_by(|left, right| match sort_field {
        SortField::Id => {
            let ordering = module_name_cmp(left, right);
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        }
        SortField::Logical => size_cmp(
            left.logical_size,
            right.logical_size,
            left,
            right,
            ascending,
        ),
        SortField::Exclusive => size_cmp(
            left.exclusive_size,
            right.exclusive_size,
            left,
            right,
            ascending,
        ),
        SortField::Shared => size_cmp(left.shared_size, right.shared_size, left, right, ascending),
        SortField::Actual => size_cmp(left.actual_size, right.actual_size, left, right, ascending),
    });
    let repository_size = calculate_real_disk_usage(&repository.root().join("repo"))?;
    print_module_sizes(&modules, actual_total_size, repository_size, json_output)
}

fn size_cmp(
    left_size: u64,
    right_size: u64,
    left: &ModuleSizeInfo,
    right: &ModuleSizeInfo,
    ascending: bool,
) -> Ordering {
    if left_size == right_size {
        return module_name_cmp(left, right);
    }
    if ascending {
        left_size.cmp(&right_size)
    } else {
        right_size.cmp(&left_size)
    }
}

fn module_name_cmp(left: &ModuleSizeInfo, right: &ModuleSizeInfo) -> Ordering {
    left.id
        .cmp(&right.id)
        .then_with(|| left.channel.cmp(&right.channel))
        .then_with(|| left.module.cmp(&right.module))
        .then_with(|| version_cmp(&left.version, &right.version))
}

fn version_cmp(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) if left != right => {
            left.partial_cmp(&right).unwrap_or(Ordering::Equal)
        }
        _ => left.cmp(right),
    }
}

fn calculate_module_sizes(paths: &[std::path::PathBuf]) -> Result<(Vec<ModuleSize>, u64), String> {
    let mut sizes = vec![ModuleSize::default(); paths.len()];
    let mut usages = HashMap::<(u64, u64), InodeUsage>::new();
    let mut actual_total_size = 0_u64;
    for (module_index, path) in paths.iter().enumerate() {
        walk_tree(path, &mut |entry| {
            let metadata = fs::symlink_metadata(entry)
                .map_err(|error| format!("failed to stat {}: {error}", entry.display()))?;
            let disk_usage = metadata.blocks().saturating_mul(512);
            if metadata.nlink() == 1 {
                sizes[module_index].exclusive_size += disk_usage;
                sizes[module_index].logical_size += disk_usage;
                sizes[module_index].actual_size += disk_usage;
                actual_total_size += disk_usage;
            } else {
                let usage = usages.entry((metadata.dev(), metadata.ino())).or_default();
                usage.disk_usage = disk_usage;
                usage.modules.insert(module_index);
            }
            Ok(())
        })?;
    }
    for usage in usages.into_values() {
        if usage.modules.is_empty() {
            continue;
        }
        actual_total_size += usage.disk_usage;
        let divided_size = usage.disk_usage / usage.modules.len() as u64;
        for module_index in usage.modules {
            sizes[module_index].logical_size += usage.disk_usage;
            if divided_size == usage.disk_usage {
                sizes[module_index].exclusive_size += usage.disk_usage;
                sizes[module_index].actual_size += usage.disk_usage;
            } else {
                sizes[module_index].shared_size += usage.disk_usage;
                sizes[module_index].actual_size += divided_size;
            }
        }
    }
    Ok((sizes, actual_total_size))
}

fn calculate_real_disk_usage(path: &Path) -> Result<u64, String> {
    let mut visited = HashSet::new();
    let mut size = 0_u64;
    walk_tree(path, &mut |entry| {
        let metadata = fs::symlink_metadata(entry)
            .map_err(|error| format!("failed to stat {}: {error}", entry.display()))?;
        if metadata.nlink() > 1 && !visited.insert((metadata.dev(), metadata.ino())) {
            return Ok(());
        }
        size += metadata.blocks().saturating_mul(512);
        Ok(())
    })?;
    Ok(size)
}

fn walk_tree(
    path: &Path,
    visit: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    visit(path)?;
    if !fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .is_dir()
    {
        return Ok(());
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(format!("failed to open {}: {error}", path.display())),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(error.to_string()),
        };
        walk_tree(&entry.path(), visit)?;
    }
    Ok(())
}

fn print_module_sizes(
    modules: &[ModuleSizeInfo],
    actual_total_size: u64,
    repository_size: u64,
    json_output: bool,
) -> Result<(), String> {
    let exclusive_total = modules
        .iter()
        .map(|module| module.exclusive_size)
        .sum::<u64>();
    let shared_total = modules.iter().map(|module| module.shared_size).sum::<u64>();
    let logical_total = modules
        .iter()
        .map(|module| module.logical_size)
        .sum::<u64>();
    if json_output {
        let modules = modules
            .iter()
            .map(|module| {
                json!({
                    "id": module.id,
                    "name": module.name,
                    "version": module.version,
                    "channel": module.channel,
                    "module": module.module,
                    "exclusiveSize": module.exclusive_size,
                    "sharedSize": module.shared_size,
                    "logicalSize": module.logical_size,
                    "actualSize": module.actual_size,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "modules": modules,
                "calculatedLogicalSize": {
                    "exclusiveSize": exclusive_total,
                    "sharedSize": shared_total,
                    "logicalSize": logical_total,
                },
                "calculatedActualSize": actual_total_size,
                "repositoryRealSize": repository_size,
            }))
            .map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "\x1b[38;5;214m{}{}{}{}{}{}{}{}\x1b[0m",
        super::display_column(&linyaps_i18n::gettext("ID"), 43),
        super::display_column(&linyaps_i18n::gettext("Version"), 16),
        super::display_column(&linyaps_i18n::gettext("Channel"), 16),
        super::display_column(&linyaps_i18n::gettext("Module"), 14),
        super::display_column(&linyaps_i18n::gettext("Exclusive"), 14),
        super::display_column(&linyaps_i18n::gettext("Shared"), 14),
        super::display_column(&linyaps_i18n::gettext("Logical"), 14),
        linyaps_i18n::gettext("Actual"),
    );
    for module in modules {
        println!(
            "{:<43}{:<16}{:<16}{:<14}{:<14}{:<14}{:<14}{}",
            format!("{} ", module.id),
            format!("{} ", module.version),
            format!("{} ", module.channel),
            format!("{} ", module.module),
            format!("{} ", format_size(module.exclusive_size)),
            format!("{} ", format_size(module.shared_size)),
            format!("{} ", format_size(module.logical_size)),
            format_size(module.actual_size),
        );
    }
    println!();
    println!(
        "{}{} ({}{}, {}{})",
        linyaps_i18n::gettext("Calculated logical total size: "),
        format_size(logical_total),
        linyaps_i18n::gettext("Exclusive: "),
        format_size(exclusive_total),
        linyaps_i18n::gettext("Shared: "),
        format_size(shared_total)
    );
    println!(
        "{}{}",
        linyaps_i18n::gettext("Calculated actual total size: "),
        format_size(actual_total_size)
    );
    println!(
        "{}{}",
        linyaps_i18n::gettext("Repository real size: "),
        format_size(repository_size)
    );
    Ok(())
}

fn format_size(size: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = size as f64;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index + 1 < units.len() {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{size} {}", units[unit_index])
    } else {
        format!("{value:.1} {}", units[unit_index])
    }
}

async fn run_depends(app: Option<String>, json_output: bool) -> Result<(), String> {
    let repository = super::open_local_repository().await?;
    let mut applications = if let Some(app) = app {
        let fuzzy = app
            .parse::<FuzzyReference>()
            .map_err(|error| error.to_string())?;
        let reference = repository
            .resolve_local(&fuzzy, false)
            .map_err(|error| error.to_string())?;
        let info = repository
            .read_layer_info(&reference, "binary")
            .map_err(|error| error.to_string())?;
        if info.kind != "app" {
            return Err(format!("{reference} is not an app"));
        }
        vec![(reference, info)]
    } else {
        let mut seen = HashSet::new();
        repository
            .list_layer_items()
            .into_iter()
            .filter(|item| item.info.kind == "app" && item.info.module == "binary")
            .filter_map(|item| {
                let reference = linyaps_repository::reference_from_info(&item.info).ok()?;
                seen.insert(reference.to_string())
                    .then_some((reference, item.info))
            })
            .collect::<Vec<_>>()
    };
    applications.sort_by_key(|application| application.0.to_string());

    let mut trees = Vec::new();
    for (app_reference, app_info) in applications {
        let base = resolve_dependency(&repository, &app_info.base, &app_info.channel)?;
        let base_info = repository
            .read_layer_info(&base, "binary")
            .map_err(|error| error.to_string())?;
        let base_index = ensure_node(&mut trees, &base.to_string(), &base_info.kind);
        append_extensions(&repository, &mut trees[base_index].children, &base_info);

        if let Some(runtime_raw) = app_info
            .runtime
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            let runtime = resolve_dependency(&repository, runtime_raw, &app_info.channel)?;
            let runtime_info = repository
                .read_layer_info(&runtime, "binary")
                .map_err(|error| error.to_string())?;
            let runtime_index = ensure_node(
                &mut trees[base_index].children,
                &runtime.to_string(),
                &runtime_info.kind,
            );
            append_extensions(
                &repository,
                &mut trees[base_index].children[runtime_index].children,
                &runtime_info,
            );
            let app_index = ensure_node(
                &mut trees[base_index].children[runtime_index].children,
                &app_reference.to_string(),
                &app_info.kind,
            );
            append_extensions(
                &repository,
                &mut trees[base_index].children[runtime_index].children[app_index].children,
                &app_info,
            );
        } else {
            let app_index = ensure_node(
                &mut trees[base_index].children,
                &app_reference.to_string(),
                &app_info.kind,
            );
            append_extensions(
                &repository,
                &mut trees[base_index].children[app_index].children,
                &app_info,
            );
        }
    }
    sort_depends(&mut trees);
    print_depends(&trees, json_output)
}

fn resolve_dependency(
    repository: &LocalRepository,
    raw: &str,
    channel: &str,
) -> Result<Reference, String> {
    let mut fuzzy = raw
        .parse::<FuzzyReference>()
        .map_err(|error| error.to_string())?;
    if fuzzy.channel.is_none() {
        fuzzy.channel = Some(channel.to_string());
    }
    if fuzzy.architecture.is_none() {
        fuzzy.architecture = Some(Architecture::current().map_err(|error| error.to_string())?);
    }
    repository
        .resolve_local(&fuzzy, true)
        .map_err(|error| error.to_string())
}

fn append_extensions(
    repository: &LocalRepository,
    nodes: &mut Vec<DependsNode>,
    target: &PackageInfoV2,
) {
    for definition in target.extensions.as_deref().into_iter().flatten() {
        let mut name = definition.name.clone();
        let version = (!definition.version.is_empty()).then(|| definition.version.clone());
        if name == "org.deepin.driver.display.nvidia" {
            let Ok(driver_version) = fs::read_to_string("/sys/module/nvidia/version") else {
                continue;
            };
            let driver_version = driver_version.trim().replace('.', "-");
            if driver_version.is_empty() {
                continue;
            }
            name.push('.');
            name.push_str(&driver_version);
        }
        let Ok(fuzzy) = FuzzyReference::new(
            Some(target.channel.clone()),
            name,
            version,
            Architecture::current().ok(),
        ) else {
            continue;
        };
        let Ok(reference) = repository.resolve_local(&fuzzy, true) else {
            continue;
        };
        let Ok(info) = repository.read_layer_info(&reference, "binary") else {
            continue;
        };
        if info.kind == "extension" {
            ensure_node(nodes, &reference.to_string(), &info.kind);
        }
    }
}

fn ensure_node(nodes: &mut Vec<DependsNode>, reference: &str, kind: &str) -> usize {
    if let Some(index) = nodes.iter().position(|node| node.reference == reference) {
        if nodes[index].kind.is_empty() {
            nodes[index].kind = kind.to_string();
        }
        return index;
    }
    nodes.push(DependsNode {
        reference: reference.to_string(),
        kind: kind.to_string(),
        children: Vec::new(),
    });
    nodes.len() - 1
}

fn sort_depends(nodes: &mut [DependsNode]) {
    nodes.sort_by(|left, right| {
        kind_rank(&left.kind)
            .cmp(&kind_rank(&right.kind))
            .then_with(|| left.reference.cmp(&right.reference))
    });
    for node in nodes {
        sort_depends(&mut node.children);
    }
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "base" => 0,
        "runtime" => 1,
        "app" => 2,
        "extension" => 3,
        _ => 4,
    }
}

fn print_depends(trees: &[DependsNode], json_output: bool) -> Result<(), String> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&trees.iter().map(depends_json).collect::<Vec<_>>())
                .map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    for (index, tree) in trees.iter().enumerate() {
        print_depends_node(tree, "");
        if index + 1 != trees.len() {
            println!();
        }
    }
    Ok(())
}

fn depends_json(node: &DependsNode) -> Value {
    json!({
        "ref": node.reference,
        "kind": node.kind,
        "children": node.children.iter().map(depends_json).collect::<Vec<_>>(),
    })
}

fn print_depends_node(node: &DependsNode, prefix: &str) {
    if node.kind.is_empty() {
        println!("{}", node.reference);
    } else {
        println!("{} ({})", node.reference, node.kind);
    }
    for (index, child) in node.children.iter().enumerate() {
        let last = index + 1 == node.children.len();
        print!("{prefix}{}", if last { "└── " } else { "├── " });
        print_depends_node(
            child,
            &format!("{prefix}{}", if last { "    " } else { "│   " }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_binary_sizes_like_upstream() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1536), "1.5 KiB");
    }

    #[test]
    fn dependency_sort_uses_kind_then_reference() {
        let mut nodes = vec![
            DependsNode {
                reference: "z".to_string(),
                kind: "app".to_string(),
                children: Vec::new(),
            },
            DependsNode {
                reference: "a".to_string(),
                kind: "base".to_string(),
                children: Vec::new(),
            },
        ];
        sort_depends(&mut nodes);
        assert_eq!(nodes[0].kind, "base");
    }
}
