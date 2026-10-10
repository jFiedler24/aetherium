# Customer Requirements

Items in this file are **owned by the customer** — they record what was
actually asked for, in the customer's words (lightly edited for clarity).
Team convention: `creq` items are only ever *covered* by product features
(`feat`); they are never reworded, split, or renamed here. Derived
requirements live in the other files in this directory and point back to
the `creq` they implement. Sources: `FeaturesAndBugs.md` and direct
requests.

## creq~ssh-terminal-for-homelab~1

I want one app to work on my remote machines (Raspberry Pi, phone via
SSH, other boxes on my LAN) with saved connection profiles — click
Connect, get a terminal, done.

**Tags:** core, usability

## creq~login-without-typing-passwords~1

Connecting must work without typing a password every time — use the
system's SSH keys and agent, like the normal `ssh` command does. On
Windows that means both the OpenSSH agent and Pageant.

**Tags:** auth, windows

## creq~remote-files-like-a-file-manager~1

I want to browse the remote file system graphically: see directories and
files with sizes, drag files between my desktop and the remote, move
things around, download and upload — like a file manager, not scp
commands.

**Tags:** files

## creq~watch-remote-logs-like-snaketail~1

Watching remote log files must feel like SnakeTail: open a log, it
follows new lines, errors and warnings are highlighted, I can pause, and
I can filter noisy logs down to the lines I care about.

**Tags:** logs

## creq~slick-modern-ui~1

The app should look modern and slick: a nice dark theme (light too),
icons instead of text buttons where they make sense, and it must not
look broken (no stray lines, no odd spacing).

**Tags:** ui

## creq~rest-api-for-ai~1

Expose a localhost REST interface that makes it super easy for an AI to
control a running instance — open a log-follow view, execute commands on
a target, replace/upload/download a certain file — and advertise the
interface via the CLI.

**Tags:** integration, ai

## creq~windows-parity~1

On Windows everything must work like on the Mac: no console window next
to the UI, drag-out of files, and the same login-without-passwords
support.

**Tags:** windows

## creq~edit-remote-files-with-local-editor~1

Double-clicking a remote file must open it in my local editor (like
MobaXterm): edit, save, and the tool asks whether to sync the changes
back to the device.

**Tags:** files, integration

## creq~zoom-terminal-text~1

I want to be able to increase and decrease the text size in the terminal.

**Tags:** usability, terminal

## creq~colorize-uncolored-terminal~1

For uncolored terminals the tool should do the coloring — output from
programs that print no ANSI colors of their own should still get readable
syntax colors.

**Tags:** terminal
