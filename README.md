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

## Acknowledgements

The storage and transfer backend is derived from [Brows3](https://github.com/rgcsekaraa/brows3), which is MIT licensed. Its license notice is kept in [LICENSES/Brows3-MIT.txt](LICENSES/Brows3-MIT.txt).

## License

supaS3 is licensed under [AGPL-3.0-or-later](LICENSE).
