#!/usr/bin/env bash
set -euo pipefail
release=${1:-44}
if [[ ! $release =~ ^[0-9]+$ ]]; then
    printf 'Expected a numeric Fedora release\n' >&2
    exit 2
fi
if [[ $(uname -m) != x86_64 ]]; then
    printf 'This packaging workflow currently targets x86_64 only\n' >&2
    exit 2
fi
script_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
main_root=$(realpath "$script_dir/../..")
workspace=$(dirname "$main_root")
box_root=${LINYAPS_BOX_PROJECT:-$workspace/linyaps-box-rust}
output=$(realpath -m "${2:-$workspace/release/fedora$release-x86_64}")
cache=${LINYAPS_FEDORA_CACHE:-$HOME/.cache/linyaps-fedora}
image=linyaps-fedora-builder:$release
for command in docker git tar; do
    command -v "$command" >/dev/null
done
test -r "$box_root/packaging/fedora/linyaps-box-rust.spec"
main_version=$(awk '/^Version:/ {print $2; exit}' "$script_dir/linyaps-rust.spec")
box_version=$(awk '/^Version:/ {print $2; exit}' "$box_root/packaging/fedora/linyaps-box-rust.spec")
main_commit=$(git -C "$main_root" rev-parse HEAD)
box_commit=$(git -C "$box_root" rev-parse HEAD)
if [[ -n $(git -C "$main_root" status --porcelain) ]]; then main_commit+=+dirty; fi
if [[ -n $(git -C "$box_root" status --porcelain) ]]; then box_commit+=+dirty; fi
source_date_epoch=$(git -C "$main_root" log -1 --format=%ct)
input=$(mktemp -d)
trap 'rm -rf -- "$input"' EXIT
git -C "$main_root" ls-files -z --cached --others --exclude-standard | tar --null -C "$main_root" -T - -cf "$input/main.tar"
git -C "$box_root" ls-files -z --cached --others --exclude-standard | tar --null -C "$box_root" -T - -cf "$input/box.tar"
mkdir -p "$output" "$cache/cargo" "$cache/fedora$release/main" "$cache/fedora$release/box"
docker build --build-arg "FEDORA_RELEASE=$release" -t "$image" -f "$script_dir/Containerfile" "$script_dir"
docker run --rm --user "$(id -u):$(id -g)" \
    --env HOME=/tmp/build-home --env CARGO_HOME=/cargo --env CARGO_INCREMENTAL=0 \
    --env MAIN_VERSION="$main_version" --env BOX_VERSION="$box_version" \
    --env MAIN_COMMIT="$main_commit" --env BOX_COMMIT="$box_commit" \
    --env SOURCE_DATE_EPOCH="$source_date_epoch" \
    --tmpfs "/work:exec,uid=$(id -u),gid=$(id -g),mode=0700,size=8g" \
    --volume "$input:/input:ro" --volume "$output:/output" \
    --volume "$script_dir/build-in-container.sh:/tools/build-in-container.sh:ro" \
    --volume "$cache/cargo:/cargo" \
    --volume "$cache/fedora$release/main:/targets/main" \
    --volume "$cache/fedora$release/box:/targets/box" \
    "$image" bash /tools/build-in-container.sh
printf 'RPM output: %s\n' "$output"
