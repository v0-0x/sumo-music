# Maintainer: v0-0x
# Builds Sumo from this checkout:  makepkg -si
pkgname=multimusic
# The same as `version` in Cargo.toml (a test checks). A fixed version, not a pkgver() function:
# makepkg writes what pkgver() returns into this file, which then blocks `git pull`.
pkgver=0.5.3
pkgrel=1
pkgdesc="Sumo: lightweight native music player for local files, Spotify and SoundCloud with synced lyrics, Discord Rich Presence and Last.fm scrobbling"
arch=('x86_64' 'aarch64')
url="https://github.com/v0-0x/sumo-music"
license=('MIT')
depends=('mpv' 'libpulse' 'alsa-lib' 'openssl' 'libxkbcommon' 'libglvnd' 'wayland' 'libx11' 'libxcursor' 'libxrandr' 'libxi'
         'hicolor-icon-theme' 'gcc-libs' 'glibc')
makedepends=('cargo')
optdepends=('pipewire-pulse: Spotify output on PipeWire desktops (usually already installed)'
            'noto-fonts: symbols (☆ ✞ ♡ ...) in song and artist names'
            'noto-fonts-cjk: Japanese, Chinese and Korean song titles'
            'inter-font: nicer interface font'
            'discord: Rich Presence (also works with Vesktop / arRPC)'
            'yt-dlp: download Spotify and Apple Music songs (found on YouTube)')
# The app is called Sumo; the package keeps its old name so installed copies update in place.
provides=('multimusic' 'sumo')
conflicts=('medley')
replaces=('medley')
options=('!lto' '!debug')

_root() {
  # The PKGBUILD lives in the repository root.
  printf '%s' "$startdir"
}

prepare() {
  cd "$(_root)"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$(_root)"
  export RUSTUP_TOOLCHAIN=stable
  cargo build --frozen --release
}

check() {
  cd "$(_root)"
  export RUSTUP_TOOLCHAIN=stable
  cargo test --frozen --release
}

package() {
  cd "$(_root)"
  install -Dm755 target/release/multimusic "$pkgdir/usr/bin/multimusic"
  ln -s multimusic "$pkgdir/usr/bin/sumo"
  install -Dm644 packaging/multimusic.desktop "$pkgdir/usr/share/applications/multimusic.desktop"
  # The icon is named "sumo" (not "multimusic"), so desktops that cached the old logo under the
  # old name load the new one.
  install -Dm644 assets/logo.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/sumo.svg"
  local size
  for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "assets/icon-$size.png" "$pkgdir/usr/share/icons/hicolor/${size}x${size}/apps/sumo.png"
  done
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
