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

## req~known-hosts-verification~1

On connect, the server's host key shall be verified against the user's
`known_hosts`; unknown keys are learned (trust-on-first-use), changed
keys are rejected.

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

## req~windows-drag-out~1

Dragging a remote file out of the app window on Windows shall offer it
to the OS via an OLE drag (CF_HDROP) built from the staged temp download.

**Covers:** feat~windows-parity~1

**Needs:** impl
