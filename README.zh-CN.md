# supaS3

一款原生桌面应用，用来浏览对象存储，并在本机与存储之间、或两个存储服务之间传输文件。使用 Rust 与 [GPUI Kit](https://github.com/longbridge/gpui-kit) 构建。

[English](README.md)

> **状态：** 版本 0.2.0。已在下表所列服务的真实存储桶上测试，但它仍是新软件：不能丢失的数据请另留副本。

## 功能

- 左右两个面板，各有独立的标签页。任一面板都可以显示本机文件夹或任一连接中的位置。
- 上传、下载、在同一连接内复制、在两个连接之间复制。跨连接复制会经本机中转，开始前会说明。
- 把条目移动到另一侧面板：用菜单，或拖拽时按住 Command（macOS）或 Shift（Windows、Linux）。每一项的原件在复制结果校验通过后才删除；无法校验的项保留原件。
- 新建文件夹、重命名，存储与本机都支持。存储中的文件夹在后台逐个对象改名。
- 传输列表，显示进度，可取消与重试。
- 删除文件与文件夹，执行前需确认；本机文件移到回收站。
- 按名称、大小或日期排序，筛选显示的行，在存储桶的某个位置下按名称搜索。
- 在标签页中打开不超过 8 MiB 的文本文件查看或编辑；查看 PNG、JPEG 与简单格式的 WebP 图片，可缩放。退出或关闭窗口时会询问未保存的修改。
- 在详情面板中修改对象的内容类型与用户元数据，查看或修改对象是私有还是公开读取；一次把文件夹中的所有对象设为私有或公开读取。
- 从网址把文件导入到存储桶。
- 复制文件、文件夹、存储桶的地址，或文件的公开网址。
- 设置同时进行的传输数，以及每个传输的速度上限。
- 为对象生成签名链接，有效期 1 小时、1 天或 7 天。
- 查看对象的版本历史，下载指定版本；在已开启版本控制的存储桶上可恢复旧版本或永久删除某个版本。
- 把本机文件夹同步到存储：先比较，列出将上传的内容，再开始。不会删除存储中的任何对象。
- 显示选中项详情的侧边面板、可收起为图标的侧栏，可用键盘在侧栏与两个面板之间移动。
- 简体中文与英文界面，浅色与深色外观。

复制时遇到目标已有同名项，会询问跳过、保留两者（使用新名称）还是替换。替换与保存编辑、同步变化的文件一样，只在被替换的文件或对象自用户看到以来没有变化时才进行。移动只提供跳过与保留两者。在部分服务和操作上，这些检查是写入前单独发出的一次请求，其间他人所做的改动无法察觉；连接属于这种情况时，编辑器会给出提示。

## 存储服务

| 服务 | 状态 |
|---|---|
| 阿里云 OSS | 已在真实存储桶上测试 |
| 腾讯云 COS | 已在真实存储桶上测试 |
| Cloudflare R2（作为通用 S3 兼容服务） | 已在真实存储桶上测试 |
| IDrive e2 | 已在真实存储桶上测试，使用专门的预设。复制、保存与替换为先检查后写入，并非原子操作。同步只有在文件由 supaS3 上传之后，才能识别其未变化 |
| 华为云 OBS | 按华为云文档实现，未经测试 |
| Amazon S3 | 本版本未在真实存储桶上测试。可输入访问密钥，或使用本机 AWS 配置中的配置 |

国内云服务均通过其 S3 兼容接口访问。阿里云要求 2025 年 3 月 20 日之后开通 OSS 的账户经绑定到存储桶的自定义域名访问中国内地的存储桶；supaS3 目前还不能使用这种域名，OSS 因此拒绝请求时会给出说明。版本历史只在未开启版本控制的存储桶上测试过；恢复版本按各服务的文档实现，未经测试。

## 安装

在[发布页面](https://github.com/web-casa/supas3/releases)下载与你的电脑对应的文件。`SHA256SUMS.txt` 列出了每个文件的校验和。

| 电脑 | 文件 |
|---|---|
| macOS，Apple 芯片 | `supas3-<版本>-macos-arm64.dmg` |
| Windows，x64 | `supas3-<版本>-windows-x64-setup.exe`，或免安装的 `.zip` |
| Windows，ARM64 | `supas3-<版本>-windows-arm64-setup.exe`，或 `.zip` |
| Linux，x64 | `supas3_<版本>_amd64.deb`、`supas3-<版本>-1.x86_64.rpm`、`supas3-<版本>-linux-x64.AppImage`，或 `.tar.gz` |
| Linux，ARM64 | `supas3_<版本>_arm64.deb`、`supas3-<版本>-1.aarch64.rpm`、`supas3-<版本>-linux-arm64.AppImage`，或 `.tar.gz` |

安装包没有使用开发者证书签名。

- **macOS：** 打开磁盘映像，把 supaS3 拖到“应用程序”。首次打开会被系统阻止；请在“系统设置 → 隐私与安全性”中选择“仍要打开”，或运行 `xattr -dr com.apple.quarantine /Applications/supaS3.app`。
- **Windows：** SmartScreen 可能提示无法识别的应用；选择“更多信息 → 仍要运行”。安装程序只为当前用户安装，不需要管理员权限。
- **Linux：** Debian、Ubuntu 用 `sudo apt install ./supas3_<版本>_<架构>.deb`；Fedora 等用 `sudo dnf install ./supas3-<版本>-1.<架构>.rpm`；AppImage 加上执行权限后直接运行（`chmod +x supas3-*.AppImage`）。各种包都需要 glibc 2.39 或更新（例如 Ubuntu 24.04、Debian 13、Fedora 40 及以后）。应用需要可用的 Vulkan 驱动；没有显卡的机器可以安装 `mesa-vulkan-drivers`。

访问密钥保存在系统的凭据存储中（钥匙串、凭据管理器，或 Linux 上的 Secret Service）。没有可用的凭据存储时，密钥只保留到应用关闭。

## 从源码构建

需要 `rust-toolchain.toml` 指定的 Rust 工具链，`rustup` 会自动安装。

```sh
cargo run -p supas3-app        # 启动应用
cargo test --workspace        # 运行测试
```

在 Debian 或 Ubuntu 上，先安装系统库：

```sh
sudo apt-get install gcc g++ clang pkg-config libfontconfig-dev libwayland-dev \
  libxkbcommon-x11-dev libx11-xcb-dev libdbus-1-dev libvulkan1
```

`packaging/` 下的脚本把发布版二进制制作成上述安装包，调用方式见 `.github/workflows/ci.yml`。

## 目录

| 路径 | 内容 |
|---|---|
| `crates/supas3-app` | 桌面应用，二进制名为 `supas3`。 |
| `crates/supas3-core` | 存储与传输后端，不依赖界面。 |
| `crates/supas3-probe` | 命令行工具，检查某个 S3 兼容服务对 supaS3 所依赖调用的响应。 |
| `packaging/` | 图标与各平台的打包脚本。 |

## 致谢

存储与传输后端源自 [Brows3](https://github.com/rgcsekaraa/brows3)（MIT 许可证）。其许可声明保留在 [LICENSES/Brows3-MIT.txt](LICENSES/Brows3-MIT.txt)，[UPSTREAM.md](UPSTREAM.md) 记录了取用与改动的内容。界面图标来自 [Lucide](https://lucide.dev)。其他程序库见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)；每个安装包都在 `THIRD_PARTY_LICENSES.txt` 中附有它们的许可证全文。

## 许可

supaS3 是自由软件，以 [AGPL-3.0-or-later](LICENSE) 许可发布。
