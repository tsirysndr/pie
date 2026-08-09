# pie

[![ci](https://github.com/tsirysndr/pie/actions/workflows/ci.yml/badge.svg)](https://github.com/tsirysndr/pie/actions/workflows/ci.yml)

A CLI that builds **official releases of language runtimes and databases from source, as
position independent executables**, from typed Pkl recipes — with live build logs, PIE
verification baked in, and GitHub Actions wiring to publish the artifacts.

```
  pie · Redis  Official Redis releases, built from source as a PIE.
  ✔ Resolve version 'latest'                                  0.4s
  ✔ Install build dependencies                               12.1s
  ✔ Download redis-8.0.1.tar.gz                               2.6s
  ✔ Compile with TLS and systemd support                    1m 48s
  ✔ Verify PIE                                                0.0s
  ✔ Create redis-v8.0.1-linux-x64-pie.tar.xz                  1.9s
  ✔ Smoke test the artifact                                   3.2s

  ✔ Redis 8.0.1 built in 2m 12s
  dist/redis-v8.0.1-linux-x64-pie.tar.xz (12.4 MiB)
```

## Recipes

Each project is one typed [Pkl](https://pkl-lang.org) manifest in `recipes/` declaring
**where the source is downloaded from**, **what has to be installed to build it**, and
**how to build, verify and package it**. `pie` evaluates it through the `pkl` CLI, so a
schema violation is caught before anything is downloaded.

| Recipe | What it builds | Version source |
|---|---|---|
| `node` | Node.js | `nodejs.org/dist/index.json` |
| `python` | CPython, fully static third-party libs | `python.org/ftp/python/` |
| `bun` | Bun | `oven-sh/bun` releases |
| `php` | PHP, full extension set | php.net release index |
| `erlang` | Erlang/OTP, every optional application | `erlang/otp` releases |
| `postgres` | PostgreSQL, full option set | `ftp.postgresql.org` |
| `mariadb` | MariaDB, full plugin set | `archive.mariadb.org` |
| `redis` | Redis with TLS | `redis/redis` releases |
| `mongodb` | MongoDB Community Server | `mongodb/mongo` tags |
| `dragonfly` | DragonflyDB | `dragonflydb/dragonfly` releases |

Only versions that exist upstream can be built: every recipe resolves and validates the
request against the project's own release index **before anything is downloaded**, so a
typo or an unofficial version fails in seconds rather than an hour into a compile.

## Usage

```sh
cargo build --release

pie list                                  # what can be built
pie show python                           # source, dependencies, steps, notes
pie build node --version lts              # aliases, series and exact versions
pie build python --version 3.12 --with pgo --with lto
pie build php --version 8.3.15 --arch arm64
pie build redis --dry-run                 # resolve and print the plan only
pie resolve node --version lts            # prints 24.19.0
pie verify /usr/bin/ssh                   # check any ELF binary
```

The version is always overridable from the CLI: `--version` accepts an exact version
(`22.11.0`), a series (`3.12`, `17`), or an alias (`latest`, `lts`). Recipe features are
opt-in per build with `--with`, and readable inside a recipe as `$PIE_WITH`.

Build output streams live: an interactive terminal gets one spinner line per step showing
the newest line of output, with the full log kept aside and replayed only if the step
fails. CI and `--verbose` get every line instead.

## GitHub Actions

`.github/workflows/build.yml` builds `pie` (cached with `Swatinem/rust-cache`), resolves
the version, then runs the tool for **both architectures — `linux-x64` and `linux-arm64`,
always** — and publishes to a GitHub release.

**By tag** — push `<recipe>-v<version>` and that recipe is built at that exact version and
released to the same tag:

```
node-v22.11.0      python-v3.12.7      redis-v8.0.1
php-v8.3.15        postgres-v17.2      dragonfly-v1.27.0
```

**By dispatch** — Actions → *build* → *Run workflow*, then pick a `recipe` and a `version`
(exact, series, or alias). The release tag is `<recipe>-v<resolved version>`.

`.github/workflows/ci.yml` is the gate: `cargo fmt --check`, `clippy -D warnings`, the test
suite, and an evaluation of every Pkl recipe.

It also runs a real **end-to-end build of `redis`** — the lightest recipe by a wide margin,
a plain Makefile build that finishes in minutes, which is what makes it affordable as a
gate. It exercises the whole pipeline (resolve → dependencies → verified download → compile
→ PIE verification → package → unpack to a fresh prefix → round-trip against a running
server), then asserts the shipped binaries are position independent **twice**: once with
`pie verify`, and once with `readelf` directly, so the gate does not rest solely on `pie`'s
own ELF parser being correct. The version is pinned rather than `latest`, so an upstream
release cannot fail an unrelated pull request.

## How PIE is applied

PIE is a property of the link, and none of these projects expose an `--enable-pie` switch,
so it comes from the toolchain. The mechanism differs per project because **what else each
build links matters** — a project that also produces shared objects cannot simply have
`-pie` in `LDFLAGS`, since `cc -shared -pie` is a hard error:

| Project | Where `-pie` goes | Why |
|---|---|---|
| Node.js | `LDFLAGS` | A default Node build links executables only |
| Redis, Bun | `LDFLAGS` | Dependencies are static archives |
| CPython | `LINKFORSHARED` | `LDFLAGS` is reused for stdlib extension `.so` files |
| PHP | `EXTRA_LDFLAGS_PROGRAM` | PHP's own program-only link variable |
| PostgreSQL | `LDFLAGS_EX` | Postgres separates `_EX` (executables) from `_SL` (shared libs) |
| MariaDB, Dragonfly | `CMAKE_EXE_LINKER_FLAGS` | CMake keeps the two apart by construction |
| MongoDB | `LINKFLAGS` | SCons applies it to program links |
| Erlang/OTP | — | Relies on the toolchain default; the verify gate enforces the result |

For CPython and PHP the configured value is read back out of the generated `Makefile` and
appended to, never replaced — overwriting `LINKFORSHARED` would silently cost the
interpreter its `-export-dynamic` and break C extensions.

## Verification

`pie` parses ELF itself rather than shelling out to `readelf`, and **fails the build**
unless each declared binary is:

- ELF type `DYN`,
- carrying `DT_FLAGS_1 = PIE`,
- and has a `PT_INTERP` segment.

All three matter: a shared library is *also* `ET_DYN` and *also* lacks `PT_INTERP`, so type
alone would wrongly pass one. Binaries are checked in the build tree and again after the
staging install, then every archive is unpacked to a **fresh prefix** and smoke tested
there — which is what actually proves the artifacts are relocatable rather than pinned to
their build path. The Redis, Dragonfly, MongoDB and PostgreSQL recipes go further and
start the server, talk to it, and shut it down.

A recipe may also declare `verify.self_contained` with a `dynamic_allowlist`, and the build
fails if the binary pulls in any shared library outside it.

## The static CPython build

`recipes/python.pkl` produces the same shape of distribution `uv` downloads: every
third-party library is compiled from source as a static `-fPIC` archive by
`scripts/python/build-deps.sh` (zlib, bzip2, xz, OpenSSL, libffi, SQLite, ncurses,
readline, libuuid, mpdecimal), and `MODULE_BUILDTYPE=static` flips
`Modules/Setup.stdlib` so every stdlib extension is compiled *into* the interpreter
instead of landing as a `.so` in `lib-dynload`.

The result needs nothing but glibc, which the build enforces rather than assumes: the
recipe's `dynamic_allowlist` rejects any `DT_NEEDED` outside libc, libm, libdl, libpthread,
librt, libutil, libcrypt, libnsl and the loader.

Two honest gaps versus `python-build-standalone`: **glibc itself is still dynamic**, so the
build host's glibc is the floor (ubuntu-24.04 ⇒ 2.39; build in a `manylinux` container for
an older one), and **tkinter and gdbm are not built**. The terminfo database is not bundled
either, so `curses` uses the host's.

## Recipes are Pkl

`recipes/*.pkl` is the **only** source of truth — the YAML is generated output and is
gitignored. `pie` loads `.pkl` natively by shelling out to the `pkl` CLI, so nothing needs
generating to build:

```sh
pie build redis                 # reads recipes/redis.pkl directly
pie generate                    # optional: render recipes/*.yaml beside the sources
pie generate redis --check      # report stale output instead of rewriting it
```

`.pkl` always wins over a `.yaml` of the same name, so a stale render can never be built by
accident. If `pkl` is not installed, `pie` says so and points at the generated YAML.

`pkl/Recipe.pkl` is the schema, and it does real work — these are type and constraint
errors, caught at load time rather than an hour into a build:

- a misspelled `resolver`, `format`, or `checksum.kind`
- a `source.url` that does not reference the resolved version, so every version would
  build the same thing
- a `package.name` missing the version, the architecture, or `pie`, when artifacts from
  every build land in one flat release directory
- an empty `build` or `package.steps`

```
–– Pkl Error ––
Type constraint `contains("{{version") || contains("{{upstream_tag}}")` violated.
Value: "https://example.com/fixed.tar.gz"
```

CI evaluates every recipe through Pkl on each push. Shell comments inside `run` blocks are
part of the script and survive rendering intact.

## Tests

```sh
cargo test
```

51 tests, no network required (Pkl is needed to load the recipes). The ELF parser is exercised against synthetic ELF64 images
covering a real PIE executable, a shared library, a fixed-address executable and a `DYN`
binary without the PIE bit. Version resolution is tested through pure selector functions
(LTS skipping a newer non-LTS release, series matching that does not treat `3.1` as a
prefix of `3.12`, numeric rather than lexical ordering). `tests/recipes.rs` lints every
shipped recipe: names match filenames, every template variable exists, artifact names are
unambiguous, referenced helper scripts are present, every recipe has a Pkl source, and a
`.pkl` always takes precedence over a generated `.yaml` beside it.

## Notes and caveats

- Runners are `ubuntu-24.04` (x64) and `ubuntu-24.04-arm` (arm64). GitHub-hosted ARM
  runners are free for public repositories; private repos need a paid or self-hosted one.
- Rough build times per architecture: Redis and Dragonfly minutes, CPython 20–40 min with
  PGO, Node.js 40–90 min, PHP and PostgreSQL under an hour, Bun 1–3 h, **MongoDB several
  hours** and very hungry for disk and RAM. The job timeout is 6 hours.
- **Source integrity** varies by what upstream publishes: Node.js and PostgreSQL and
  MariaDB are verified against a published `SHASUMS`-style manifest, PHP against the
  digest in its release index. CPython, Bun, Redis, MongoDB and Dragonfly have no
  published manifest, so the digest is recorded next to the artifact and in the log rather
  than gated on.
- **Bun is the fragile one.** Upstream ships a non-PIE `ELF EXEC` binary, so this is a real
  rebuild rather than a repackage — but it pins an exact LLVM (21.1.8, while `apt.llvm.org`
  serves whatever 21.x is newest) and moved off CMake between 1.2 and 1.3.
- **MongoDB** is Community Server under the SSPL; check the licence before redistributing.
- Dragonfly has no `source:` block: it is cloned with submodules, because helio and the
  other vendored dependencies are absent from a GitHub archive tarball.
