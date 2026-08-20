use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use goblin::Object;

#[derive(Debug, Args)]
pub struct InstallOptions {
    #[arg(long, default_value = "/")]
    destdir: PathBuf,
    #[arg(long, default_value = "/usr")]
    prefix: PathBuf,
    #[arg(long)]
    binary_dir: Option<PathBuf>,
    #[arg(long)]
    ll_box: Option<PathBuf>,
    #[arg(long)]
    skip_binaries: bool,
}

struct Asset {
    destination: &'static str,
    content: &'static [u8],
    mode: u32,
}

macro_rules! asset_prefix {
    ($destination:literal, $source:literal) => {
        Asset {
            destination: $destination,
            content: include_bytes!($source),
            mode: 0o644,
        }
    };
}

const ASSETS: &[Asset] = &[
    asset_prefix!(
        "lib/systemd/system/org.deepin.linglong.PackageManager.service",
        "../../../misc/lib/systemd/system/org.deepin.linglong.PackageManager.service"
    ),
    asset_prefix!(
        "lib/systemd/system-preset/91-linglong.preset",
        "../../../misc/lib/systemd/system-preset/91-linglong.preset"
    ),
    asset_prefix!(
        "lib/sysusers.d/linglong.conf",
        "../../../misc/lib/sysusers.d/linglong.conf"
    ),
    asset_prefix!(
        "lib/tmpfiles.d/linglong.conf",
        "../../../misc/lib/tmpfiles.d/linglong.conf"
    ),
    asset_prefix!(
        "lib/linglong/container/README.md",
        "../../../misc/lib/linglong/container/README.md"
    ),
    asset_prefix!(
        "share/dbus-1/system-services/org.deepin.linglong.PackageManager1.service",
        "../../../misc/share/dbus-1/system-services/org.deepin.linglong.PackageManager1.service"
    ),
    asset_prefix!(
        "share/dbus-1/system.d/org.deepin.linglong.PackageManager1.conf",
        "../../../misc/share/dbus-1/system.d/org.deepin.linglong.PackageManager1.conf"
    ),
    asset_prefix!(
        "share/polkit-1/actions/org.deepin.linglong.PackageManager1.policy",
        "../../../misc/share/polkit-1/actions/org.deepin.linglong.PackageManager1.policy"
    ),
    asset_prefix!(
        "share/linglong/config.yaml",
        "../../../misc/share/linglong/config.yaml"
    ),
    asset_prefix!(
        "share/linglong/export-dirs.json",
        "../../../misc/share/linglong/export-dirs.json"
    ),
    asset_prefix!(
        "share/linglong/builder/uab/blacklist",
        "../../../misc/share/linglong/builder/uab/blacklist"
    ),
    asset_prefix!(
        "share/linglong/builder/templates/example.yaml",
        "../../../misc/share/linglong/builder/templates/example.yaml"
    ),
    asset_prefix!(
        "share/mime/packages/vnd.linyaps.uab.xml",
        "../../../misc/share/mime/packages/vnd.linyaps.uab.xml"
    ),
    asset_prefix!(
        "share/applications/linyaps.desktop",
        "../../../misc/share/applications/linyaps.desktop"
    ),
    asset_prefix!(
        "share/icons/hicolor/scalable/apps/linyaps.svg",
        "../../../misc/share/icons/hicolor/scalable/apps/linyaps.svg"
    ),
    asset_prefix!(
        "share/bash-completion/completions/ll-builder",
        "../../../misc/share/bash-completion/completions/ll-builder"
    ),
    asset_prefix!(
        "share/bash-completion/completions/ll-cli",
        "../../../misc/share/bash-completion/completions/ll-cli"
    ),
    asset_prefix!(
        "share/zsh/vendor-completions/_ll-builder",
        "../../../misc/share/zsh/vendor-completions/_ll-builder"
    ),
    asset_prefix!(
        "share/zsh/vendor-completions/_ll-cli",
        "../../../misc/share/zsh/vendor-completions/_ll-cli"
    ),
    asset_prefix!(
        "share/fish/vendor_completions.d/ll-builder.fish",
        "../../../misc/share/fish/vendor_completions.d/ll-builder.fish"
    ),
    asset_prefix!(
        "share/fish/vendor_completions.d/ll-cli.fish",
        "../../../misc/share/fish/vendor_completions.d/ll-cli.fish"
    ),
];

pub fn run(options: InstallOptions) -> Result<()> {
    let helper = env::current_exe().context("failed to locate ll-system-helper")?;
    let binary_dir = options
        .binary_dir
        .or_else(|| helper.parent().map(Path::to_path_buf))
        .context("failed to locate binary directory")?;
    if !options.skip_binaries {
        validate_static_init(&binary_dir.join("ll-init"))?;
    }
    install_layout(
        &options.destdir,
        &options.prefix,
        &binary_dir,
        &helper,
        options.ll_box.as_deref(),
        options.skip_binaries,
    )
}

fn validate_static_init(path: &Path) -> Result<()> {
    let content = fs::read(path)
        .with_context(|| format!("required executable is missing: {}", path.display()))?;
    let Object::Elf(elf) = Object::parse(&content)
        .with_context(|| format!("failed to parse ll-init ELF executable: {}", path.display()))?
    else {
        bail!("ll-init must be a Linux ELF executable: {}", path.display());
    };
    if elf.interpreter.is_some() || !elf.libraries.is_empty() {
        bail!(
            "ll-init must be statically linked; build it for a static target before installation: {}",
            path.display()
        );
    }
    Ok(())
}

fn install_layout(
    destdir: &Path,
    prefix: &Path,
    binary_dir: &Path,
    helper: &Path,
    ll_box: Option<&Path>,
    skip_binaries: bool,
) -> Result<()> {
    if !prefix.is_absolute() {
        bail!("installation prefix must be absolute");
    }
    fs::create_dir_all(destdir)?;
    for asset in ASSETS {
        let path = destination(destdir, &prefix.join(asset.destination))?;
        write_file(&path, asset.content, asset.mode)?;
    }
    install_locale_catalogs(destdir, prefix)?;
    install_xdg_shell_integration(destdir, prefix)?;

    let helper_destination =
        destination(destdir, &prefix.join("libexec/linglong/ll-system-helper"))?;
    copy_executable(helper, &helper_destination)?;
    for relative in [
        "lib/systemd/system-environment-generators/61-linglong",
        "lib/systemd/user-generators/linglong-user-systemd-generator",
        "libexec/linglong/font-cache-generator",
        "libexec/linglong/ld-cache-generator",
        "libexec/linglong/app-conf-generator",
    ] {
        copy_executable(helper, &destination(destdir, &prefix.join(relative))?)?;
    }

    if !skip_binaries {
        for (name, relative) in [
            ("ll-cli", "bin/ll-cli"),
            ("llpkg", "bin/llpkg"),
            ("ll-builder", "bin/ll-builder"),
            ("ll-builder-export", "bin/ll-builder-export"),
            ("ll-driver-detect", "libexec/linglong/ll-driver-detect"),
            ("ll-init", "libexec/linglong/ll-init"),
            ("ll-package-manager", "libexec/linglong/ll-package-manager"),
            ("uab-header", "lib/linglong/builder/uab/uab-header"),
            ("uab-loader", "lib/linglong/builder/uab/uab-loader"),
        ] {
            copy_executable(
                &binary_dir.join(name),
                &destination(destdir, &prefix.join(relative))?,
            )?;
        }
        for relative in [
            "libexec/linglong/fetch-archive-source",
            "libexec/linglong/fetch-dsc-source",
            "libexec/linglong/fetch-file-source",
            "libexec/linglong/fetch-git-source",
            "libexec/linglong/builder/helper/config-check.sh",
            "libexec/linglong/builder/helper/ldd-check.sh",
            "libexec/linglong/builder/helper/main-check.sh",
            "libexec/linglong/builder/helper/symbols-strip.sh",
        ] {
            copy_executable(
                &binary_dir.join("ll-builder"),
                &destination(destdir, &prefix.join(relative))?,
            )?;
        }
        let automatic_box = binary_dir.join("ll-box");
        let box_source =
            ll_box.or_else(|| automatic_box.is_file().then_some(automatic_box.as_path()));
        if let Some(box_source) = box_source {
            copy_executable(
                box_source,
                &destination(destdir, &prefix.join("bin/ll-box"))?,
            )?;
        }
    }
    Ok(())
}

fn install_locale_catalogs(destdir: &Path, prefix: &Path) -> Result<()> {
    for language in linyaps_i18n::LANGUAGES {
        let catalog = prefix
            .join("share/locale")
            .join(language)
            .join("LC_MESSAGES/linyaps.mo");
        write_file(
            &destination(destdir, &catalog)?,
            &linyaps_i18n::mo_bytes(language),
            0o644,
        )?;
    }
    Ok(())
}

fn install_xdg_shell_integration(destdir: &Path, prefix: &Path) -> Result<()> {
    let helper = shell_quote(&prefix.join("libexec/linglong/ll-system-helper"));
    let generator = format!(
        "#!/bin/sh\nif [ -x {helper} ]; then\n    XDG_DATA_DIRS=\"$({helper} xdg-value)\"\nfi\n"
    );
    let generator_path = prefix.join("lib/linglong/generate-xdg-data-dirs.sh");
    write_file(
        &destination(destdir, &generator_path)?,
        generator.as_bytes(),
        0o755,
    )?;

    let generator = shell_quote(&generator_path);
    let profile = format!(
        "if [ -r {generator} ]; then\n    . {generator}\n    [ -n \"${{XDG_DATA_DIRS}}\" ] && export XDG_DATA_DIRS\nfi\n"
    );
    for path in [
        Path::new("/etc/profile.d/linglong.sh"),
        Path::new("/etc/X11/Xsession.d/21linglong"),
    ] {
        write_file(&destination(destdir, path)?, profile.as_bytes(), 0o644)?;
    }
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn destination(destdir: &Path, absolute: &Path) -> Result<PathBuf> {
    let relative = absolute
        .strip_prefix("/")
        .with_context(|| format!("installation path must be absolute: {}", absolute.display()))?;
    Ok(destdir.join(relative))
}

fn write_file(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn copy_executable(source: &Path, destination: &Path) -> Result<()> {
    let content = fs::read(source)
        .with_context(|| format!("required executable is missing: {}", source.display()))?;
    write_file(destination, &content, 0o755)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn installs_complete_layout_without_placeholders() {
        let temporary = tempdir().unwrap();
        let binaries = temporary.path().join("binaries");
        fs::create_dir_all(&binaries).unwrap();
        for name in [
            "ll-cli",
            "llpkg",
            "ll-builder",
            "ll-builder-export",
            "ll-driver-detect",
            "ll-init",
            "ll-package-manager",
            "uab-header",
            "uab-loader",
        ] {
            fs::write(binaries.join(name), name).unwrap();
        }
        let helper = temporary.path().join("helper");
        fs::write(&helper, "helper").unwrap();
        let root = temporary.path().join("root");
        install_layout(&root, Path::new("/usr"), &binaries, &helper, None, false).unwrap();
        let service = fs::read_to_string(
            root.join("usr/lib/systemd/system/org.deepin.linglong.PackageManager.service"),
        )
        .unwrap();
        assert!(service.contains("User=deepin-linglong"));
        assert!(service.contains("ExecStart=/usr/libexec/linglong/ll-package-manager"));
        assert!(!service.contains('@'));
        assert!(root.join("usr/bin/ll-cli").is_file());
        assert!(root.join("usr/bin/ll-builder-export").is_file());
        assert!(
            root.join("usr/lib/linglong/builder/uab/uab-header")
                .is_file()
        );
        assert!(
            root.join("usr/lib/linglong/builder/uab/uab-loader")
                .is_file()
        );
        assert!(!root.join("usr/libexec/linglong/uab-header").exists());
        for relative in [
            "usr/libexec/linglong/fetch-archive-source",
            "usr/libexec/linglong/fetch-dsc-source",
            "usr/libexec/linglong/fetch-file-source",
            "usr/libexec/linglong/fetch-git-source",
            "usr/libexec/linglong/builder/helper/config-check.sh",
            "usr/libexec/linglong/builder/helper/ldd-check.sh",
            "usr/libexec/linglong/builder/helper/main-check.sh",
            "usr/libexec/linglong/builder/helper/symbols-strip.sh",
        ] {
            assert!(root.join(relative).is_file(), "missing {relative}");
        }
        assert!(
            root.join("usr/lib/systemd/system-environment-generators/61-linglong")
                .is_file()
        );
        assert_eq!(
            fs::metadata(root.join("usr/libexec/linglong/app-conf-generator"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let profile = fs::read_to_string(root.join("etc/profile.d/linglong.sh")).unwrap();
        let xsession = fs::read_to_string(root.join("etc/X11/Xsession.d/21linglong")).unwrap();
        assert_eq!(profile, xsession);
        assert_eq!(
            profile,
            include_str!("../../../misc/etc/profile.d/linglong.sh")
        );
        assert_eq!(
            xsession,
            include_str!("../../../misc/etc/X11/Xsession.d/21linglong")
        );
        for language in ["de", "zh_CN", "zh_TW"] {
            let catalog = root
                .join("usr/share/locale")
                .join(language)
                .join("LC_MESSAGES/linyaps.mo");
            assert!(catalog.is_file(), "missing {}", catalog.display());
            assert_eq!(
                fs::metadata(catalog).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
        assert_eq!(
            fs::metadata(root.join("usr/lib/linglong/generate-xdg-data-dirs.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn xdg_shell_integration_uses_the_selected_prefix() {
        let temporary = tempdir().unwrap();
        install_xdg_shell_integration(temporary.path(), Path::new("/opt/linyaps")).unwrap();
        let generator = fs::read_to_string(
            temporary
                .path()
                .join("opt/linyaps/lib/linglong/generate-xdg-data-dirs.sh"),
        )
        .unwrap();
        assert!(generator.starts_with("#!/bin/sh\n"));
        assert!(generator.contains("'/opt/linyaps/libexec/linglong/ll-system-helper' xdg-value"));
        let profile =
            fs::read_to_string(temporary.path().join("etc/profile.d/linglong.sh")).unwrap();
        assert!(profile.contains("'/opt/linyaps/lib/linglong/generate-xdg-data-dirs.sh'"));
    }

    #[test]
    fn rejects_dynamic_ll_init() {
        let error = validate_static_init(Path::new("/proc/self/exe")).unwrap_err();
        assert!(error.to_string().contains("statically linked"));
    }

    #[test]
    fn rejects_non_elf_ll_init() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("ll-init");
        fs::write(&path, "not an executable").unwrap();
        let error = validate_static_init(&path).unwrap_err();
        assert!(error.to_string().contains("must be a Linux ELF executable"));
    }
}
