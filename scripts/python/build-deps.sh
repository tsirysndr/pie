#!/usr/bin/env bash
#
# Builds every third-party library CPython links against as a *static* archive
# compiled -fPIC, installed into $DEPS_PREFIX.
#
# Only .a files end up in $DEPS_PREFIX/lib, which is the whole trick: when
# CPython later links -lssl / -lsqlite3 / -lz, the linker has nothing but static
# archives to choose from, so the interpreter absorbs them. -fPIC keeps every
# object eligible for the final PIE link.
#
# The result is a python that is dynamically linked against glibc only — the
# same shape as the python-build-standalone distributions uv downloads.
#
# Inputs (environment):
#   DEPS_PREFIX      install prefix                       (required)
#   DEPS_SRC         scratch dir for sources              (default $PWD/deps-src)
#   DEPS_LOCK        sha256 manifest to verify against    (default alongside this script)
#   DEPS_DIGESTS     where to write observed digests      (default $PWD/deps-digests.txt)
#   OPENSSL_VERSION  override the pinned OpenSSL version  (optional)

set -euo pipefail

DEPS_PREFIX="${DEPS_PREFIX:?DEPS_PREFIX must be set}"
DEPS_SRC="${DEPS_SRC:-$PWD/deps-src}"
DEPS_LOCK="${DEPS_LOCK:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/deps.lock}"
DEPS_DIGESTS="${DEPS_DIGESTS:-$PWD/deps-digests.txt}"
JOBS="$(nproc)"

# Pinned versions. Bump these deliberately; a 404 here means a version was
# withdrawn upstream and the pin needs updating.
ZLIB_VERSION=1.3.1
BZIP2_VERSION=1.0.8
XZ_VERSION=5.4.7                       # deliberately pre-5.6.0 / post-backdoor-safe
OPENSSL_VERSION="${OPENSSL_VERSION:-3.5.0}"
LIBFFI_VERSION=3.4.6
SQLITE_YEAR=2024
SQLITE_VERSION=3460100                 # 3.46.1
NCURSES_VERSION=6.5
READLINE_VERSION=8.2
UTIL_LINUX_SERIES=2.40
UTIL_LINUX_VERSION=2.40.2
MPDECIMAL_VERSION=4.0.0

mkdir -p "$DEPS_PREFIX" "$DEPS_SRC"
: > "$DEPS_DIGESTS"

export CFLAGS="-fPIC -O2 -fstack-protector-strong"
export CXXFLAGS="$CFLAGS"
export CPPFLAGS="-I${DEPS_PREFIX}/include"
export LDFLAGS="-L${DEPS_PREFIX}/lib"
export PKG_CONFIG_PATH="${DEPS_PREFIX}/lib/pkgconfig"

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# Downloads $1 into $DEPS_SRC, verifies it against $DEPS_LOCK when the manifest
# has an entry for it, and always records the digest so an unpinned dependency
# can be pinned after the first run.
fetch() {
  local url="$1" file
  file="$(basename "$url")"

  if [ ! -f "$DEPS_SRC/$file" ]; then
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$DEPS_SRC/$file" "$url"
  fi

  local actual expected=''
  actual="$(sha256sum "$DEPS_SRC/$file" | awk '{print $1}')"
  if [ -f "$DEPS_LOCK" ]; then
    expected="$(awk -v f="$file" '$2 == f {print $1}' "$DEPS_LOCK" | head -n1)"
  fi

  if [ -n "$expected" ]; then
    if [ "$actual" != "$expected" ]; then
      echo "::error::checksum mismatch for $file: expected $expected, got $actual" >&2
      exit 1
    fi
    echo "verified $file"
  else
    echo "::warning::$file is not pinned in $(basename "$DEPS_LOCK") — recording $actual" >&2
  fi

  printf '%s  %s\n' "$actual" "$file" >> "$DEPS_DIGESTS"
  printf '%s' "$DEPS_SRC/$file"
}

# Unpacks $1 into $DEPS_SRC/$2 and cds there.
unpack() {
  local tarball="$1" dir="$2"
  rm -rf "${DEPS_SRC:?}/$dir"
  mkdir -p "$DEPS_SRC/$dir"
  tar -xf "$tarball" -C "$DEPS_SRC/$dir" --strip-components=1
  cd "$DEPS_SRC/$dir"
}

build_zlib() {
  log "zlib $ZLIB_VERSION"
  # The fossils/ directory keeps every release forever; the top level does not.
  unpack "$(fetch "https://zlib.net/fossils/zlib-${ZLIB_VERSION}.tar.gz")" zlib
  ./configure --prefix="$DEPS_PREFIX" --static
  make -j"$JOBS"
  make install
}

build_bzip2() {
  log "bzip2 $BZIP2_VERSION"
  unpack "$(fetch "https://sourceware.org/pub/bzip2/bzip2-${BZIP2_VERSION}.tar.gz")" bzip2
  # bzip2 has no configure; build only the static library and install by hand so
  # no shared object is produced.
  make -j"$JOBS" libbz2.a CFLAGS="$CFLAGS -D_FILE_OFFSET_BITS=64"
  install -Dm644 libbz2.a "$DEPS_PREFIX/lib/libbz2.a"
  install -Dm644 bzlib.h "$DEPS_PREFIX/include/bzlib.h"
}

build_xz() {
  log "xz $XZ_VERSION"
  unpack "$(fetch "https://github.com/tukaani-project/xz/releases/download/v${XZ_VERSION}/xz-${XZ_VERSION}.tar.gz")" xz
  ./configure --prefix="$DEPS_PREFIX" \
    --disable-shared --enable-static --with-pic \
    --disable-xz --disable-xzdec --disable-lzmadec --disable-lzmainfo \
    --disable-lzma-links --disable-scripts --disable-doc --disable-nls
  make -j"$JOBS"
  make install
}

build_openssl() {
  log "openssl $OPENSSL_VERSION"
  unpack "$(fetch "https://github.com/openssl/openssl/releases/download/openssl-${OPENSSL_VERSION}/openssl-${OPENSSL_VERSION}.tar.gz")" openssl
  # --libdir=lib stops OpenSSL from using lib64 on 64-bit hosts, which keeps a
  # single -L path working for every dependency.
  ./Configure \
    --prefix="$DEPS_PREFIX" \
    --openssldir="$DEPS_PREFIX/ssl" \
    --libdir=lib \
    no-shared no-module no-tests no-docs \
    -fPIC
  make -j"$JOBS"
  make install_sw
}

build_libffi() {
  log "libffi $LIBFFI_VERSION"
  unpack "$(fetch "https://github.com/libffi/libffi/releases/download/v${LIBFFI_VERSION}/libffi-${LIBFFI_VERSION}.tar.gz")" libffi
  ./configure --prefix="$DEPS_PREFIX" \
    --disable-shared --enable-static --with-pic \
    --disable-multi-os-directory --disable-docs
  make -j"$JOBS"
  make install
}

build_sqlite() {
  log "sqlite $SQLITE_VERSION"
  unpack "$(fetch "https://www.sqlite.org/${SQLITE_YEAR}/sqlite-autoconf-${SQLITE_VERSION}.tar.gz")" sqlite
  # Feature set mirrors what general-purpose Python distributions enable.
  ./configure --prefix="$DEPS_PREFIX" --disable-shared --enable-static --with-pic \
    CFLAGS="$CFLAGS \
      -DSQLITE_ENABLE_FTS4 -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_RTREE \
      -DSQLITE_ENABLE_COLUMN_METADATA -DSQLITE_ENABLE_MATH_FUNCTIONS \
      -DSQLITE_ENABLE_DBSTAT_VTAB -DSQLITE_SECURE_DELETE \
      -DSQLITE_MAX_VARIABLE_NUMBER=250000"
  make -j"$JOBS"
  make install
}

build_ncurses() {
  log "ncurses $NCURSES_VERSION"
  unpack "$(fetch "https://ftp.gnu.org/gnu/ncurses/ncurses-${NCURSES_VERSION}.tar.gz")" ncurses
  # --disable-db-install: the terminfo database is NOT bundled (it is large and
  # every Linux ships one). --with-fallbacks compiles entries for the common
  # terminals straight into the archive so curses still works without a DB.
  ./configure --prefix="$DEPS_PREFIX" \
    --without-shared --with-normal --with-pic \
    --enable-widec --enable-pc-files \
    --with-pkg-config-libdir="$DEPS_PREFIX/lib/pkgconfig" \
    --without-cxx-binding --without-ada --without-manpages --without-tests \
    --without-debug --without-progs --disable-db-install \
    --with-fallbacks=linux,xterm,xterm-256color,screen,screen-256color,vt100,dumb \
    --disable-stripping
  make -j"$JOBS"
  make install

  # readline and CPython variously look for -lncurses / -ltinfo. With
  # --with-termlib omitted everything lives in libncursesw.a, so alias the rest.
  local l
  for l in libncurses libtinfo libtinfow; do
    ln -sf libncursesw.a "$DEPS_PREFIX/lib/${l}.a"
  done
}

build_readline() {
  log "readline $READLINE_VERSION"
  unpack "$(fetch "https://ftp.gnu.org/gnu/readline/readline-${READLINE_VERSION}.tar.gz")" readline
  # readline probes for a termcap provider; point it at the static ncursesw we
  # just installed instead of letting it find a system shared library.
  ./configure --prefix="$DEPS_PREFIX" \
    --disable-shared --enable-static --with-curses \
    bash_cv_termcap_lib=libncursesw \
    CPPFLAGS="$CPPFLAGS -I${DEPS_PREFIX}/include/ncursesw"
  make -j"$JOBS" SHLIB_LIBS="-lncursesw"
  make install
}

build_libuuid() {
  log "util-linux (libuuid only) $UTIL_LINUX_VERSION"
  unpack "$(fetch "https://www.kernel.org/pub/linux/utils/util-linux/v${UTIL_LINUX_SERIES}/util-linux-${UTIL_LINUX_VERSION}.tar.xz")" util-linux
  ./configure --prefix="$DEPS_PREFIX" \
    --disable-all-programs --enable-libuuid \
    --disable-shared --enable-static --with-pic \
    --disable-nls --without-python --without-systemd --disable-asciidoc
  make -j"$JOBS"
  make install
}

build_mpdecimal() {
  log "mpdecimal $MPDECIMAL_VERSION"
  unpack "$(fetch "https://www.bytereef.org/software/mpdecimal/releases/mpdecimal-${MPDECIMAL_VERSION}.tar.gz")" mpdecimal
  # Only needed by CPython versions that dropped the bundled copy; harmless
  # otherwise, and it keeps `decimal` on the fast C implementation either way.
  ./configure --prefix="$DEPS_PREFIX" --disable-shared --enable-static --disable-cxx
  make -j"$JOBS"
  make install
}

build_zlib
build_bzip2
build_xz
build_openssl
build_libffi
build_sqlite
build_ncurses
build_readline
build_libuuid
build_mpdecimal

log "static libraries in $DEPS_PREFIX/lib"
ls -1 "$DEPS_PREFIX"/lib/*.a

# A stray .so here would silently become a runtime dependency of the interpreter.
if compgen -G "$DEPS_PREFIX/lib/*.so*" > /dev/null; then
  echo "::error::shared objects were installed into $DEPS_PREFIX/lib — the build would not be self-contained:" >&2
  ls -1 "$DEPS_PREFIX"/lib/*.so* >&2
  exit 1
fi

log "dependency digests"
cat "$DEPS_DIGESTS"
