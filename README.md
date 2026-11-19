# seshka for Windows

**Kill what Claude Code left behind.** A Windows terminal port of [jigjoy-ai/seshka](https://github.com/jigjoy-ai/seshka), the macOS menu bar app.

When a Claude Code session ends, the dev servers, watchers, MCP servers and browsers it started don't. On Windows it's worse than on macOS, because ending a process never ends its children. seshka finds those processes and kills them.

## What it does

- Finds every process started by a Claude Code session that has since closed.
- Groups them by session (project and title), with their listening ports, memory and age.
- Catches browsers left open by Playwright and other automation. Each one shows as a single row, with its helper processes and `conhost.exe` folded in.
- Kills one process, one session's worth, or all of them. Whatever a killed process started goes with it.
- Tells you how much memory they were holding.
- Shows every open session too: whether it's working or idle, for how long, and what it holds.
- Ends idle sessions when you ask. If you turn the option on, it also ends them by itself once they've waited past your limit. The limit is two hours unless you change it, and the option is off by default.

## Build

You need Rust (stable, MSVC toolchain).

```powershell
cargo build --release
.\target\release\seshka.exe
```

The result is one `seshka.exe` with no dependencies. Put it anywhere on your `PATH`.

## Use

```
seshka                 Open the interactive view
seshka list [--json]   Print leftovers and open sessions, then exit
seshka kill-all [-y]   Kill every leftover process (except protected ones)
seshka kill-idle [-y]  End sessions idle longer than the limit
seshka config          Print where settings are kept, and what they are
```

Keys in the interactive view:

| Key | Action |
| --- | --- |
| `↑ ↓` / `j k` | move |
| `x` / `Del` | kill the selected process, or everything in the selected session. On an open session it ends the session (asks first) |
| `X` | kill everything left behind (asks first) |
| `i` | end sessions idle longer than the limit (asks first) |
| `p` | protect or unprotect the selected program |
| `c` | copy the command, or the session ID |
| `o` | open the process's folder in Explorer |
| `r` / `F5` | scan now |
| `s` | settings: scan interval, idle limit, auto-end, protected patterns |
| `?` | help |
| `q` / `Esc` | quit |

Settings are saved in `%APPDATA%\seshka\config.json`.

## How it works

Claude Code sets `CLAUDECODE=1` in the environment of everything it starts. It also sets the session's ID (`CLAUDE_CODE_SESSION_ID`) and its own process ID (`CLAUDE_PID`). On macOS seshka reads these with `ps`. Windows has no tool for that, so this port reads each process's environment, command line and working directory directly from its PEB, using `NtQueryInformationProcess` and `ReadProcessMemory`. It only does this for processes running under your own account. A process counts as left behind once the Claude Code process that started it is gone.

Differences from macOS, and how the port handles them:

- **Parent links go stale.** Windows doesn't reparent orphans, and it reuses PIDs quickly. A parent only counts as one if it was created before its child. Every kill also checks the process's creation time, so a reused PID is never killed.
- **Claude Desktop is also `claude.exe`.** The Claude Code CLI is told apart from the desktop app by `~/.claude/sessions/<pid>.json`, the record Claude Code keeps for each running process (its `procStart` must match the process's creation time), and by its install path.
- **There's no SIGTERM.** Apps with a window are first sent `WM_CLOSE` and given two seconds to close. Anything still running after that is ended with `TerminateProcess`.
- **Children outlive their parents.** Killing a row also kills everything it started, except programs matching a protected pattern.
- **Memory** is private bytes: what the process holds that isn't shared with other processes.

Processes running elevated (as admin) or under another user can't be inspected from a normal prompt. Run seshka elevated if you need to see those.

Nothing leaves your machine.

## License

MIT. This is a port of [seshka](https://github.com/jigjoy-ai/seshka), Copyright (c) 2026 JigJoy, MIT licensed.
