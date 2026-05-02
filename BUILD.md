# Build And Deployment

μAgent targets Linux-capable devices and larger platforms: SBCs, edge devices,
desktops, servers, iOS, and Android. The runtime is standard-library Rust with
platform differences handled through target triples and host adapters.

## Local Development Build

```bash
make build     # cargo build --workspace
make test      # cargo test --workspace
make clippy    # cargo clippy --all-targets -- -D warnings
```

## Target Matrix

| Device | Target triple | Make target | Link mode | Typical use |
|---|---|---|---|---|
| Raspberry Pi 4/5 64-bit | `aarch64-unknown-linux-musl` | `make pi` | Static | Copy to the Pi and run without depending on the Pi's glibc version |
| Raspberry Pi 4/5 64-bit glibc | `aarch64-unknown-linux-gnu` | `make pi-gnu` | Dynamic | Smaller binary; must match the target glibc version |
| Rock 5 / Orange Pi / Jetson | `aarch64-unknown-linux-musl` | `make pi` | Static | Same path as 64-bit Raspberry Pi |
| Pi Zero / Pi 3 32-bit Raspios | `armv7-unknown-linux-musleabihf` | `make pi32` | Static | 32-bit ARM hard-float |
| Linux server / Docker | `x86_64-unknown-linux-musl` | `make linux-x86` | Static | Alpine or scratch-friendly images |
| macOS host | Host default | `make build` | Dynamic | Local development and testing |
| iOS / Android | UniFFI bundle | See below | Library | Library embedding |

## Cross-Compilation Toolchain

Linux cross-compilation is expected to use
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild). With Zig
installed, the same setup can produce musl static binaries from macOS.

```bash
cargo install cargo-zigbuild
brew install zig        # macOS; use apt or the official tarball on Linux

rustup target add aarch64-unknown-linux-musl
```

The Makefile targets such as `make pi` run `rustup target add` and `zigbuild`
for you.

## Build Profiles

```bash
cargo build --workspace                    # debug, symbols, fast builds
cargo build --workspace --release          # standard release build
cargo build --workspace --profile=min      # small binary, used by cross targets
```

The `min` profile is defined in the workspace `Cargo.toml` with
`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, and
`strip = true`. It builds more slowly and removes unwind backtraces, but
produces a small static binary.

Use the release profile for cross builds when needed:

```bash
make pi PROFILE=release
```

## Raspberry Pi Deployment

```bash
make pi

scp target/aarch64-unknown-linux-musl/min/muagent pi@raspberrypi.local:~/
ssh pi@raspberrypi.local './muagent --help'
```

Add systemd service files or Docker wrappers in the deployment repository that
owns the target environment.

## Verify Cross Artifacts

```bash
file target/aarch64-unknown-linux-musl/min/muagent
```

Check dynamic dependencies. A musl static build should have no `NEEDED` entries:

```bash
objdump -p target/aarch64-unknown-linux-musl/min/muagent | grep NEEDED || echo "static OK"
```

## iOS And Android

Mobile targets should embed μAgent as a UniFFI library rather than shipping the
CLI binary.

- iOS: build `aarch64-apple-ios-sim` / `aarch64-apple-ios` static libraries,
  generate Swift bindings with `uniffi-bindgen swift`, and package an
  XCFramework.
- Android: build with `cargo ndk` for `arm64-v8a` and `armeabi-v7a`, generate
  Kotlin bindings with `uniffi-bindgen kotlin`, and package an AAR.

## Out Of Scope

- MCU targets such as Cortex-M devices with less than 1 MB RAM. TLS, JSON, and
  the runtime FSM require a larger host environment. μAgent is `std`-only and
  does not provide a `no_std` build.
- Browser WASM as a first-class target. The code can be adapted, but
  `tokio::time`, `std::fs`, and network/process integration require host-side
  replacements for `SessionStore`, `NetEgress`, `Clock`, and related adapters.
