# Security policy

## Supported versions

`v0.1.x` is the current release line.

## Report a vulnerability

Do not open a public issue for a security report.

Use GitHub private security advisories:

https://github.com/holdfast-labs/wgrep/security/advisories/new

Include:

- the `wg` version and platform;
- steps to reproduce;
- the impact;
- any relevant logs, with secrets removed.

The maintainers will review the report and coordinate a fix and disclosure with
you.

## Scope

`wg` runs a local binary and a loopback-only daemon. Reports about the daemon,
its metadata and token files, path handling, the text index, the semantic
index, and release archives are in scope.

Do not include secrets, private source trees, or model credentials in a report.
Use a minimal reproduction.
