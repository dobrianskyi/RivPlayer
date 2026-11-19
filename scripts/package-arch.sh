#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
release="$root/target/release/rivplayer"
destination="$root/dist"

case "${1:-}" in
    --help|-h) printf 'Usage: %s [--install]\n' "${0##*/}"; exit 0 ;;
    ''|--install) ;;
    *) printf 'Usage: %s [--install]\n' "${0##*/}" >&2; exit 2 ;;
esac
if (( $# > 1 )); then
    printf 'Usage: %s [--install]\n' "${0##*/}" >&2
    exit 2
fi
if [[ ! -x "$release" ]]; then
    printf 'Build the Release first with cargo build --release --locked.\n' >&2
    exit 1
fi

version=$(sed -n '/^\[package\]/,/^\[/s/^version = "\([^"]*\)"$/\1/p' "$root/Cargo.toml")
if [[ ! "$version" =~ ^[[:alnum:].+_]+$ ]]; then
    printf 'Invalid Arch package version in Cargo.toml.\n' >&2
    exit 1
fi

mkdir -p -- "$destination"
staging=$(mktemp -d)
trap 'rm -rf -- "$staging"' EXIT
cp -- "$release" "$root/assets/"{rivplayer.desktop,rivplayer.svg,rivplayer.png,Roboto-OFL.txt} \
    "$root/docs/THIRD-PARTY-NOTICES.txt" "$staging"
cd -- "$staging"

printf "pkgver='%s'\n" "$version" > PKGBUILD
cat >> PKGBUILD <<'PKG'
pkgname=rivplayer
pkgrel=1
pkgdesc='Native Linux media player with built-in decoders'
url='https://github.com/dobrianskyi/RivPlayer'
arch=('x86_64')
conflicts=('rvideoplayer')
replaces=('rvideoplayer')
depends=('glibc' 'libgcc' 'alsa-lib' 'libglvnd' 'libxkbcommon' 'wayland'
         'libx11' 'libxcb' 'libxcursor' 'libxi' 'libxrandr'
         'desktop-file-utils' 'shared-mime-info')
optdepends=('libva: VA-API hardware video decoding'
            'xdg-desktop-portal: native file chooser with a desktop portal backend'
            'zenity: fallback file chooser')
options=('!strip' '!debug' '!lto')
source=('rivplayer' 'rivplayer.desktop' 'rivplayer.svg' 'rivplayer.png'
        'Roboto-OFL.txt' 'THIRD-PARTY-NOTICES.txt')

package() {
    install -Dm755 "$srcdir/rivplayer" "$pkgdir/usr/bin/rivplayer"
    install -Dm644 "$srcdir/rivplayer.desktop" "$pkgdir/usr/share/applications/rivplayer.desktop"
    install -Dm644 "$srcdir/rivplayer.svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/rivplayer.svg"
    install -Dm644 "$srcdir/rivplayer.png" "$pkgdir/usr/share/icons/hicolor/128x128/apps/rivplayer.png"
    install -Dm644 "$srcdir/Roboto-OFL.txt" "$pkgdir/usr/share/licenses/rivplayer/Roboto-OFL.txt"
    install -Dm644 "$srcdir/THIRD-PARTY-NOTICES.txt" "$pkgdir/usr/share/licenses/rivplayer/THIRD-PARTY-NOTICES.txt"
}
PKG

checksums=$(makepkg --geninteg)
printf '%s\n' "$checksums" >> PKGBUILD
export PKGDEST="$destination"
makepkg --force
package_file=$(makepkg --packagelist)
printf '%s\n' "$package_file"

if [[ "${1:-}" == --install ]]; then
    sudo pacman -U -- "$package_file"
fi
