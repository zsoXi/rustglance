# Security policy

## Reporting a vulnerability

Please do not open a public issue. Email zxdubx@gmail.com with what you found,
how to reproduce it, and the version or commit you used.

## Supported versions

Only the latest version on the default branch gets fixes.

## What counts as a vulnerability

- A write that leaves the driven tab although the config blocks it.
- A file read through `upload` from outside the upload folder.
- A request to the live view that gets past its Host and Origin checks.
- A tool that runs JavaScript written by the agent.
- A password that reaches the agent or the journal through `tree` or `type`.

## Known limits

The limits in [Security model and limits](README.md#security-model-and-limits)
are known and are not vulnerabilities on their own.
