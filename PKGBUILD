# Maintainer: Jules <jules@example.com>
pkgname=dawg
pkgver=0.1.0
pkgrel=1
pkgdesc="A native, high-performance Linux desktop application for managing Minecraft servers, optimized for CachyOS."
arch=('x86_64')
url="https://github.com/DasFletchi/Dawg"
license=('MIT')
depends=('glibc' 'gcc-libs' 'wayland' 'libxkbcommon' 'vulkan-driver')
makedepends=('cargo' 'cmake' 'pkgconf' 'wayland-protocols')
source=("git+$url.git")
md5sums=('SKIP')

build() {
  cd "$srcdir/$pkgname"
  export RUSTFLAGS="-C target-cpu=x86-64-v3"
  cargo build --release --locked
}

package() {
  cd "$srcdir/$pkgname"

  install -Dm755 "target/release/dawg-daemon" "$pkgdir/usr/bin/dawg-daemon"
  install -Dm755 "target/release/dawg-ui" "$pkgdir/usr/bin/dawg"

  # Optional desktop file
  install -Dm644 dawg.desktop "$pkgdir/usr/share/applications/dawg.desktop"
}
