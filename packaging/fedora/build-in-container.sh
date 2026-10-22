#!/usr/bin/env bash
set -euo pipefail
mkdir -p "$HOME" /output/RPMS /output/SRPMS
build_package() {
    local name=$1
    local version=$2
    local input=$3
    local target=$4
    local root=/work/$name
    local source=$root/source/$name-$version
    mkdir -p "$source" "$root/rpm/SOURCES" "$root/rpm/SPECS"
    tar -xf "$input" -C "$source"
    (
        cd "$source"
        mkdir -p .cargo
        cargo vendor --locked --versioned-dirs cargo-vendor > .cargo/config.toml
    )
    cp "$source/packaging/fedora/$name.spec" "$root/rpm/SPECS/"
    XZ_OPT=-T2 tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
        -cJf "$root/rpm/SOURCES/$name-$version.tar.xz" -C "$root/source" "$name-$version"
    CARGO_TARGET_DIR="$target" rpmbuild -ba --define "_topdir $root/rpm" "$root/rpm/SPECS/$name.spec"
    cp "$root/rpm/RPMS/x86_64/"*.rpm /output/RPMS/
    cp "$root/rpm/SRPMS/"*.src.rpm /output/SRPMS/
}
build_package linyaps-box-rust "$BOX_VERSION" /input/box.tar /targets/box
build_package linyaps-rust "$MAIN_VERSION" /input/main.tar /targets/main
rpmlint -c "/work/linyaps-rust/source/linyaps-rust-$MAIN_VERSION/packaging/fedora/rpmlint.toml" /output/RPMS /output/SRPMS
{
    cat /etc/os-release
    printf '\nMain source: %s\nBox source: %s\n' "$MAIN_COMMIT" "$BOX_COMMIT"
    printf 'SOURCE_DATE_EPOCH: %s\n' "$SOURCE_DATE_EPOCH"
    rustc --version
    cargo --version
    rpm --version
    printf '\nSource snapshot hashes:\n'
    sha256sum /input/main.tar /input/box.tar
    printf '\nRPMs:\n'
    rpm -qp --queryformat '%{NAME} %{VERSION}-%{RELEASE} %{ARCH}\n' /output/RPMS/*.rpm
} > /output/MANIFEST.txt
cd /output
sha256sum RPMS/*.rpm SRPMS/*.rpm MANIFEST.txt > SHA256SUMS
