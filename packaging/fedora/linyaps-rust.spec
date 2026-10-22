%global debug_package %{nil}
%global _build_id_links none

Name:           linyaps-rust
Version:        1.15.0
Release:        1.rust1%{?dist}
Summary:        Rust implementation of the Linyaps application manager
License:        LGPL-3.0-or-later
URL:            https://github.com/guanzi008/linyaps-rust
Source0:        %{name}-%{version}.tar.xz
ExclusiveArch:  x86_64
BuildRequires:  cargo
BuildRequires:  rust >= 1.92
BuildRequires:  gcc
BuildRequires:  glibc-static
BuildRequires:  systemd-rpm-macros
BuildRequires:  desktop-file-utils
Requires:       linyaps-box-rust >= 2.4.0
Requires:       dbus
Requires:       desktop-file-utils
Requires:       erofs-utils
Requires:       fuse-overlayfs
Requires:       hicolor-icon-theme
Requires:       polkit
Requires:       shared-mime-info
Requires:       systemd
Requires(post): systemd
Provides:       linyaps = %{version}-%{release}
Conflicts:      linyaps
Conflicts:      linglong-bin
%{?systemd_requires}

%description
Command-compatible Rust implementation of the Linyaps application manager,
including the command-line client, system D-Bus service, application runtime
integration and host NVIDIA driver fallback. This is a third-party build,
not an official Fedora package or a claim of complete upstream parity.

%package builder
Summary:        Build and export Linyaps applications
Requires:       %{name}%{?_isa} = %{version}-%{release}
Requires:       git
Requires:       shadow-utils
Conflicts:      linglong-builder

%description builder
Rust build and export tools for Linyaps applications, including source
fetchers, build helpers and UAB templates.

%prep
%autosetup -p1

%build
export RUSTFLAGS="${RUSTFLAGS:-} -Cdebuginfo=0 -Cstrip=symbols"
cargo build --release --locked --offline --workspace --exclude ll-init
cargo rustc --release --locked --offline -p ll-init -- -C target-feature=+crt-static

%install
target_dir="${CARGO_TARGET_DIR:-target}/release"
"$target_dir/ll-system-helper" install --destdir "%{buildroot}" --prefix /usr --binary-dir "$target_dir"
for helper in \
    "%{buildroot}%{_prefix}/lib/systemd/system-environment-generators/61-linglong" \
    "%{buildroot}%{_prefix}/lib/systemd/user-generators/linglong-user-systemd-generator" \
    "%{buildroot}%{_libexecdir}/linglong/font-cache-generator" \
    "%{buildroot}%{_libexecdir}/linglong/ld-cache-generator" \
    "%{buildroot}%{_libexecdir}/linglong/app-conf-generator"; do
    ln -srf "%{buildroot}%{_libexecdir}/linglong/ll-system-helper" "$helper"
done
for helper in fetch-archive-source fetch-dsc-source fetch-file-source fetch-git-source \
    builder/helper/config-check.sh builder/helper/ldd-check.sh \
    builder/helper/main-check.sh builder/helper/symbols-strip.sh; do
    ln -srf "%{buildroot}%{_bindir}/ll-builder" "%{buildroot}%{_libexecdir}/linglong/$helper"
done
install -d "%{buildroot}%{_sysconfdir}/X11/xinit/xinitrc.d"
{
    printf '#!/bin/sh\n'
    cat "%{buildroot}%{_sysconfdir}/X11/Xsession.d/21linglong"
} > "%{buildroot}%{_sysconfdir}/X11/xinit/xinitrc.d/50-linglong.sh"
chmod 755 "%{buildroot}%{_sysconfdir}/X11/xinit/xinitrc.d/50-linglong.sh"
rm "%{buildroot}%{_sysconfdir}/X11/Xsession.d/21linglong"
rmdir "%{buildroot}%{_sysconfdir}/X11/Xsession.d"
mv "%{buildroot}%{_datadir}/zsh/vendor-completions" "%{buildroot}%{_datadir}/zsh/site-functions"
sed -i '1{/^#!/d;}' "%{buildroot}%{_datadir}/bash-completion/completions/ll-cli" "%{buildroot}%{_datadir}/bash-completion/completions/ll-builder"
install -d "%{buildroot}%{_mandir}/man1"
install -m644 debian/*.1 "%{buildroot}%{_mandir}/man1/"
desktop-file-validate "%{buildroot}%{_datadir}/applications/linyaps.desktop"
%find_lang linyaps

%check
export RUSTFLAGS="${RUSTFLAGS:-} -Cdebuginfo=0 -Cstrip=symbols"
target_dir="${CARGO_TARGET_DIR:-target}/release"
"$target_dir/ll-cli" --version
"$target_dir/ll-builder" --version
if readelf -l "$target_dir/ll-init" | grep -q INTERP; then
    exit 1
fi
if readelf -d "$target_dir/ll-init" | grep -q NEEDED; then
    exit 1
fi
result=0
"$target_dir/ll-init" /bin/sh -c 'exit 37' || result=$?
test "$result" = 37
cargo test --release --locked --offline -p ll-system-helper

%post
/usr/bin/systemd-sysusers %{_sysusersdir}/linglong.conf
/usr/bin/systemd-tmpfiles --create %{_tmpfilesdir}/linglong.conf
%systemd_post org.deepin.linglong.PackageManager.service

%preun
%systemd_preun org.deepin.linglong.PackageManager.service

%postun
%systemd_postun_with_restart org.deepin.linglong.PackageManager.service

%files -f linyaps.lang
%license LICENSE LICENSES
%doc README.md COMPATIBILITY.md
%config(noreplace) %{_sysconfdir}/profile.d/linglong.sh
%{_sysconfdir}/X11/xinit/xinitrc.d/50-linglong.sh
%{_bindir}/ll-cli
%{_bindir}/llpkg
%dir %{_prefix}/lib/linglong
%{_prefix}/lib/linglong/container/
%{_prefix}/lib/linglong/generate-xdg-data-dirs.sh
%{_unitdir}/org.deepin.linglong.PackageManager.service
%{_prefix}/lib/systemd/system-preset/91-linglong.preset
%{_prefix}/lib/systemd/system-environment-generators/61-linglong
%{_prefix}/lib/systemd/user-generators/linglong-user-systemd-generator
%{_sysusersdir}/linglong.conf
%{_tmpfilesdir}/linglong.conf
%dir %{_libexecdir}/linglong
%{_libexecdir}/linglong/ll-package-manager
%{_libexecdir}/linglong/ll-driver-detect
%{_libexecdir}/linglong/ll-init
%{_libexecdir}/linglong/ll-system-helper
%{_libexecdir}/linglong/font-cache-generator
%{_libexecdir}/linglong/ld-cache-generator
%{_datadir}/applications/linyaps.desktop
%{_datadir}/dbus-1/system-services/org.deepin.linglong.PackageManager1.service
%{_datadir}/dbus-1/system.d/org.deepin.linglong.PackageManager1.conf
%{_datadir}/polkit-1/actions/org.deepin.linglong.PackageManager1.policy
%{_datadir}/icons/hicolor/scalable/apps/linyaps.svg
%dir %{_datadir}/linglong
%{_datadir}/linglong/config.yaml
%{_datadir}/linglong/export-dirs.json
%{_datadir}/mime/packages/vnd.linyaps.uab.xml
%{_datadir}/bash-completion/completions/ll-cli
%{_datadir}/fish/vendor_completions.d/ll-cli.fish
%{_datadir}/zsh/site-functions/_ll-cli
%{_mandir}/man1/ll-cli.1*
%{_mandir}/man1/llpkg.1*
%ghost %dir %attr(0755,deepin-linglong,deepin-linglong) /var/lib/linglong
%ghost %dir %attr(1777,deepin-linglong,deepin-linglong) /run/linglong
%ghost %attr(0644,deepin-linglong,deepin-linglong) /run/linglong/lock
%ghost %dir %attr(0755,root,root) %{_sysconfdir}/linglong
%ghost %dir %attr(0755,root,root) %{_sysconfdir}/linglong/config.d

%files builder
%{_bindir}/ll-builder
%{_bindir}/ll-builder-export
%{_prefix}/lib/linglong/builder/
%{_libexecdir}/linglong/builder/
%{_libexecdir}/linglong/app-conf-generator
%{_libexecdir}/linglong/fetch-archive-source
%{_libexecdir}/linglong/fetch-dsc-source
%{_libexecdir}/linglong/fetch-file-source
%{_libexecdir}/linglong/fetch-git-source
%{_datadir}/linglong/builder/
%{_datadir}/bash-completion/completions/ll-builder
%{_datadir}/fish/vendor_completions.d/ll-builder.fish
%{_datadir}/zsh/site-functions/_ll-builder
%{_mandir}/man1/ll-builder.1*
%{_mandir}/man1/ll-builder-export.1*

%changelog
* Thu Oct 22 2026 guanzi008 <20619190+guanzi008@users.noreply.github.com> - 1.15.0-1.rust1
- Package the Rust NVIDIA branch for Fedora, with native session integration.
