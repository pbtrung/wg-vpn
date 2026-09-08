#!/usr/bin/env bash
# Build wg-server/wg-client for x86_64 and aarch64 via
# package/Dockerfile.build, upload them as GitHub release assets, and
# update package/PKGBUILD's pkgver/sha256sums to match.
#
# Usage: package/release.sh <version>   (e.g. package/release.sh 1.2.3)
#
# Requires: docker (with buildx), gh (authenticated), git.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <version>" >&2
    exit 1
fi
version="$1"
tag="v${version}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package_dir="$repo_root/package"
dist_dir="$package_dir/dist"

if ! git -C "$repo_root" rev-parse "$tag" >/dev/null 2>&1; then
    echo "==> tagging ${tag} at HEAD and pushing"
    git -C "$repo_root" tag "$tag"
    git -C "$repo_root" push origin "$tag"
fi

# PKGBUILD's LICENSE checksum is taken from the local working tree
# below, so this only produces a correct PKGBUILD when run at (or
# after tagging) the same commit LICENSE will be fetched from.
echo "==> building x86_64 + aarch64 binaries for ${tag}"
rm -rf "$dist_dir"
mkdir -p "$dist_dir"
docker buildx build \
    -f "$package_dir/Dockerfile.build" \
    --output "type=local,dest=${dist_dir}" \
    --build-arg "WG_VPN_REF=${tag}" \
    "$package_dir"

echo "==> renaming assets"
declare -A assets
for arch in x86_64 aarch64; do
    for bin in wg-server wg-client; do
        src="${dist_dir}/${arch}/${bin}"
        dest="${dist_dir}/${bin}-${arch}"
        mv "$src" "$dest"
        assets["${bin}_${arch}"]="$dest"
    done
done

echo "==> computing checksums"
declare -A sums
for key in "${!assets[@]}"; do
    sums["$key"]="$(sha256sum "${assets[$key]}" | cut -d' ' -f1)"
done

echo "==> creating GitHub release ${tag}"
gh release create "$tag" \
    --repo pbtrung/wg-vpn \
    --title "$tag" \
    --generate-notes \
    "${assets[wg-server_x86_64]}#wg-server-x86_64" \
    "${assets[wg-client_x86_64]}#wg-client-x86_64" \
    "${assets[wg-server_aarch64]}#wg-server-aarch64" \
    "${assets[wg-client_aarch64]}#wg-client-aarch64"

echo "==> updating package/PKGBUILD"
license_sum="$(sha256sum "$repo_root/LICENSE" | cut -d' ' -f1)"
pkgbuild="$package_dir/PKGBUILD"
sed -i \
    -e "s/^pkgver=.*/pkgver=${version}/" \
    -e "s/^pkgrel=.*/pkgrel=1/" \
    -e "s/REPLACE_WG_SERVER_X86_64/${sums[wg-server_x86_64]}/" \
    -e "s/REPLACE_WG_CLIENT_X86_64/${sums[wg-client_x86_64]}/" \
    -e "s/REPLACE_WG_SERVER_AARCH64/${sums[wg-server_aarch64]}/" \
    -e "s/REPLACE_WG_CLIENT_AARCH64/${sums[wg-client_aarch64]}/" \
    -e "s/REPLACE_LICENSE/${license_sum}/" \
    "$pkgbuild"

echo "==> done: ${tag} released, PKGBUILD updated"
echo "    remember to commit package/PKGBUILD and, if this is meant for"
echo "    the AUR, run 'makepkg --printsrcinfo > .SRCINFO' there and push."
