# supaS3

A native desktop application for browsing object storage and moving files between this computer and storage, or from one storage service to another. Built with Rust and [GPUI Kit](https://github.com/longbridge/gpui-kit).

[简体中文](README.zh-CN.md)

> **Status:** version 0.2.0. It has been tested against real buckets on the services listed below, but it is new software: keep a copy of anything you cannot afford to lose.

## What it does

- Two panes side by side, each with its own tabs. Either pane shows a folder on this computer or a location in any connection.
- Upload, download, copy within a connection, and copy between two connections. A copy between connections passes through this computer and says so before it starts.
- Move items to the other pane, from the menu or by dragging with Command held on macOS or Shift on Windows and Linux. Each original is removed only after its copy has been checked; anything that cannot be checked keeps its original.
- New folder and rename, in storage and on this computer. A folder in storage is renamed object by object, in the background.
- A transfer list with progress, cancel and retry.
- Delete files and folders, after a confirmation. On this computer they go to the recycle bin.
- Sort by name, size or date, filter the rows shown, and search names below a location in a bucket.
- Open text files of up to 8 MiB in a tab to read or edit them, and view PNG, JPEG and simple WebP images with zoom. Quitting or closing the window asks about unsaved edits.
- Change an object's Content-Type, and see or change whether it is private or public, from the details panel.
- Set how many transfers run at once and a speed limit for each transfer.
- Make a signed link to an object that expires after an hour, a day or a week.
- Show an object's version history, download a chosen version, and restore an older version or delete a version for good on buckets with versioning turned on.
- Sync a folder on this computer to storage: compare first, see what would be uploaded, then start. Nothing in storage is deleted.
- A details panel for the selected item, a sidebar that collapses to icons, and keyboard movement between the sidebar and the panes.
- English and 简体中文, light and dark appearance.

When a copy finds a name already taken, it asks whether to skip the item, keep both under a new name, or replace the existing file or object. A replacement, like saving an edited file or syncing a changed file, goes ahead only if what it replaces has not changed since it was seen. A move offers only skip and keep both. On some services and operations these checks are a separate request made just before the write, so a change made by someone else in between is not noticed; the editor says so when that applies to the connection.

## Storage services

| Service | State |
|---|---|
| Alibaba Cloud OSS | Tested against a real bucket |
| Tencent Cloud COS | Tested against a real bucket |
| Cloudflare R2 (as a generic S3-compatible service) | Tested against a real bucket |
| IDrive e2 | Tested against a real bucket, with its own preset. Copying, saving and replacing check first and then write, which is not atomic. Sync recognises unchanged files only after supaS3 itself uploaded them |
| Huawei Cloud OBS | Implemented from Huawei's documentation, not tested |
| Amazon S3 | Not tested against a real bucket in this version. Connects with typed keys or with a profile of this computer's AWS configuration |

The domestic cloud services are used through their S3-compatible interface. Alibaba Cloud requires accounts that started using OSS after 20 March 2025 to reach buckets in mainland China through a custom domain bound to the bucket; supaS3 cannot use such a domain yet, and says so when OSS refuses a request for this reason. Version history has been tested only on buckets without versioning; restoring a version follows the services' documentation and is untested.

## Install

Download the file for your computer from the [releases page](https://github.com/web-casa/supas3/releases). `SHA256SUMS.txt` lists the checksum of each file.

| Computer | File |
|---|---|
| macOS, Apple Silicon | `supas3-<version>-macos-arm64.dmg` |
| Windows, x64 | `supas3-<version>-windows-x64-setup.exe`, or the `.zip` to run without installing |
| Windows, ARM64 | `supas3-<version>-windows-arm64-setup.exe`, or the `.zip` |
| Linux, x64 | `supas3_<version>_amd64.deb`, or the `.tar.gz` |
| Linux, ARM64 | `supas3_<version>_arm64.deb`, or the `.tar.gz` |

The packages are not signed with a developer certificate.

- **macOS:** open the disk image and drag supaS3 to Applications. The first time, macOS refuses to open it; open *System Settings → Privacy & Security* and choose *Open Anyway*, or run `xattr -dr com.apple.quarantine /Applications/supaS3.app`.
- **Windows:** SmartScreen may warn about an unrecognized app; choose *More info → Run anyway*. The installer installs for the current user and needs no administrator rights.
- **Linux:** `sudo apt install ./supas3_<version>_<arch>.deb`. The application needs a working Vulkan driver; on machines without a GPU, `mesa-vulkan-drivers` provides one.

Access keys are kept in the system's credential store (Keychain, Credential Manager, or the Secret Service on Linux). Where none is available, a key is kept only until the application closes.

## Build from source

Requires the Rust toolchain named in `rust-toolchain.toml`; `rustup` installs it automatically.

```sh
cargo run -p supas3-app        # start the application
cargo test --workspace        # run the tests
```

On Debian or Ubuntu, install the system libraries first:

```sh
sudo apt-get install gcc g++ clang pkg-config libfontconfig-dev libwayland-dev \
  libxkbcommon-x11-dev libx11-xcb-dev libdbus-1-dev libvulkan1
```

The scripts under `packaging/` turn a release binary into the packages above; `.github/workflows/ci.yml` shows how they are called.

## Layout

| Path | Contents |
|---|---|
| `crates/supas3-app` | The desktop application. The binary is named `supas3`. |
| `crates/supas3-core` | Storage and transfer backend, with no UI dependency. |
| `crates/supas3-probe` | A command-line tool that checks how an S3-compatible service answers the calls supaS3 relies on. |
| `packaging/` | Icons and the scripts that build the packages for each platform. |

## Acknowledgements

The storage and transfer backend is derived from [Brows3](https://github.com/rgcsekaraa/brows3), which is MIT licensed. Its license notice is kept in [LICENSES/Brows3-MIT.txt](LICENSES/Brows3-MIT.txt), and [UPSTREAM.md](UPSTREAM.md) records what was taken and changed. The interface icons are [Lucide](https://lucide.dev) icons. Other libraries are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md); every package carries the full text of their licenses in `THIRD_PARTY_LICENSES.txt`.

## License

supaS3 is free software, licensed under [AGPL-3.0-or-later](LICENSE).
