# rustglance

A browser tool for AI agents, written in Rust. It drives Chromium over the
DevTools protocol (CDP) and serves its tools as MCP over stdio.

- **Ten tools**: `open`, `tree`, `tap`, `type`, `upload`, `download`,
  `screenshot`, `logs`, `get`, `check`. No tool runs agent-written JavaScript.
- **Settles by itself**: every action waits until the network and the DOM go
  quiet, so the agent does not add sleeps.
- **Refusals that say what to do**: a refused call returns `error` (a class to
  branch on), `reason` and `next`.
- **Write check on every call**: requests other than GET, HEAD and OPTIONS
  from the driven tab are blocked unless the operator allowed them; each
  result lists the writes it blocked. See
  [Security model and limits](#security-model-and-limits) for what it does
  not cover.
- **Evidence**: every call goes to a JSONL journal with time and duration;
  screenshots and downloads are saved with their SHA-256.
- **Replay without a model**: a journal is also a script. Replay reads only by
  default and stops with a report at the first failed step.
- **Live view with takeover**: an MJPEG stream on 127.0.0.1; a human can take
  over the browser, and agent calls wait until it is released.
- **No daemon**: Chromium starts with `--remote-debugging-port=0`; a browser
  already running with the same profile is reused.

## Build

```bash
cargo build --release
```

Needs Rust 1.85 or newer and a Chromium-based browser (`chromium`,
`google-chrome`, or the path in the config). Tested on Linux only; on other
systems set `chromium` to the browser's path.

## Use

```bash
rustglance login --config agent.json     # a human logs in once, in a window
rustglance mcp --config agent.json       # MCP server for the agent
```

`login` is needed only for sites behind a login. For public sites skip it and
run `mcp` directly; the agent browses as a visitor without an account.

An MCP client entry:

```json
{ "mcpServers": { "browser": { "command": "rustglance", "args": ["mcp", "--config", "/path/agent.json"] } } }
```

Agents never log in. When a site answers 401 or redirects to its login page,
calls are refused with `logged_out` and a human runs `rustglance login` again.

`skills/rustglance/SKILL.md` is an agent skill with the working loop, data
reading, refusal handling and evidence; copy it to your agent's skills folder.

## Configuration

JSON, every field optional. Keep it, the profile and the evidence outside any
repository: the profile holds the logged-in session.

```json
{
  "profile": "/home/me/.local/share/rustglance/profile",
  "start_url": "https://app.example.com",
  "evidence": "/home/me/rustglance-evidence",
  "upload_dir": "/home/me/rustglance-uploads",
  "headless": true,
  "write_block": ["*"],
  "write_allow": ["https://app.example.com/api/search*"],
  "writes": false,
  "logged_out_url": "/login",
  "view_port": 9333
}
```

| field | default | meaning |
|---|---|---|
| `profile` | `~/.local/share/rustglance/profile` | Chromium user data dir |
| `chromium` | searched in PATH | browser binary |
| `headless` | `true` | no window (login always opens one) |
| `webgl` | `false` | WebGL without a window; headless Chromium draws it on the CPU |
| `start_url` | none | opened at start; base for relative paths |
| `evidence` | `~/.local/share/rustglance/evidence` | journals, `shots/`, `downloads/` |
| `upload_dir` | none (uploads off) | the only folder `upload` reads |
| `write_block` | `["*"]` | URL globs whose writes are checked |
| `write_allow` | `[]` | URL globs where writes pass |
| `writes` | `false` | allow every write |
| `logged_out_url` | none | URL part that means "logged out" |
| `cdp_timeout_ms` | `15000` | a page that does not answer in time is replaced |
| `settle_ms` | `8000` | longest wait for the page to go quiet |
| `download_ms` | `120000` | longest wait for a download |
| `retries` | `4` | retries of `get` on 429 and 503 |
| `get_max_bytes` | `16384` | largest `get` body handed to the agent; some MCP clients cut longer output; `0` hands every body over |
| `view_port` | none (off) | live view port on 127.0.0.1; if the view cannot start, tools still work |
| `window` | `[1280, 900]` | window size |
| `chromium_args` | `[]` | extra browser flags |

Every tab rustglance attaches to is brought to front and sees `prefers-reduced-motion: reduce`, so pages that honour it stop looping animations instead of drawing them on the CPU.

## Tools

| tool | what it does |
|---|---|
| `open` | go to a URL (or a path resolved against the page) and settle |
| `tree` | page outline: refs like `e12` for interactive elements, headings, text, groups; `find` filters lines |
| `tap` | click by `ref` or visible `text`; refuses when something covers the target |
| `type` | type into a field (replaces by default), pick a select option, or press a `key` |
| `upload` | put a file from `upload_dir` into a file input |
| `download` | click or fetch a download; saved with size and SHA-256 |
| `screenshot` | PNG to the evidence folder; `look: true` returns the image |
| `logs` | console, page errors, failed requests, HTTP errors, writes (`allowed` true or false) |
| `get` | HTTP GET with the browser's login; JSON body, `fields` to keep only some keys of list items; a body over `get_max_bytes` keeps the first list items (`truncated`) and is saved whole (`file`); retries 429 and 503 |
| `check` | wait until text shows, text goes, the URL matches, or a ref exists; `timeout_ms` up to 120000 |

Refusal classes: `bad_args`, `not_found`, `stale`, `covered`, `logged_out`,
`rate_limited`, `outside_folder`, `timeout`, `wedged`, `page_error`, `site`,
`browser`, `human_control`, `io`.

## Journal, scripts, replay

Each `mcp` run writes `evidence/journal-<time>.jsonl`, one line per call:
time, tool, arguments, duration, result or refusal, blocked writes.

```bash
rustglance report journal.jsonl                  # markdown summary
rustglance script run1.jsonl run2.jsonl > s.jsonl
rustglance replay s.jsonl --param p1=value       # reads only
rustglance replay s.jsonl --writes               # also writes
```

`script` with two runs of the same task turns string arguments that differ
into `{{p1}}`, `{{p2}}`... and prints the values seen. Replay skips tools that
write and blocks every network write unless `--writes`. It prints nothing per
step; at the first failed step (refusal, `check` false, or a tap that hit a
different element than in the recording) it stops and writes
`replay-<time>-failure.md` with a screenshot and the browser log.

## Live view

With `view_port` set, open `http://127.0.0.1:<port>/`. "Take over" refuses
agent calls with `human_control` and sends your clicks, wheel and keys to the
page; "Release" gives control back. It listens on 127.0.0.1 only and checks
the Host and Origin headers; any local process can still reach it. Write
blocking stays on during a takeover, so a save you click is blocked too unless
the config allows it.

## Security model and limits

rustglance keeps an agent from saving data by mistake. It is not a sandbox
against a hostile page or a hostile local user.

- Write blocking covers the one tab rustglance drives. Tabs and popups the
  page opens, cross-origin iframes that run in their own process, and workers
  (dedicated, shared and service workers) are not attached, so their requests
  are not checked. WebSocket messages are not checked either.
- GET, HEAD and OPTIONS always pass, so a site that changes data on GET is not
  protected. Nothing limits which hosts the agent opens: a page with injected
  instructions can make the agent open a URL that carries data out. Blocking
  writes does not stop data from leaving.
- Chromium's DevTools port listens on 127.0.0.1 without a password while the
  browser runs. Any local process, of any user, can drive the logged-in
  browser. Run it on a machine you trust.
- The journal, screenshots and downloads are written with your default file
  permissions (umask), and the journal holds every argument, including text
  the agent typed. `type` refuses password fields and keeps that attempt's
  text out of the journal. Keep the evidence folder private.

## Extending

Add tools in your own binary and keep the core as a dependency:

```rust
fn main() -> std::process::ExitCode {
    let mut ops = rustglance::ops::core();
    ops.extend(my_tools());
    rustglance::cli::main_with(ops)
}
```

An `OpDef` with `writes: true` is skipped by replay unless `--writes`.
`Ctx::permit(pattern)` lets one call write to matching URLs.

## License

MIT or Apache-2.0, at your option.
