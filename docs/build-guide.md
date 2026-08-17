# Build Guide

This is for those who want to build `rathole` themselves, possibly because the need of latest features or the minimal binary size.

## Build

To use default build settings, run:

```sh
cargo build --release
```

You may need to pre-install [openssl](https://docs.rs/openssl/latest/openssl/index.html) dependencies in Unix-like systems.

## Customize the Build

`rathole` comes with lots of *crate features* that determine whether a certain feature will be compiled or not. Supported features can be checked out in `[features]` of [Cargo.toml](../Cargo.toml).

For example, to build `rathole` with the `client` and `noise` feature:

```sh
cargo build --release --no-default-features --features client,noise
```

## Rustls Support

`rathole` provides optional `rustls` support. It's an almost drop-in replacement of `native-tls` support. (See [Transport](transport.md) for more information.)

To enable this, disable the default features and enable `rustls` feature. And for websocket feature, enable `websocket-rustls` feature as well.

You can also use command line option for this. For example, to replace all default features with `rustls`:

```sh
cargo build --release --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload
```

Feature `rustls` and `websocket-rustls` cannot be enabled with `native-tls` and `websocket-native-tls` at the same time, as they are mutually exclusive. Enabling both will result in a compile error.

(Note that default features contains `native-tls` and `websocket-native-tls`. On Linux, CI builds exclusively with the rustls feature set, so no system OpenSSL is required there; `native-tls` is only exercised on Windows, where it maps to Schannel.)

## Cross-compiling and Testing for ARM (rathole-x fork)

The Linux release targets are musl static binaries built with the rustls feature set:

- `x86_64-unknown-linux-musl`, `i686-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`

`armv7-unknown-linux-musleabihf` (ARMv7 hard-float) is the right target for a Raspberry Pi 3B running a 32-bit userland.

### Toolchain setup (one-time)

1. Rust targets:

   ```sh
   rustup target add x86_64-unknown-linux-musl i686-unknown-linux-musl \
     aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf \
     armv7-unknown-linux-gnueabihf
   ```

2. [zig](https://ziglang.org) as the musl C toolchain/linker (needed by `ring`). Install it permanently, e.g. `~/.local/opt/zig-*` with `~/.local/bin/zig` symlinked, then create shim scripts in `~/.local/bin`:

   - `zig-cc-targets`: dispatches on its symlink name to `zig cc -target <t>` and **drops `-march=*`/`-mfpu=*`/`-Wl,-melf*` arguments** (zig cc rejects some of the spellings `ring`'s build script emits; the zig target already implies the correct baseline). Symlink it as `zig-cc-armv7-musl` (`arm-linux-musleabihf`), `zig-cc-aarch64-musl` (`aarch64-linux-musl`), `zig-cc-i686-musl` (`x86-linux-musl`), `zig-cc-x86_64-musl` (`x86_64-linux-musl`).
   - `zig-ar` → `exec zig ar "$@"`, `zig-ranlib` → `exec zig ranlib "$@"`.

3. GNU cross gcc for glibc ARM builds (optional, needed for `*-gnueabihf`):

   ```sh
   sudo apt-get install gcc-arm-linux-gnueabihf gcc-aarch64-linux-gnu
   ```

4. qemu-user for running foreign binaries, registered persistently through binfmt:

   ```sh
   sudo apt-get install qemu-user-static
   sudo systemctl restart systemd-binfmt.service   # applies /usr/lib/binfmt.d/qemu-*.conf
   ```

5. `~/.cargo/config.toml` (user-global, applies to every project):

   ```toml
   # rustc's self-contained musl CRT and zig's CRT both define _start;
   # link-self-contained=no lets zig provide the startup files.
   [target.armv7-unknown-linux-musleabihf]
   linker = "zig-cc-armv7-musl"
   rustflags = ["-C", "link-self-contained=no"]
   runner = "qemu-arm-static"

   [target.aarch64-unknown-linux-musl]
   linker = "zig-cc-aarch64-musl"
   rustflags = ["-C", "link-self-contained=no"]
   runner = "qemu-aarch64-static"

   [target.i686-unknown-linux-musl]
   linker = "zig-cc-i686-musl"
   rustflags = ["-C", "link-self-contained=no"]
   runner = "qemu-i386-static"

   [target.x86_64-unknown-linux-musl]
   linker = "zig-cc-x86_64-musl"
   rustflags = ["-C", "link-self-contained=no"]

   [target.armv7-unknown-linux-gnueabihf]
   linker = "arm-linux-gnueabihf-gcc"
   runner = ["qemu-arm-static", "-L", "/usr/arm-linux-gnueabihf"]

   [target.aarch64-unknown-linux-gnu]
   linker = "aarch64-linux-gnu-gcc"
   runner = ["qemu-aarch64-static", "-L", "/usr/aarch64-linux-gnu"]

   [env]
   CC_armv7_unknown_linux_musleabihf = "zig-cc-armv7-musl"
   AR_armv7_unknown_linux_musleabihf = "zig-ar"
   CC_aarch64_unknown_linux_musl = "zig-cc-aarch64-musl"
   AR_aarch64_unknown_linux_musl = "zig-ar"
   CC_i686_unknown_linux_musl = "zig-cc-i686-musl"
   AR_i686_unknown_linux_musl = "zig-ar"
   CC_x86_64_unknown_linux_musl = "zig-cc-x86_64-musl"
   AR_x86_64_unknown_linux_musl = "zig-ar"
   CC_armv7_unknown_linux_gnueabihf = "arm-linux-gnueabihf-gcc"
   AR_armv7_unknown_linux_gnueabihf = "arm-linux-gnueabihf-gcc-ar"
   CC_aarch64_unknown_linux_gnu = "aarch64-linux-gnu-gcc"
   AR_aarch64_unknown_linux_gnu = "aarch64-linux-gnu-gcc-ar"
   ```

### Build and test

```sh
# musl static binary (same as CI/release):
cargo build --locked --release --target armv7-unknown-linux-musleabihf \
  --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload

# full test suite on emulated ARMv7 (binfmt or the configured runner
# executes the ARM test binaries under qemu):
cargo test --locked --release --target armv7-unknown-linux-musleabihf \
  --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload
```

`cargo zigbuild --release --target <musl-triple> ...` also works and handles the CRT details itself, but has no `test`/`check` subcommand — for those use plain `cargo` with the config above.

To package local release artifacts the same way CI does (`dist/rathole-x-<target>.tar.gz` plus `sha256sums.txt`), run `scripts/dist.sh`; it builds the default target set (`x86_64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`, override with `TARGETS=...`).

### Testing in an ARM rootfs (Raspberry Pi 3B 32-bit stand-in)

A Pi 3B in 32-bit mode runs an ARMv7 hard-float userland; the ~3 MB [Alpine armhf minirootfs](https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/armhf/) is a compact stand-in (a Raspberry Pi OS 32-bit rootfs works identically). Because the musl binary is fully static it runs in any armhf rootfs:

```sh
mkdir -p ~/Codes/rootfs/alpine-armhf && cd ~/Codes/rootfs/alpine-armhf
curl -sSL -o m.tgz https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/armhf/alpine-minirootfs-3.20.3-armhf.tar.gz
tar xzf m.tgz && rm m.tgz
cp /usr/bin/qemu-arm-static usr/bin/          # needed for chroot
cp target/armv7-unknown-linux-musleabihf/release/rathole-x usr/local/bin/

# smoke test inside the emulated rootfs:
sudo chroot ~/Codes/rootfs/alpine-armhf qemu-arm-static /usr/local/bin/rathole-x --version
```

With binfmt's `F` flag registered (step 4), ARM binaries can also be executed directly without explicit `qemu-arm-static`.

## Minimalize the binary

1. Build with the `minimal` profile

The `release` build profile optimize for the program running time, not the binary size.

However, the `minimal` profile enables lots of optimization for the binary size to produce a much smaller binary.

For example, to build `rathole` with `client` feature with the `minimal` profile:

```sh
cargo build --profile minimal --no-default-features --features client
```

2. `strip` and `upx`

The binary that step 1 produces can be even smaller, by using `strip` and `upx` to remove the symbols and compress the binary.

Like:

```sh
strip rathole
upx --best --lzma rathole
```

At the time of writting the build guide, the produced binary for `x86_64-unknown-linux-glibc` has the size of **574 KiB**, while `frpc` has the size of **~10 MiB**, which is much larger.
