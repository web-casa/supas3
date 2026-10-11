# supaS3

A native multi-cloud object storage transfer workbench, built with Rust and GPUI.

面向多云对象存储的原生文件传输工作台，使用 Rust 与 GPUI 构建。

> **Status:** early development. There is no usable release yet.
>
> **状态：** 早期开发阶段，尚无可用版本。

## Goals

- **Platforms:** macOS (Apple Silicon), Windows (x64, ARM64), Linux (x64, ARM64)
- **Storage:** Amazon S3 and S3-compatible services, Alibaba Cloud OSS, Tencent Cloud COS, Huawei Cloud OBS
- **Languages:** English, 简体中文
- Dual-pane workspace with independent tabs, a transfer queue, cross-cloud copy, lightweight preview and small-file editing

## Building

Requires the Rust toolchain named in `rust-toolchain.toml`; `rustup` installs it automatically.

```sh
cargo run -p supas3-app        # start the application
cargo test --workspace        # run the tests
```

On Debian or Ubuntu, install the system libraries first:

```sh
sudo apt-get install gcc g++ clang pkg-config libfontconfig-dev libwayland-dev \
  libxkbcommon-x11-dev libx11-xcb-dev libvulkan1
```

## Layout

| Path | Contents |
|---|---|
| `crates/supas3-app` | The desktop application (GPUI Kit). The binary is named `supas3`. |
| `crates/supas3-core` | Storage and transfer backend, with no UI dependency. |
| `crates/supas3-probe` | A command-line tool that checks how an S3-compatible service answers the calls supaS3 relies on. |

## Acknowledgements

The storage and transfer backend is derived from [Brows3](https://github.com/rgcsekaraa/brows3), which is MIT licensed. Its license notice is kept in [LICENSES/Brows3-MIT.txt](LICENSES/Brows3-MIT.txt).

## License

supaS3 is licensed under [AGPL-3.0-or-later](LICENSE).
