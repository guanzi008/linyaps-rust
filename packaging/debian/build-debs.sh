#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$project_root"

require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$1" >&2
        exit 1
    }
}

detect_architecture() {
    if command -v dpkg >/dev/null 2>&1; then
        dpkg --print-architecture
        return
    fi

    case "$(uname -m)" in
        x86_64) printf '%s\n' amd64 ;;
        aarch64) printf '%s\n' arm64 ;;
        riscv64) printf '%s\n' riscv64 ;;
        loongarch64) printf '%s\n' loong64 ;;
        *)
            printf 'unsupported Debian architecture for %s\n' "$(uname -m)" >&2
            exit 1
            ;;
    esac
}

musl_target_for_architecture() {
    case "$1" in
        amd64) printf '%s\n' x86_64-unknown-linux-musl ;;
        arm64) printf '%s\n' aarch64-unknown-linux-musl ;;
        riscv64) printf '%s\n' riscv64gc-unknown-linux-musl ;;
        loong64) printf '%s\n' loongarch64-unknown-linux-musl ;;
        *)
            printf 'no supported static ll-init target for Debian architecture %s\n' "$1" >&2
            exit 1
            ;;
    esac
}

workspace_version=$(awk '
    /^\[workspace.package\]$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version = / {
        gsub(/^version = "/, "")
        gsub(/"$/, "")
        print
        exit
    }
' "$project_root/Cargo.toml")

if [[ -z "$workspace_version" ]]; then
    printf 'failed to read workspace version from Cargo.toml\n' >&2
    exit 1
fi

deb_version=${DEB_VERSION:-${workspace_version}~rust1-1}
artifact_version=${deb_version//\~/.}
deb_arch=${DEB_ARCH:-$(detect_architecture)}
native_arch=$(detect_architecture)
output_dir=${OUTPUT_DIR:-$project_root/dist}
maintainer=${DEB_MAINTAINER:-guanzi008 <20619190+guanzi008@users.noreply.github.com>}
box_min_version=${LINGLONG_BOX_MIN_VERSION:-2.3.0~rust1}
source_date_epoch=${SOURCE_DATE_EPOCH:-}
ll_init_target=${LL_INIT_TARGET:-$(musl_target_for_architecture "$deb_arch")}

if [[ -z "$source_date_epoch" ]]; then
    require_command git
    source_date_epoch=$(git log -1 --format=%ct)
fi

export SOURCE_DATE_EPOCH=$source_date_epoch

if [[ "$deb_arch" != "$native_arch" && ${LINYAPS_DEB_ALLOW_CROSS:-0} != 1 ]]; then
    printf 'DEB_ARCH=%s does not match native architecture %s\n' "$deb_arch" "$native_arch" >&2
    exit 1
fi

for command in cargo dpkg dpkg-deb dpkg-shlibdeps readelf md5sum gzip; do
    require_command "$command"
done

dpkg --validate-version "$deb_version"
dpkg --validate-version "$box_min_version"

if [[ ${CARGO_TARGET_DIR:-} = /* ]]; then
    target_dir=$CARGO_TARGET_DIR
elif [[ -n ${CARGO_TARGET_DIR:-} ]]; then
    target_dir=$project_root/$CARGO_TARGET_DIR
else
    target_dir=$project_root/target
fi

if [[ ${LINYAPS_DEB_SKIP_BUILD:-0} != 1 ]]; then
    cargo build --manifest-path "$project_root/Cargo.toml" --workspace --release --locked
    cargo build --manifest-path "$project_root/Cargo.toml" \
        -p ll-init --release --locked --target "$ll_init_target"
    cp "$target_dir/$ll_init_target/release/ll-init" "$target_dir/release/ll-init"
fi

helper=$target_dir/release/ll-system-helper
if [[ ! -x "$helper" ]]; then
    printf 'release installer not found: %s\n' "$helper" >&2
    exit 1
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/linyaps-deb.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT

all_root=$work_dir/all
runtime_root=$work_dir/linglong-bin
builder_root=$work_dir/linglong-builder

"$helper" install \
    --destdir "$all_root" \
    --prefix /usr \
    --binary-dir "$target_dir/release"

copy_path() {
    local relative=$1
    local destination_root=$2
    local source=$all_root/$relative
    local destination=$destination_root/$relative

    if [[ ! -e "$source" && ! -L "$source" ]]; then
        printf 'installer output is missing required path: %s\n' "$relative" >&2
        exit 1
    fi
    mkdir -p "$(dirname "$destination")"
    cp -a "$source" "$destination"
}

runtime_paths=(
    etc/X11/Xsession.d/21linglong
    etc/profile.d/linglong.sh
    usr/bin/ll-cli
    usr/bin/llpkg
    usr/lib/linglong/container
    usr/lib/linglong/generate-xdg-data-dirs.sh
    usr/lib/systemd/system-environment-generators/61-linglong
    usr/lib/systemd/system-preset/91-linglong.preset
    usr/lib/systemd/system/org.deepin.linglong.PackageManager.service
    usr/lib/systemd/user-generators/linglong-user-systemd-generator
    usr/lib/sysusers.d/linglong.conf
    usr/lib/tmpfiles.d/linglong.conf
    usr/libexec/linglong/font-cache-generator
    usr/libexec/linglong/ld-cache-generator
    usr/libexec/linglong/ll-driver-detect
    usr/libexec/linglong/ll-init
    usr/libexec/linglong/ll-package-manager
    usr/libexec/linglong/ll-system-helper
    usr/share/applications/linyaps.desktop
    usr/share/bash-completion/completions/ll-cli
    usr/share/dbus-1/system-services/org.deepin.linglong.PackageManager1.service
    usr/share/dbus-1/system.d/org.deepin.linglong.PackageManager1.conf
    usr/share/fish/vendor_completions.d/ll-cli.fish
    usr/share/icons/hicolor/scalable/apps/linyaps.svg
    usr/share/linglong/config.yaml
    usr/share/linglong/export-dirs.json
    usr/share/locale
    usr/share/mime/packages/vnd.linyaps.uab.xml
    usr/share/polkit-1/actions/org.deepin.linglong.PackageManager1.policy
    usr/share/zsh/vendor-completions/_ll-cli
)

builder_paths=(
    usr/bin/ll-builder
    usr/bin/ll-builder-export
    usr/lib/linglong/builder
    usr/libexec/linglong/app-conf-generator
    usr/libexec/linglong/builder
    usr/libexec/linglong/fetch-archive-source
    usr/libexec/linglong/fetch-dsc-source
    usr/libexec/linglong/fetch-file-source
    usr/libexec/linglong/fetch-git-source
    usr/share/bash-completion/completions/ll-builder
    usr/share/fish/vendor_completions.d/ll-builder.fish
    usr/share/linglong/builder
    usr/share/zsh/vendor-completions/_ll-builder
)

for relative in "${runtime_paths[@]}"; do
    copy_path "$relative" "$runtime_root"
done
for relative in "${builder_paths[@]}"; do
    copy_path "$relative" "$builder_root"
done

find "$all_root" \( -type f -o -type l \) -printf '%P\n' | LC_ALL=C sort \
    >"$work_dir/all-files"
{
    find "$runtime_root" \( -type f -o -type l \) -printf '%P\n'
    find "$builder_root" \( -type f -o -type l \) -printf '%P\n'
} | LC_ALL=C sort >"$work_dir/package-files"

if ! cmp -s "$work_dir/all-files" "$work_dir/package-files"; then
    printf 'Debian package split does not cover the complete install layout:\n' >&2
    diff -u "$work_dir/all-files" "$work_dir/package-files" >&2 || true
    exit 1
fi

install -Dm644 "$project_root/packaging/debian/linglong.conf" \
    "$runtime_root/usr/lib/sysctl.d/linglong.conf"

scan_shlibs() {
    local package_name=$1
    local package_root=$2
    local scan_dir=$work_dir/shlibdeps-$package_name
    local shlib_output
    local -a shlib_arguments=()

    mkdir -p "$scan_dir/debian"
    cat >"$scan_dir/debian/control" <<EOF
Source: $package_name
Section: admin
Priority: optional
Maintainer: $maintainer
Standards-Version: 4.6.2

Package: $package_name
Architecture: any
Description: dependency scan package
EOF

    while IFS= read -r -d '' executable; do
        if readelf -h "$executable" >/dev/null 2>&1 \
            && readelf -d "$executable" 2>/dev/null | grep -q '(NEEDED)'; then
            shlib_arguments+=("-e$executable")
        fi
    done < <(find "$package_root" -type f -perm /111 -print0)

    if ((${#shlib_arguments[@]} == 0)); then
        return
    fi

    shlib_output=$(cd "$scan_dir" && dpkg-shlibdeps --warnings=0 -O "${shlib_arguments[@]}")
    printf '%s\n' "${shlib_output#shlibs:Depends=}"
}

prepare_documentation() {
    local package_name=$1
    local package_root=$2
    local summary=$3
    local doc_root=$package_root/usr/share/doc/$package_name
    local changelog_date

    mkdir -p "$doc_root"
    cat >"$doc_root/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: linyaps-rust
Source: https://github.com/guanzi008/linyaps-rust

Files: *
Copyright: 2018-2026 UnionTech Software Technology Co., Ltd.
           2026 linyaps-rust contributors
License: LGPL-3.0-or-later
 On Debian systems, the complete text of the GNU Lesser General Public
 License version 3 can be found in /usr/share/common-licenses/LGPL-3.
EOF
    cp -a "$project_root/LICENSES" "$doc_root/licenses"
    changelog_date=$(date -u --date="@$source_date_epoch" --rfc-email)
    cat >"$work_dir/$package_name.changelog.Debian" <<EOF
$package_name ($deb_version) unstable; urgency=medium

  * $summary

 -- $maintainer  $changelog_date
EOF
    gzip -n -9 -c "$work_dir/$package_name.changelog.Debian" \
        >"$doc_root/changelog.Debian.gz"
}

write_md5sums() {
    local package_root=$1
    (
        cd "$package_root"
        find . -path ./DEBIAN -prune -o -type f -print0 \
            | LC_ALL=C sort -z \
            | while IFS= read -r -d '' file; do md5sum "$file"; done \
            | sed 's#  \./#  #' >DEBIAN/md5sums
    )
}

prepare_documentation linglong-bin "$runtime_root" \
    'Publish the complete Rust rewrite of the Linyaps package manager.'
prepare_documentation linglong-builder "$builder_root" \
    'Publish the complete Rust rewrite of the Linyaps application builder.'

mkdir -p "$runtime_root/usr/share/lintian/overrides" \
    "$builder_root/usr/share/lintian/overrides"
cat >"$runtime_root/usr/share/lintian/overrides/linglong-bin" <<'EOF'
linglong-bin: embedded-library libyaml *
linglong-bin: shared-library-lacks-prerequisites [usr/libexec/linglong/ll-init]
EOF
cat >"$builder_root/usr/share/lintian/overrides/linglong-builder" <<'EOF'
linglong-builder: embedded-library libyaml *
EOF

runtime_shlibs=$(scan_shlibs linglong-bin "$runtime_root")
builder_shlibs=$(scan_shlibs linglong-builder "$builder_root")
runtime_size=$(du -sk "$runtime_root/etc" "$runtime_root/usr" | awk '{ total += $1 } END { print total }')
builder_size=$(du -sk "$builder_root/usr" | awk '{ print $1 }')

mkdir -p "$runtime_root/DEBIAN" "$builder_root/DEBIAN"
cat >"$runtime_root/DEBIAN/control" <<EOF
Package: linglong-bin
Version: $deb_version
Architecture: $deb_arch
Maintainer: $maintainer
Installed-Size: $runtime_size
Depends: desktop-file-utils, libglib2.0-bin, linglong-box (>= $box_min_version) | crun, shared-mime-info, pkexec | policykit-1, erofs-utils, init-system-helpers (>= 1.52), systemd | systemd-standalone-sysusers | systemd-sysusers, $runtime_shlibs
Recommends: erofsfuse
Section: admin
Priority: optional
Homepage: https://github.com/guanzi008/linyaps-rust
Description: Linyaps package manager implemented in Rust
 A command-compatible Rust rewrite of the Linyaps package manager.
EOF

cat >"$builder_root/DEBIAN/control" <<EOF
Package: linglong-builder
Version: $deb_version
Architecture: $deb_arch
Maintainer: $maintainer
Installed-Size: $builder_size
Depends: erofs-utils, fuse-overlayfs, git, linglong-bin (= $deb_version), linglong-box (>= $box_min_version) | crun, uidmap, $builder_shlibs
Recommends: linglong-loader
Suggests: devscripts
Section: admin
Priority: optional
Homepage: https://github.com/guanzi008/linyaps-rust
Description: Linyaps application builder implemented in Rust
 Tools for building, exporting and publishing Linyaps applications.
EOF

install -m755 "$project_root/packaging/debian/linglong-bin.postinst" \
    "$runtime_root/DEBIAN/postinst"
install -m755 "$project_root/packaging/debian/linglong-bin.prerm" \
    "$runtime_root/DEBIAN/prerm"
install -m755 "$project_root/packaging/debian/linglong-bin.postrm" \
    "$runtime_root/DEBIAN/postrm"
cat >"$runtime_root/DEBIAN/conffiles" <<'EOF'
/etc/X11/Xsession.d/21linglong
/etc/profile.d/linglong.sh
EOF

write_md5sums "$runtime_root"
write_md5sums "$builder_root"

find "$runtime_root" "$builder_root" -print0 \
    | xargs -0 touch --no-dereference --date="@$source_date_epoch"

mkdir -p "$output_dir"
runtime_artifact=$output_dir/linglong-bin_${artifact_version}_${deb_arch}.deb
builder_artifact=$output_dir/linglong-builder_${artifact_version}_${deb_arch}.deb

dpkg-deb --root-owner-group --uniform-compression -Zxz -z9 \
    --build "$runtime_root" "$runtime_artifact"
dpkg-deb --root-owner-group --uniform-compression -Zxz -z9 \
    --build "$builder_root" "$builder_artifact"

(
    cd "$output_dir"
    sha256sum "$(basename "$runtime_artifact")" "$(basename "$builder_artifact")" \
        >SHA256SUMS
)

dpkg-deb --info "$runtime_artifact" >/dev/null
dpkg-deb --info "$builder_artifact" >/dev/null
printf '%s\n%s\n' "$runtime_artifact" "$builder_artifact"
