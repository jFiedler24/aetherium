# Remote Files

## feat~file-tree~1

The app shall show the remote file system as a lazily loaded tree with
directory disclosure, file sizes, selection, and context-menu actions.

**Covers:** creq~remote-files-like-a-file-manager~1

**Needs:** req, impl

## req~sftp-operations~1

Directory listing, rename/move, recursive delete, create file/dir, and
single/directory transfers shall work over SFTP without blocking the
terminal session.

**Covers:** feat~file-tree~1

**Needs:** impl

## feat~transfer-progress~1

Uploads and downloads shall show a graphical progress bar with size,
speed, ETA, and a cancel button.

**Covers:** creq~remote-files-like-a-file-manager~1

**Needs:** impl

## feat~os-file-drag-out~1

Dragging a remote file from the tree to the OS desktop or a file
manager shall download it to a temp file in the background and hand the
real local path to the OS once staged.

**Covers:** creq~remote-files-like-a-file-manager~1

**Needs:** impl

## feat~os-file-drop-in~1

Dropping local files onto the tree or terminal shall upload them into
the target directory (or the session's home directory).

**Covers:** creq~remote-files-like-a-file-manager~1

**Needs:** impl

## feat~remote-edit-writeback~1

Double-clicking a remote file shall open a staged local copy in the
user's editor; when the copy is saved, the app asks whether to upload
the changes back to the device.

**Covers:** creq~edit-remote-files-with-local-editor~1

**Needs:** impl
