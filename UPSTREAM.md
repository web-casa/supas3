# Upstream

The storage and transfer backend in `crates/supas3-core` is derived from Brows3.

| | |
|---|---|
| Repository | https://github.com/rgcsekaraa/brows3 |
| Baseline commit | `24b02bd0aea2bf2bbfb6ecb0b02cc35586700cd8` (v0.2.46, 2026-09-25) |
| License | MIT, see [LICENSES/Brows3-MIT.txt](LICENSES/Brows3-MIT.txt) |
| Imported | `src-tauri/src/` → `crates/supas3-core/src/` |
| Not imported | The Next.js frontend, Tauri configuration, icons, build script and release tooling |

The import is a one-time copy. Upstream fixes are ported by hand when needed.

## Changes from upstream

- Tauri is removed. `src/host.rs` supplies small stand-ins for the Tauri types the backend used (`State`, `AppHandle`, `async_runtime`) so the function bodies stay as close to upstream as possible.
- `src/lib.rs` is rewritten: the Tauri builder, window, menu and plugin setup are replaced by `Core::init`.
- Product identity (configuration directory, keychain service, environment variables) is renamed from Brows3 to supaS3.
- `Core::shutdown` stops the scheduler, cancels transfers and releases shared state; `start_scheduler` returns its task handle for this purpose.
- `TransferManager::try_add_jobs` reports a journal failure to the caller. The command functions use it, so queueing a transfer that could not be saved returns an error instead of success.
- Custom endpoints carry compatibility settings (`credentials/compat.rs`): addressing style, listing version and write guard, with presets for Alibaba Cloud OSS, Tencent Cloud COS and Huawei Cloud OBS.
- `s3/guard.rs` adapts overwrite protection to services whose conditional writes differ from S3, and adds `Content-MD5` to batch deletes on custom endpoints. `s3/listing.rs` can page with ListObjects markers where ListObjectsV2 is unavailable.
- Region detection is skipped for custom endpoints.
- `credentials::with_profile` runs an operation against a named connection instead of the globally selected one, so two panes can use different connections at once. Queued transfers are tied to the connection revision they were queued with.
- Secrets are never written to a plaintext file: without a system keychain they are kept in memory for the session. Temporary credentials (session tokens) are supported.
- A lock file keeps a second instance out of the same data directory.

To see every change to the imported files, compare `crates/supas3-core/src` with `src-tauri/src` at the baseline commit.
