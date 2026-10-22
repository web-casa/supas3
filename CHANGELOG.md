# Changelog

## Unreleased

- Copy the address of files, folders and buckets, or the public web address of files, to the clipboard.
- Edit an object's user metadata; entries left as they are stay as they are.
- Make every object in a folder private or public at once, as a task that can be cancelled.
- Import files into a bucket from web addresses.
- A download cut off by closing supaS3 can be retried, from the start, to the same file.
- AppImage and RPM packages for Linux, x64 and ARM64.
- Create and delete buckets. Deleting asks for the bucket's name and refuses while editors or transfers in it are open.
- Turn versioning on or pause it, and edit the bucket policy as JSON, from the details panel of a bucket. Services that refuse these show why.
- Version history, restore, download and delete of versions are now tested on Tencent Cloud COS and MinIO.
- Uploads, downloads and copies that fail on a network error are retried up to three times. An upload is retried only while the target is still as it was before, and its unfinished parts are removed first.
- Copy and move to the other pane with Cmd/Ctrl+Shift+C and Cmd/Ctrl+Shift+M.
- Files under 100 MiB upload many times faster to distant services: they are read in 1 MiB pieces instead of 4 KiB.
- Uploads of large files no longer fail when the network pauses for a few seconds.
- The sidebar folds to icons when the details panel leaves little room; dialogs have consistent widths; field errors show beside their field.

## 0.2.0

- New folder, and rename of files and folders, in storage and on this computer.
- Move to the other pane, from the menu or by dragging with Command (macOS) or Shift (Windows and Linux) held. Each original is removed only after its copy has been checked.
- When a copy's target name is taken: skip, keep both under a new name, or replace only while the existing item is unchanged.
- Copy between folders on this computer; deleting there moves items to the recycle bin.
- Sort by name, size or date; filter the rows shown; search names below a location in a bucket.
- A details panel (Cmd/Ctrl+I), a sidebar that collapses to icons (Cmd/Ctrl+B), keyboard movement between the sidebar and the panes, and menu entries for new folder, rename and filter.
- Text files open in a tab of the pane, up to 8 MiB; quitting or closing asks about unsaved edits.
- Download a chosen version of an object, or delete a version for good.
- A preset for IDrive e2: copying, saving and syncing now work there.
- A message that explains Alibaba Cloud's refusal of mainland-China buckets through the public endpoint for newer accounts.
- Transfer settings: how many transfers run at once, and a speed limit for each transfer.
- Change an object's Content-Type from the details panel, only while the object still has the content shown, and for objects up to 5 GiB.
- See whether an object is private or public, and change it, from the details panel. IDrive e2 refuses changes to object permissions, so there they are only shown.
- Zoom in image previews: fit, actual size, and steps between 25% and 400%.
- Connect to Amazon S3 with a profile of this computer's AWS configuration (~/.aws), whose keys stay there.
- The Windows installer follows the system language: English or Simplified Chinese.

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
