# bulti

**English** · [한국어](README_KO.md)

> **bulti** (불티, "a spark") — a local-LLM coding agent CLI written in Rust that
> finishes long tasks through a **context handoff chain**.
> It drives only OpenAI-compatible inference servers running on your own machine
> (llama.cpp, vLLM, Ollama, LM Studio); remote paid APIs are out of scope.
> When the context window runs low it summarizes the work into nine sections and
> continues in a fresh segment, so a small local model can carry a large job to the end.

*A single-binary coding agent for local models. It does not fight the context limit —
it hands the task off before the limit is hit. Registration, context-length probing,
lazy skills/MCP, an automatic history database, a chat/TUI, and single-shot
orchestration are all built in. No daemon, no server, no cloud.*

![Rust](https://img.shields.io/badge/Rust-edition%202024-dea584?logo=rust&logoColor=white)
![License](https://img.shields.io/badge/license-MIT-blue)
![Backend](https://img.shields.io/badge/backend-OpenAI%20compatible-6e56cf)
![Mode](https://img.shields.io/badge/mode-local%20LLM%20only-orange)

The problem bulti targets is not raw model intelligence but **the physical limit of a
local model's context window**. The design answers it with one mechanism: the
[context handoff chain](#the-context-handoff-chain). Everything else — the probe chain,
lazy loading, the history database, the guards — exists to keep that handoff clean.
Single-shot `bulti run` is the orchestration contract (exit codes + `--json`), and
interactive `bulti chat` is the daily driver (sessions + TUI). The full design document
is [`DESIGN.md`](DESIGN.md).

## Table of contents

- [Why bulti exists](#why-bulti-exists)
- [At a glance](#at-a-glance)
- [Features](#features)
- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Interactive mode (`bulti chat`)](#interactive-mode-bulti-chat)
- [Single-shot mode and orchestration (`bulti run`)](#single-shot-mode-and-orchestration-bulti-run)
- [The context handoff chain](#the-context-handoff-chain)
- [Endpoints and context-length probing](#endpoints-and-context-length-probing)
- [Task history](#task-history)
- [Skills (lazy loading)](#skills-lazy-loading)
- [MCP (lazy loading, two-stage)](#mcp-lazy-loading-two-stage)
- [System prompt](#system-prompt)
- [Native tools](#native-tools)
- [Guards](#guards)
- [Configuration](#configuration)
- [Automatic updates](#automatic-updates)
- [Architecture](#architecture)
- [Development](#development)
- [License](#license)

## Why bulti exists

Local models are cheap, private, and always available — but their context windows are
small and their reasoning is fragile. Pointing a conventional agent at a local endpoint
produces the same failure every time: the context fills up mid-task, and the run dies
with nothing carried forward. Every previous attempt to "just keep chatting" quietly
drops the beginning of the conversation, and the model begins to hallucinate what it
cannot see.

bulti takes the opposite position. It treats context as a **scarce resource**:

1. **Inject as little as possible up front.** Skills and MCP servers put only an index
   of names and descriptions into the system prompt; their bodies and schemas are loaded
   on demand through tools.
2. **Save sessions, but link tasks.** The interactive mode persists and resumes
   sessions. Every execution is recorded as a run, and when context runs low the work is
   summarized and handed to a fresh segment automatically.

The result is a single binary that behaves like a careful colleague: it reads before it
edits, verifies before it declares completion, and when it runs out of room it writes a
meticulous handover note and picks the task up again.

## At a glance

```
        External orchestrator (shell script / CI / another agent)
                 │  bulti run "prompt" --json   (exit 0/1/2/130)
                 ▼
 ┌────────────────────────── single bulti binary ──────────────────────────┐
 │                                                                         │
 │  cli(clap) ── config(~/.bulti/config.toml) ── endpoint(probe · n_ctx)   │
 │      │                                                                  │
 │      ├── bulti run  ──▶ agent::run  ── handoff chain (segment 1→2→…→N)  │
 │      │      │           │ summary request → ===NEXT_TASK=== → new segment│
 │      │      │           ▼                                              │
 │      │      │     segment loop ── llm(SSE client) ──▶ local inference   │
 │      │      │           │ ▲                                            │
 │      │      │           ▼ │                                            │
 │      │      │     tools registry ── bash / read_file / write_file /     │
 │      │      │            edit_file / glob / grep (definition+dispatch)  │
 │      │      │            / skill_load · mcp_tools · mcp_call (lazy)     │
 │      │      │            / history_list · history_read                 │
 │      │      │                                                          │
 │      ├── bulti chat ──▶ agent::session ── interactive prompt loop       │
 │      │      │           (multi-turn + session save/resume)             │
 │      │      │           same core · same tools registry (shared)        │
 │      │      │                                                          │
 │      ├── context(token estimate · trim · truncate) · guards(regression) │
 │      ├── history(SQLite: runs · sessions · chains, automatic)           │
 │      ├── prompt(layered assembly: built-in + global + project + index)  │
 │      └── update(GitHub Releases → self-replace)                        │
 └─────────────────────────────────────────────────────────────────────────┘
```

- **Two entry points, one core.** `bulti run` for automation and `bulti chat` for people
  share the agent loop, the handoff chain, skills/MCP, and history.
- **A single binary.** No daemon, no server process, no runtime files beyond `~/.bulti`.

## Features

- **Context handoff chain** — the core. At 75% of the context window the agent writes a
  nine-section structured summary plus a `===NEXT_TASK===` marker, then starts a fresh
  segment from that summary. Chains are identified by `chain_id` and depth-capped at 12.
- **Two entry points**
  - `bulti chat` (the default when no subcommand is given) — interactive multi-turn
    chat/TUI with session save and resume.
  - `bulti run` — single-shot, complete-in-one-call automation with an exit-code contract
    and a `--json` report.
- **OpenAI-compatible endpoint management** — register llama.cpp / vLLM / Ollama /
  LM Studio servers, activate one, test connectivity, and probe the context length.
- **Context-length probe chain** — manual value → `GET /v1/models` (`max_model_len`,
  `meta.n_ctx`, `max_context_length`) → `GET /props` (llama.cpp) →
  `GET /api/show` (Ollama) → fallback 32768, with runtime correction on HTTP 400.
- **Native tools** — `bash`, `read_file`, `write_file`, `edit_file`, `glob`, `grep`,
  `history_list`, `history_read`, `skill_load`, `mcp_tools`, `mcp_call`.
- **Lazy skills and MCP** — the system prompt carries only an index. Bodies and tool
  schemas are fetched on demand, so dozens of MCP tools never consume the context budget.
- **Automatic history** — every run is recorded in SQLite (`~/.bulti/history.db`) and
  queryable from the CLI or by the model itself.
- **Layered system prompts** — built-in base + `~/.bulti/prompts/default.md` +
  `<project>/.bulti/system.md` + an always-present index section, with a full-replace
  override.
- **Reasoning support** — `reasoning_content` is streamed, displayed (💭), and recorded,
  but never re-sent in the next request.
- **i18n** — English (default), Korean, and Japanese UI, with `/language` at runtime.
- **Automatic updates** — semver comparison against GitHub Releases, sha256 verification,
  and exit-time self-replace.
- **Guards** — a table-tested defense layer against empty-response loops, stream
  repetition, stuck tool signatures, U+FFFD degeneration, false completion, and runaway
  handoffs.

## Requirements

| Item | Value |
|------|-------|
| Rust | edition 2024 (rustc 1.85+) |
| OS | Linux, macOS (any platform with a Rust toolchain) |
| LLM backend | An OpenAI-compatible local server (llama.cpp, vLLM, Ollama, LM Studio, …) |
| Network | Only outbound HTTPS to `api.github.com` for optional update checks |

bulti has **no cloud requirements**. `reqwest` uses `rustls` so there is no system
OpenSSL dependency, and `rusqlite` is bundled — no system SQLite is needed either.

## Installation

### Build from source

```bash
git clone https://github.com/agurrrrr/bulti.git
cd bulti
cargo build --release
# the binary is at target/release/bulti
install -m 0755 target/release/bulti ~/.local/bin/bulti
```

Edition 2024 requires Rust 1.85 or newer. Repository formatting is defined by
[`rustfmt.toml`](rustfmt.toml) (max width 100, 4 spaces, Unix newlines).

### Prebuilt binaries

Release artifacts are published as static musl binaries, one per target triple
(e.g. `bulti-x86_64-unknown-linux-musl.tar.gz`). `bulti update` selects the asset that
matches the running binary's target triple, verifies the sha256 checksum when a
`checksums.txt` asset is present, and replaces the running executable at exit time.

### Verify the install

```bash
bulti version
bulti --help
```

## Quick start

### 1. Start a local inference server

Any OpenAI-compatible server works. With llama.cpp:

```bash
llama-server -m /path/to/model.gguf --port 8084 --ctx-size 32768 --no-context-shift
```

`--no-context-shift` is recommended: servers that silently shift the context instead of
erroring on overflow are invisible to the client and cause the model to degenerate.

### 2. Register the endpoint

```bash
bulti endpoint add main \
  --url http://127.0.0.1:8084/v1 \
  --model qwen3.8-27b-q2

bulti endpoint use main
bulti endpoint test main     # connectivity and authentication
bulti endpoint probe main    # context length + which source resolved it
```

`context_tokens` defaults to `0`, which means "probe automatically every run". Set it
explicitly when probing is unreliable:

```bash
bulti endpoint set main context_tokens=32768
```

### 3. Talk to it

```bash
# Single-shot: complete one task and exit.
bulti run "Read README.md and summarize its three main features."

# Interactive: multi-turn chat/TUI (also the default with no subcommand).
bulti chat
bulti                          # same as `bulti chat`
```

### 4. Orchestrate it

```bash
echo "Summarize today's changes" | bulti run - --json --quiet \
  | jq -e '.status == "completed"'
```

## Interactive mode (`bulti chat`)

`bulti chat` (or running `bulti` with no subcommand) starts an interactive session. On a
TTY it uses a ratatui TUI and switches to a plain streaming text loop with `--no-tui`.

```bash
bulti chat [--endpoint NAME] [--model M] [--system-file F] [--system "TEXT"]
           [--resume <session_id>] [--no-tui] [--no-color] [--first "PROMPT"]
```

A turn (one user prompt → one completed model answer) is driven by `agent::session`,
which internally runs the same segment loop as `bulti run`. If a turn reaches the context
limit it continues through the handoff chain.

### TUI

- Streaming display of assistant output, tool calls (`🔧 name → args`), and results.
- Reasoning (💭) is shown and can be collapsed/expanded with `Ctrl+T`.
- A status line shows the endpoint, model, token usage, session id, and elapsed time.
- Multi-line input is always available (Shift+Enter), and a block cursor plus the real
  terminal cursor marks the caret even on an empty line.
- The input box wraps automatically; the cursor can be moved by character, word,
  Home/End, `Ctrl+A`/`Ctrl+E`, and `Delete`.
- Slash-command autocomplete suggests commands and arguments (endpoints, models, MCP
  servers, languages).
- `↑`/`↓` browse previous prompts.

### Slash commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `/exit` | `/quit`, `/q` | Exit the conversation |
| `/new` | | Start a new session |
| `/help` | `/?` | Command help |
| `/resume <id>` | | Resume a session |
| `/model <name> [effort]` | `/m` | Switch model (optionally set reasoning effort) |
| `/effort <low\|medium\|high>` | `/e` | Set reasoning effort |
| `/endpoint [name \| add\|set\|use\|remove …]` | `/ep` | Show or configure endpoints |
| `/mcp [name \| add\|set\|remove …]` | | Show or register MCP servers |
| `/language <en\|ko\|ja>` | `/lang`, `/l` | Change the UI language |
| `/session-info` | `/info` | Show session id / model / context usage |
| `/sessions` | `/ls` | List sessions |
| `/compact` | | Summarize and shrink the conversation history |
| `/fork` | | Fork the current session to a new id |
| `/export [path]` | | Export the conversation to a Markdown file |
| `/usage` | `/u` | Show session token and cost usage |
| `/history [query]` | `/h` | Browse prompt history |

### Sessions

Sessions are stored in `~/.bulti/sessions/<session_id>.json`. Each turn updates the
message array, and `--resume <id>` (or `/resume`) restores it. Session files are the
source of truth for resuming a conversation; the history database is the audit record for
work units, linked by `session_id`.

```bash
bulti session list
bulti session delete <id>
bulti chat --resume <id>
```

## Single-shot mode and orchestration (`bulti run`)

`bulti run` completes one task in a single command. No tool-execution approval is
requested, which makes it safe to call from scripts, CI jobs, and other agents. Progress
goes to stderr; with `--json` the final report goes to stdout exactly once.

```bash
bulti run "PROMPT" [--endpoint NAME] [--model M]
                  [--system-file F] [--system "TEXT"]
                  [--json] [--quiet] [--no-color]
                  [--max-time SECONDS] [--max-handoff-depth N]
```

- `bulti run -` reads the whole prompt from stdin (pipes and heredocs).
- When stderr is not a TTY, progress output is minimized automatically.

### Exit-code contract

| Code | Meaning | Run status |
|------|---------|------------|
| 0 | Chain completed | `completed` |
| 1 | Failure (endpoint error, fatal bug) | `failed` |
| 2 | Incomplete exit (guard, depth limit, max-time exceeded) | `incomplete` |
| 130 | SIGINT | `interrupted` |

### `--json` report

```json
{
  "version": "0.2.0",
  "status": "completed",
  "chain_id": "0f9c…",
  "segments": 3,
  "handoff_depth": 2,
  "endpoint": "main",
  "model": "qwen3.8-27b-q2",
  "input_tokens": 81234,
  "output_tokens": 9412,
  "duration_ms": 331000,
  "files_touched": ["src/main.rs", "src/agent/loop_.rs"],
  "result": "final segment's completion text",
  "runs": [1, 2, 3]
}
```

Ready-to-copy patterns live in [`examples/`](examples/): a shell pipeline, a CI job, and a
subprocess call from another agent.

## The context handoff chain

This is the heart of bulti.

```
[before every request] estimate(messages) ≥ context_tokens × handoff_threshold_pct (75%)
      │
      ▼
attempt_handoff: last request without tools — 9-section summary + ===NEXT_TASK=== directive
      │
      ├─ quality gate passes
      │     ├─ NEXT_TASK present ─▶ mark current segment complete
      │     │                       start a new segment (fresh messages) from summary + task
      │     └─ NEXT_TASK absent  ─▶ whole chain complete (exit 0)
      └─ gate failed / request failed ─▶ continue current segment with trim fallback (retry next turn)

handoff_depth ≥ warn(8)  ─▶ stderr warning
handoff_depth ≥ max(12)  ─▶ runaway guard: no more handoff → incomplete (exit 2)
```

- The new segment's prompt is the full summary plus the task under `===NEXT_TASK===`.
  Because the next segment cannot see the previous conversation, the directive requires
  file paths, decisions, and caveats to be included explicitly.
- The nine sections: original request/intent, key technical concepts, files read/changed,
  work done, failures and fixes, current progress, remaining work, things not to do, and
  the next single step.
- The quality gate requires at least 200 characters, at least five required section
  keywords, and a degenerate-content check. A failed handoff falls back to trimming the
  oldest turns instead of losing the run.
- The final run status is decided by the **whole chain**, not the last segment: if any
  segment failed or ended incomplete, the run inherits that status.

## Endpoints and context-length probing

The context length is the reference value for every handoff, so it is resolved first, in
this priority order:

1. **Manual setting** — `context_tokens > 0` wins.
2. **`GET {base}/models`** — scans `data[].max_model_len` (vLLM), `data[].meta.n_ctx`
   (llama.cpp), and `data[].max_context_length` (LM Studio).
3. **`GET {root}/props`** (llama.cpp) — reads `default_generation_settings.n_ctx`. If the
   URL ends in `/v1`, the parent path is tried.
4. **`GET {host}/api/show?model=<id>`** (Ollama) — reads `model_info`'s
   `*.context_length`.
5. **Fallback 32768** with a stderr warning.

Probe results are not cached: the value is re-confirmed at the start of every run because
a server restart can change `n_ctx`. If a request fails with a context-overflow 400, the
number is parsed out of the error, the endpoint setting is corrected automatically, and a
warning is logged.

**Secret handling.** API keys are stored in `config.toml` (recommended mode 600) and
always masked in output. `endpoint set` treats an empty or still-masked key field as
"unchanged" and never overwrites the real key.

```bash
bulti endpoint add|list|use|remove|set|test|probe
```

## Task history

Every run is recorded automatically in `~/.bulti/history.db` (SQLite, bundled) — the user
cannot turn it off. This is the single path that supplies past context when sessions are
not reused.

```bash
bulti history list [-n N] [--status S] [--chain ID]
bulti history show <id>
bulti history last [--chain]
```

The model can also query it through the `history_list` and `history_read` tools. This is
how "continue the previous task" works in single-shot mode: the model retrieves the
relevant context itself, while the system prompt only mentions that the tools exist.

## Skills (lazy loading)

- **Discovery order:** project `<root>/.bulti/skills/` → global `~/.bulti/skills/`. On a
  name collision the project wins.
- **Format:** Markdown with YAML frontmatter (`name`, `description`), either a single
  file `<name>.md` or a directory `<name>/SKILL.md` with supporting resources.
- **Only the index is injected:** skill names and descriptions (one line each). Bodies
  are never auto-injected.
- **`skill_load(name)`** returns the full body when the model decides it needs it.
- bulti ships two bundled examples (`commit-message`, `korean-report`) as usage patterns.

```bash
bulti skill list
bulti skill show <name>
```

## MCP (lazy loading, two-stage)

Unlike shepherd, bulti never injects MCP tool schemas into the prompt. Dozens of schemas
would be fatal to a local model's context budget.

- **Config:** `[mcp.<name>]` in `config.toml` (`command`, `args`, `env`, `description`).
- **Only the server index is injected:** name plus description.
- **Two-stage loading:**
  1. `mcp_tools(server)` returns the tool list (name, description, parameter summary).
     From that point the server's schemas are opt-in injected into later requests — the
     model asked for them, so the lazy principle is intact. Definition and dispatcher are
     activated together.
  2. `mcp_call(server, tool, args)` invokes a tool. Schema-mismatch errors re-state the
     parameter schema.
- **Client:** `rmcp` over stdio. The server process is spawned only on the first MCP tool
  call.
- **Result parsing:** both `content` (text) and `structuredContent` are considered; if the
  text is empty, the raw structured JSON is used.
- Timeouts (60s default) and server failures are returned as tool results and do not kill
  the run.

```bash
bulti mcp list                 # CLI: list configured servers
# In chat: /mcp [name | add|set|remove …]
```

## System prompt

The assembly is a predictable layering:

```
[built-in base]                      # identity, tool rules, completion rules, handoff cooperation
+ [~/.bulti/prompts/default.md]      # global extra instructions (optional)
+ [<project>/.bulti/system.md]       # project extra instructions (optional)
+ [index section]                    # always automatic: skills list, MCP servers, history tools
```

- **Full replacement:** `--system-file <path>` or `--system "<text>"` ignores the
  built-in, global, and project layers and substitutes the given content. The index
  section (skills/MCP/history) is retained, because otherwise the lazy-loading guidance
  disappears.
- **Template variables** (substituted in every layer): `{{cwd}}`, `{{os}}`,
  `{{endpoint}}`, `{{model}}`, `{{context_tokens}}`.
- The built-in base lives at [`src/prompt/base.md`](src/prompt/base.md) and is embedded
  with `include_str!`, so it is versioned with the code.

```bash
bulti prompt show    # print the fully assembled prompt (debug/verify)
bulti prompt edit    # open the global file in $EDITOR
```

## Native tools

Definitions and execution both live in `src/tools/`. Native tool schemas (everything
except MCP and skill tools) are included in requests by default because they are short.

| Tool | Arguments | Design notes |
|------|-----------|--------------|
| `bash` | `command`, `timeout?` | cwd pinned to the project root; no shell state is kept. Output capped at 64KB, cut on a rune boundary |
| `read_file` | `path`, `offset?`, `limit?` | 200-line window by default; a paging footer states the next offset; auto-advance |
| `write_file` | `path`, `content` | Parent directories created automatically; empty content is an explicit create |
| `edit_file` | `path`, `find`, `replace`, `replace_all?` | Exact string replacement; multiple matches are an error unless `replace_all` |
| `glob` | `pattern` | Ignores `.git`; result cap plus a hint to narrow the pattern |
| `grep` | `pattern`, `glob?`, `path?` | Self-implemented (walkdir + regex); result cap plus a hint |
| `history_list` | `query?`, `limit?` | Query the run history |
| `history_read` | `run_id` | Read one run in full |
| `skill_load` | `name` | Load a skill body |
| `mcp_tools` | `server` | List a server's tools (opt-in injection) |
| `mcp_call` | `server`, `tool`, `args(object)` | Invoke an MCP tool |

`read_file` prefixes each line with its number so the model can reference exact lines in
`edit_file`'s `find`. With a vision endpoint, image files are returned as base64
`image_url` content.

Tool results are truncated at 8,000 characters before being stored, and the truncation
message carries a **tool-specific actionable hint** (redirect to a file and page through
it, narrow the grep pattern, …) so the model changes strategy instead of repeating a
dead-end call.

## Guards

Local models have well-known failure modes. bulti ships a table-tested defense layer in
`src/agent/guards.rs` — every guard has positive (must catch) and negative (must not
catch) cases.

| Guard | Trigger | Action |
|-------|---------|--------|
| tool-call index accumulation | id/name missing in later chunks | accumulate arguments by index |
| `required:null → []` | schema serialization | normalize to an empty array |
| Korean token estimate | rune-based estimation | ASCII 4:1, non-ASCII 1:1 |
| empty-response loop | consecutive empty-content turns | incomplete at 6 turns |
| stream repetition | same line 8× or short phrase 8× in the last ~4KB | stop the stream → "repetition" incomplete |
| stuck tool signature | same (tool+args) 4 turns in a row | incomplete "no progress" |
| U+FFFD degeneration | ≥ 0.2 U+FFFD ratio (min 20 runes) | incomplete "silent context overflow" |
| future-intention nudge | no tool call + "I will …" ending | nudge instead of completion (max 2) |
| build gate | code changed and final message mentions build but `bash` not called | incomplete "build verification never run" |
| pause-summary | "stopped at / next session / to be continued" | nudge twice, then route to handoff |
| handoff quality gate | summary length/sections/degenerate | fall back to trimming |
| handoff depth | depth ≥ 12 | forbid further handoff, incomplete |

## Configuration

Settings live in `~/.bulti/config.toml` (recommended permissions 600). The full
reference is in [`DESIGN.md`](DESIGN.md) §3.1.

```toml
version = 1
active_endpoint = "main"
language = "en"                 # en | ko | ja

[endpoints.main]
url = "http://127.0.0.1:8084/v1"
api_key = "..."                 # optional (keyless local servers)
model = "qwen3.8-27b-q2"
context_tokens = 0              # 0 = automatic probe
vision = true                   # vision-capable model toggle
thinking = true                 # show/record reasoning_content
max_iterations = 200            # tool-call turn cap per segment
# reasoning_effort = "medium"   # low | medium | high
# input_price_per_mtok = 0.0    # optional cost display
# output_price_per_mtok = 0.0

[mcp.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/me"]
description = "filesystem access"

[context]
handoff_threshold_pct = 75      # trigger handoff above this % of ctx
max_handoff_depth = 12          # chain depth cap (runaway guard)
handoff_warn_depth = 8          # warn from this depth

[update]
repo = "agurrrrr/bulti"
mode = "check"                  # check | download | off
```

```bash
bulti config get <key>
bulti config set <key> <value>
bulti config list
```

The complete config directory layout:

```
~/.bulti/
├── config.toml          # all settings (recommended mode 600)
├── history.db           # run history (SQLite)
├── update.json          # release-check cache (etag, checked-at)
├── prompts/
│   └── default.md       # global system-prompt additions (optional)
├── sessions/            # interactive sessions
│   └── <session_id>.json
└── skills/
    ├── <name>.md        # single-file skill
    └── <name>/SKILL.md  # skill with a resource directory

<project root>/
└── .bulti/
    ├── system.md        # project system-prompt additions (optional)
    └── skills/          # project skills (project wins on name collision)
```

## Automatic updates

- **Endpoint:** `GET https://api.github.com/repos/{repo}/releases/latest` (the repo comes
  from `[update] repo`; baked in at build time via the `BULTI_REPO` env var, defaulting to
  `agurrrrr/bulti`).
- **Check cadence:** the result (etag and timestamp) is cached in `~/.bulti/update.json`
  and re-checked at most every 24 hours. At run start a background task prints a one-line
  notice to stderr.
- **`bulti update`:** compares the semver tag, matches the asset to the running target
  triple (e.g. `bulti-x86_64-unknown-linux-musl.tar.gz`), verifies sha256 against
  `checksums.txt` when present, unpacks to a temp directory, and replaces the executable
  at exit time (`self_replace`) so a running process is never swapped under itself.
- **Modes:** `check` (default, notice only), `download` (check and self-replace), `off`.
  `bulti update --check` only checks and changes nothing.

## Architecture

bulti is a single crate with clear module boundaries, organized so it can be split into a
workspace later if it grows.

```
bulti/
├── Cargo.toml
├── DESIGN.md                # full design document
├── README.md / README_KO.md
├── LICENSE                  # MIT
├── examples/                # orchestration patterns (shell, CI, subprocess)
├── tests/                   # integration/e2e tests (wiremock SSE)
├── .github/workflows/       # CI (fmt, clippy -D warnings, test, release build)
└── src/
    ├── main.rs              # entry point, clap parsing, exit-code mapping
    ├── lib.rs               # library crate root
    ├── cli/                 # subcommands (chat, run, endpoint, history, skill, mcp, prompt, config, update, version)
    ├── config.rs            # ~/.bulti/config.toml load/save (serde + toml)
    ├── endpoint/            # registration, probe, context-length resolution
    ├── llm/                 # OpenAI-compatible client (SSE streaming, tool-call accumulation)
    ├── agent/
    │   ├── mod.rs           # run/session orchestration, chain and segment management
    │   ├── loop_.rs         # segment loop, completion decision
    │   ├── context.rs       # token estimate, trimming, tool-result truncation
    │   ├── handoff.rs       # handoff directive, parser, quality gate
    │   └── guards.rs        # regression / false-completion / stuck guards
    ├── tools/               # native tools + ToolRegistry (definition and dispatch together)
    ├── session/             # interactive session save/load/delete
    ├── history/             # rusqlite storage and queries
    ├── skills/              # lazy skill discovery and loading (bundled examples)
    ├── mcp/                 # lazy MCP client (rmcp)
    ├── prompt/              # layered system-prompt assembly (base.md)
    ├── slash/               # slash-command registry and autocomplete
    ├── completion/          # prompt-history completion sources
    ├── render/              # Markdown → ANSI/TUI rendering
    ├── i18n/                # en/ko/ja catalogs and language state
    ├── tui/                 # interactive TUI rendering (ratatui)
    └── update/              # GitHub release check and self-replace
```

Technology stack: `tokio` (rt-multi-thread, macros, process, fs, io-util), `reqwest`
0.12 with rustls, `eventsource-stream`, `serde`/`serde_json`/`toml`, `clap` v4 derive,
`rusqlite` (bundled), `dirs`, `walkdir`/`glob`/`regex`, `rmcp`, `ratatui`/`crossterm`,
`semver`/`sha2`/`tar`/`flate2`/`tempfile`, and `thiserror`/`anyhow`/`tracing`.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

The CI workflow ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs exactly
these checks on push to `main` and on every pull request. Integration tests use
`wiremock` to emulate the SSE endpoint, so no live model server is required.

**Build gate:** code changes must not be declared complete until `cargo build` and
`cargo test` pass. The agent's own build gate enforces the same rule at runtime.

## Contributing

Issues and pull requests are welcome. Please run the checks above before opening a PR, and
open an issue to discuss direction before large features — keeping the surface small is an
explicit goal of this project. Design decisions are recorded in [`DESIGN.md`](DESIGN.md).

## License

MIT License — see [LICENSE](LICENSE).

---

> ***"The fire on the altar must be kept burning; it must not go out."***
> — Leviticus 6:13
