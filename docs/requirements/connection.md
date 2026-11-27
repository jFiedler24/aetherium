# Connections and Authentication

Derived from the customer requirements — features cover `creq`s,
requirements refine features, code and tests tag back to requirements.

## feat~connection-profiles~1

The app shall offer named connection profiles (host, port, username,
auth method) that persist between launches, with one-click connect from
a sidebar.

**Covers:** creq~ssh-terminal-for-homelab~1

**Needs:** req, impl

## req~profile-storage~1

Profiles shall be stored on disk (TOML) with passwords/passphrases
encrypted at rest under the user's config directory.

**Covers:** feat~connection-profiles~1

**Needs:** impl

## req~known-hosts-verification~2

On connect, the server's host key shall be verified against the user's
`known_hosts`: unknown keys are learned (trust-on-first-use); a changed
key renews the stale entry automatically — every recorded entry for that
host is replaced with the presented key and the connect proceeds. Only
when the stale entry cannot be replaced (e.g. `known_hosts` is not
writable) is the changed key rejected, in which case the UI offers the
one-click accept flow described below.

**Covers:** feat~connection-profiles~1

**Needs:** impl

## req~connect-error-visibility~1

When a connection attempt fails — including host-key verification
failure, DNS/connection errors, and authentication failure — the error
text shall be shown in the UI (status bar and in-app log), not only in
a stderr or log-file diagnostic. A failed tab keeps its error message
instead of reverting to a generic "disconnected", and stays
reconnectable.

**Covers:** feat~connection-profiles~1

**Needs:** impl

## req~accept-changed-host-key~1

When verification fails because the recorded host key changed and the
stale entry could not be renewed automatically (e.g. `known_hosts` is
not writable), the app shall show the new key's fingerprint and offer a
one-click way to remove the stale known_hosts entry and reconnect — the
`ssh-keygen -R` plus trust-on-first-use equivalent.

**Covers:** feat~connection-profiles~1

**Needs:** impl

## feat~silent-key-auth~1

Connecting shall first try key-based authentication without prompting
(ssh-agent, then default key files), falling back to the stored
password only when key auth does not succeed — mirroring OpenSSH's
default order.

**Covers:** creq~login-without-typing-passwords~1

**Needs:** req, impl

## req~agent-auth~1

Agent authentication shall support the platform ssh-agent: Unix domain
socket via `SSH_AUTH_SOCK`, and on Windows the OpenSSH agent named pipe
falling back to Pageant.

**Covers:** feat~silent-key-auth~1

**Needs:** impl

## req~default-key-fallback~1

When no agent identity is accepted, the app shall try the default
private keys `id_ed25519`, `id_rsa`, `id_ecdsa` from the user's `.ssh`
directory before using a stored password.

**Covers:** feat~silent-key-auth~1

**Needs:** impl

## req~none-auth-fallback~1

When neither agent nor default keys authenticate, the app shall attempt
SSH "none" authentication (the implicit first method of OpenSSH),
which gadget devices such as BeagleBone boards accept for root.

**Covers:** feat~silent-key-auth~1

**Needs:** impl

## feat~windows-parity~1

On Windows the app shall behave like the macOS build: GUI subsystem
without a console window, working agent-based passwordless login, and
file drag-out to Explorer.

**Covers:** creq~windows-parity~1, creq~login-without-typing-passwords~1

**Needs:** req, impl

## req~windows-gui-subsystem~1

The Windows binary shall be a GUI-subsystem application; diagnostics
shall be written to a log file because stderr is unavailable.

**Covers:** feat~windows-parity~1

**Needs:** impl

## req~detailed-log-timestamps~1

Every application diagnostic line, including each physical line of a
multiline message, and every in-app tool-log entry shall begin with an
ISO 8601 local timestamp including milliseconds and the UTC offset.

**Covers:** feat~windows-parity~1

**Needs:** impl, utest

## req~windows-drag-out~1

Dragging a remote file out of the app window on Windows shall offer it
to the OS via an OLE drag (CF_HDROP) built from the staged temp download.

**Covers:** feat~windows-parity~1

**Needs:** impl
