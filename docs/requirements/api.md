# REST API

## feat~rest-api~1

A self-describing HTTP API on localhost shall let an AI or local
scripts control the running app: discover targets, open shell and
log-follow tabs, run commands, and read/replace remote files.

**Covers:** creq~rest-api-for-ai~1

**Needs:** req, impl

## req~api-transport~1

The API shall listen on 127.0.0.1 only, on a configurable port
(`AETHERIUM_API_PORT` or 48920..48925 fallback), hand-rolled HTTP/1.1
with JSON responses, and advertise its URL and token location in the
startup log and status bar.

**Covers:** feat~rest-api~1

**Needs:** impl

## req~api-token-auth~1

Every endpoint except the self-describing index and health check shall
require a bearer token persisted in the config directory.

**Covers:** feat~rest-api~1

**Needs:** impl

## req~api-drives-ui-sessions~1

API operations shall run on the UI thread against the visible tabs'
existing SSH sessions — no hidden parallel connections.

**Covers:** feat~rest-api~1

**Needs:** impl
