---
name: rustglance
description: Use a logged-in Chromium through the rustglance MCP server (tools open, tree, tap, type, upload, download, screenshot, logs, get, check). Use when a task means reading or acting on a web app in a browser a human already logged in to, checking what a page shows, collecting screenshots as evidence, or reading the app's JSON API with the browser's session.
---

# rustglance

rustglance drives a Chromium; when a site needs an account, a human logged it
in. You reach it through MCP tools. You never log in and never handle
passwords.

## Before you start

1. For a site behind a login, a human ran `rustglance login --config <file>`
   and logged in. Public sites need no login. If a call is refused with
   `logged_out`, stop and ask the human to log in.
2. Writes (any request other than GET, HEAD, OPTIONS) from the driven tab are
   blocked unless the config allows them. A result with `blocked_writes` means
   the page tried to save something and was stopped. Report it; do not look
   for a way around.

## The loop

1. `open` a URL or a path. It waits until the page goes quiet.
2. `tree` gives the page outline with refs like `e12`. Use `find` to keep only
   lines with a given text, e.g. `{"find": "Save"}`.
3. `tap` a `ref` (or visible `text`), `type` into a `ref` (`submit: true`
   adds Enter). Each action waits for the page to settle.
4. `check` waits for a condition: `text` visible, `gone`, `url` contains.
5. `screenshot` saves a PNG as evidence (`name` tags the file). Add
   `look: true` only when you must see the image; images fill your context.

Refs belong to one page state. After the page changes, call `tree` again.

## Reading data

`get` reads the app's API with the browser's session:
`{"path": "/api/items", "query": {"limit": 20}}` returns `status` and `body`
(already parsed JSON).

- `fields: ["id", "name"]` keeps only those keys in each item of the body's
  lists. Use it for long lists.
- A body larger than `get_max_bytes` (default 16 KB) keeps only the first
  items of its lists. `truncated` says how many were kept of how many, and
  `file` names the whole body saved in the evidence folder. Narrow `query`
  or `fields` instead of guessing the rest.
- Prefer `get` over reading numbers off the screen when the app has an API.

## Refusals

A refused call returns `error` (the class), `reason` and `next`. Follow `next`.

| `error` | What to do |
|---|---|
| `stale` | the ref is gone; call `tree` and use a fresh ref |
| `bad_args` | fix the arguments, e.g. pick one of the listed refs |
| `not_found` | the element or file is not there, or is disabled; wait or check the page |
| `covered` | something lies over the target; close it first |
| `logged_out` | stop; a human must log in again |
| `wedged` | the tab stopped answering and was replaced; check with `tree` or `get` whether your last action took effect before repeating it |
| `rate_limited` | wait, then repeat |
| `timeout` | the page or site was slow; repeat later |
| `human_control` | a human took over the live view; wait |
| `password` | the field takes a password; stop, a human must log in |

Never repeat an action that saves data until you have checked it did not
happen already.

## Evidence and logs

- `logs` lists console messages, page errors, failed requests, HTTP errors
  and writes (`allowed` true: sent; false: blocked). Pass the returned `next`
  as `since` to read only newer entries.
- Every call goes to a JSONL journal in the evidence folder; screenshots and
  downloads are saved with their SHA-256.

## Notes

- One tab, kept in front. Pages get `prefers-reduced-motion: reduce`, and
  headless WebGL is off unless the config sets `webgl: true`.
- Some MCP clients cut long tool output on their own. Keep results small:
  `tree` with `find`, `get` with `fields`.
