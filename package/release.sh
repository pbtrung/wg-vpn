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

echo "==> packaging both binaries into one tarball per arch"
declare -A assets
for arch in x86_64 aarch64; do
    tarball="${dist_dir}/${arch}/wg-vpn-${arch}.tar.gz"
    # Both binaries keep their plain names (wg-server/wg-client) inside
    # the tarball -- only the archive's own filename carries the arch
    # suffix -- so PKGBUILD's package() doesn't need to know $CARCH to
    # find them after makepkg auto-extracts this source.
    tar -czf "$tarball" -C "${dist_dir}/${arch}" wg-server wg-client
    assets["$arch"]="$tarball"
done

echo "==> computing checksums"
declare -A sums
for arch in x86_64 aarch64; do
    sums["$arch"]="$(sha256sum "${assets[$arch]}" | cut -d' ' -f1)"
done

echo "==> creating GitHub release ${tag}"
gh release create "$tag" \
    --repo pbtrung/wg-vpn \
    --title "$tag" \
    --generate-notes \
    "${assets[x86_64]}" \
    "${assets[aarch64]}"

echo "==> updating package/PKGBUILD"
license_sum="$(sha256sum "$repo_root/LICENSE" | cut -d' ' -f1)"
pkgbuild="$package_dir/PKGBUILD"
sed -i \
    -e "s/^pkgver=.*/pkgver=${version}/" \
    -e "s/^pkgrel=.*/pkgrel=1/" \
    -e "s/REPLACE_WG_VPN_X86_64/${sums[x86_64]}/" \
    -e "s/REPLACE_WG_VPN_AARCH64/${sums[aarch64]}/" \
    -e "s/REPLACE_LICENSE/${license_sum}/" \
    "$pkgbuild"

echo "==> done: ${tag} released, PKGBUILD updated"
echo "    remember to commit package/PKGBUILD and, if this is meant for"
echo "    the AUR, run 'makepkg --printsrcinfo > .SRCINFO' there and push."
