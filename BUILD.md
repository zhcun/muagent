# Build & 跨平台部署

μAgent 的运行时目标是**能跑 Linux 的设备及以上**(SBC / Edge AI / 桌面 / 服务器 / iOS / Android)。
代码本身是 pure std Rust,没有平台分叉 —— 切平台 = 切 target triple。

## 开发机本地

```bash
make build     # cargo build --workspace
make test      # cargo test --workspace
make clippy    # -D warnings
```

## 目标矩阵

| 设备 | Target triple | make target | 链接方式 | 典型用法 |
|---|---|---|---|---|
| **Raspberry Pi 4/5 (64-bit)** | `aarch64-unknown-linux-musl` | `make pi` | 静态 | `scp` 过去直接跑,不依赖 Pi 的 glibc 版本 |
| Raspberry Pi 4/5 (64-bit, glibc) | `aarch64-unknown-linux-gnu` | `make pi-gnu` | 动态 | 二进制更小,需与 Pi 的 glibc 版本匹配 |
| Rock 5 / Orange Pi / Jetson | `aarch64-unknown-linux-musl` | `make pi` | 静态 | 同 Pi 64-bit |
| Pi Zero / Pi 3 (32-bit raspios) | `armv7-unknown-linux-musleabihf` | `make pi32` | 静态 | 32-bit ARM hard-float |
| Linux 服务器 / Docker | `x86_64-unknown-linux-musl` | `make linux-x86` | 静态 | Alpine 镜像 / scratch 镜像友好 |
| macOS host | 默认 | `make build` | 动态 | 开发 / 本地测 |
| iOS / Android | UniFFI bundle | 见 §iOS / Android | — | M2 / M3 路线图 |

## 交叉编译工具链

跨平台编译推荐 [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) —— 一次 `brew install zig`,所有 Linux target 的 linker 都齐了,macOS 上也能直出 musl 静态 binary。

```bash
# 一次性准备
cargo install cargo-zigbuild
brew install zig        # macOS;Linux 上用 apt / 官方 tarball 都行

# 之后每个新 target 再加一次
rustup target add aarch64-unknown-linux-musl
```

Makefile 里 `make pi` 等目标会自动跑 `rustup target add` 和 zigbuild,不需要手动记命令。

## Build profile

```
cargo build --workspace                    # debug,带 symbol,编译快
cargo build --workspace --release          # 标准 release,带 symbol
cargo build --workspace --profile=min      # 极小 binary(默认 cross 用这个)
```

`min` profile(workspace Cargo.toml 里定义):`opt-level = "z"` + `lto = true` + `codegen-units = 1` + `panic = "abort"` + `strip = true`。代价是编译慢 3–5×、没有 unwind backtrace,产出 3 MB 左右的 aarch64 静态 binary。

要用 release 而不是 min:`make pi PROFILE=release`。

## Pi 部署示例

```bash
# 1) 在开发机
make pi
# → target/aarch64-unknown-linux-musl/min/muagent  (~3 MB, static)

# 2) 推到 Pi 并跑
scp target/aarch64-unknown-linux-musl/min/muagent pi@raspberrypi.local:~/
ssh pi@raspberrypi.local './muagent --help'
```

systemd 服务 / Docker 镜像的 wrapper 后续按需再补。

## 验证交叉产物

```bash
file target/aarch64-unknown-linux-musl/min/muagent
# → ELF 64-bit LSB executable, ARM aarch64, statically linked, ...

# 动态依赖(musl 静态应为空):
objdump -p target/aarch64-unknown-linux-musl/min/muagent | grep NEEDED || echo "static OK"
```

## iOS / Android

走 UniFFI bundle,不是 CLI 二进制:

- **iOS**:`cargo build --target aarch64-apple-ios-sim` / `aarch64-apple-ios` 产 `.a`,然后 `uniffi-bindgen swift` 生成 Swift binding,打成 XCFramework。
- **Android**:`cargo ndk -t arm64-v8a -t armeabi-v7a ...` + `uniffi-bindgen kotlin`,产物是 `.so` + Kotlin binding,打成 AAR。

详见路线图 §M2 / §M3(`design/11-roadmap.md`)。M0 阶段不打 mobile bundle。

## 不在目标范围

- MCU(Cortex-M,< 1 MB RAM):TLS + JSON + FSM 本身就吃不下;没这个需求就直接写 MQTT firmware。μAgent 内核是 `std`-only,不提供 `no_std` 编译路径。
- WASM:可编译,但 `tokio::time` / `std::fs` 等在浏览器里不通,host 需要自己换 `SessionStore` / `NetEgress` / `Clock` 三个 trait 的实现。没作为一等 target 维护。
