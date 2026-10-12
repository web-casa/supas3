# Changelog

## 0.1.0

The first public version.

- Two panes with tabs, showing folders on this computer and locations in storage connections.
- Connections to Alibaba Cloud OSS, Tencent Cloud COS, Huawei Cloud OBS, Amazon S3 and other S3-compatible services, with access keys kept in the system's credential store.
- Upload, download, copy within a connection, copy between connections, and delete, with a transfer list that shows progress and allows cancelling and retrying.
- Viewing and editing of text files; viewing of PNG, JPEG and simple WebP images.
- Signed links that expire, version history with restore, and one-way sync of a local folder to storage.
- English and Simplified Chinese; light and dark appearance.
- Packages for macOS (Apple Silicon), Windows (x64 and ARM64) and Linux (x64 and ARM64).

Known limitations:

- The packages are not signed with a developer certificate; see the README for the first launch on macOS and Windows.
- Huawei Cloud OBS and Amazon S3 have not been tested against real buckets. Restoring a version has not been tested on any service.
- On IDrive e2 buckets that report object owners without an ID, copying, saving an edited file and replacing a file during sync fail without writing anything.
- Uploads and copies refuse to replace an object that already exists, and there is no option to replace it. On some services the check is a separate request before the write, so it cannot rule out a change made in between.
