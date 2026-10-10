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

## req~tree-parent-navigation~1

The file tree shall offer an "up one level" (`..`) row whenever the
current root has a parent directory; activating it re-roots the tree at
the parent, keeping the previous root visible and expanded inside it.

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

## feat~folder-zip-download~1

Right-clicking a remote directory shall offer "Download as ZIP": the
whole folder structure streams into a single zip archive in ~/Downloads,
preserving hierarchy and empty directories.

**Covers:** creq~folder-download-as-zip~1

**Needs:** req, impl

## req~folder-zip-progress~1

The ZIP download shall walk the tree first so the transfer progress bar
shows byte totals, stream files without holding them in memory, honor
the transfer cancel, and remove the partial archive on failure.

**Covers:** feat~folder-zip-download~1

**Needs:** impl, utest

## feat~chmod-ui~1

Right-clicking any tree entry shall offer "Permissions…": a dialog
showing the current mode and a user/group/other × read/write/exec grid,
with an octal readout and a recursive option for directories.

**Covers:** creq~chmod-from-filetree~1

**Needs:** req, impl

## req~chmod-operations~1

Applying permissions shall read each entry's current mode and replace
only the 0o777 bits (file-type bits preserved), walking subdirectories
depth-first when recursive is requested; the dialog's checkbox grid maps
to and from the nine permission bits exactly.

**Covers:** feat~chmod-ui~1

**Needs:** impl, utest
