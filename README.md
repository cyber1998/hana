<p align="center">
  <img src="docs/banner.svg" alt="hana: cleans up after Claude Code on Windows" width="100%">
</p>

<p align="center">
  <img alt="Windows 10 and 11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-F4A7C0?style=flat-square&logo=windows&logoColor=white&labelColor=1F1520">
  <img alt="Written in Rust" src="https://img.shields.io/badge/Rust-stable-F74C00?style=flat-square&logo=rust&logoColor=white&labelColor=1F1520">
  <img alt="MIT license" src="https://img.shields.io/badge/license-MIT-B5E8C9?style=flat-square&labelColor=1F1520">
  <img alt="Based on seshka" src="https://img.shields.io/badge/based%20on-seshka-E8B4D8?style=flat-square&labelColor=1F1520">
</p>

<p align="center">
  <b>Claude Code sessions end. The dev servers, watchers, browsers and worktrees they started don't.</b><br>
  hana finds what they left behind, shows what it costs you, and cleans it up.
</p>

<p align="center"><sub><i>hana</i> (花) means <i>flower</i>.</sub></p>

---

## ✿ Credits

hana is a Windows port of **[seshka](https://github.com/jigjoy-ai/seshka)** by the **[JigJoy](https://jigjoy.ai)** team, a macOS menu bar app that kills what Claude Code leaves behind.

seshka came up with the idea and the method this whole tool rests on: read the markers Claude Code leaves in every process's environment, and treat a process as left behind once its Claude Code is gone. Its session grouping, helper folding, protected programs and idle-session handling are all carried over here. hana rewrites it in Rust for Windows and adds a storage tab and themes.

seshka is MIT-licensed, Copyright (c) 2026 JigJoy, and that notice is kept in [LICENSE](LICENSE). If you're on a Mac, use the original.

---

## ✿ Install

**1. Install Rust** (skip this step if `cargo --version` already works)

```powershell
winget install Rustlang.Rustup
```

If rustup asks to install the Visual Studio C++ build tools, say yes. Then open a new terminal.

**2. Install hana**

```powershell
cargo install --git https://github.com/cyber1998/vac
```

**3. Run it**

```powershell
hana
```

That's all. `hana.exe` is a single file in `%USERPROFILE%\.cargo\bin`, which is already on your `PATH`.

<details>
<summary>Building from a clone instead</summary>

```powershell
git clone https://github.com/cyber1998/vac
cd vac
cargo install --path .
```

</details>

---

## ✿ What it does

|  | Processes tab | Storage tab |
| --- | --- | --- |
| **Finds** | Every process started by a Claude Code session that has since closed, grouped by session, with ports, memory and age | Every session's data on disk, and every git worktree in the repositories your sessions worked in |
| **Shows** | Open sessions too: working or idle, for how long, and what they hold | How much each one weighs, and for worktrees whether they hold uncommitted work |
| **Cleans** | One process, one session's worth, or all of them, along with whatever they started | Session data goes to the Recycle Bin; worktrees are removed through git |

On top of that:

- **Browsers count as one row.** A browser opened by Playwright appears once, with its helper processes and console host folded in.
- **Idle sessions can be ended**, either when you ask or automatically after a limit you set. Auto-ending is off by default.
- **Protected programs** are never touched by bulk kills.
- **Eleven themes**, shown below.

---

## ✿ Themes

<p align="center">
  <img src="docs/themes.svg" alt="The eleven themes: original, kawaii, cyberpunk, neon, tokyo-night, github, matrix, deep-ocean, autumn-forest, cherry-blossom, white-orchard" width="100%">
</p>

Press `t` / `T` in the app to cycle through them, or pick one directly:

```powershell
hana --theme cherry-blossom
```

`hana themes` prints all of them with their colors. Your choice is remembered.

---

## ✿ Use

```text
hana [--storage]                       open the interactive view (on the storage tab)
hana list [--json]                     print leftovers and open sessions
hana kill-all [-y]                     kill every leftover process (except protected ones)
hana kill-idle [-y]                    end sessions idle longer than the limit

hana disk [--json]                     how much each worktree and session's data weighs
hana delete-session <id>... [-y]       move sessions' data to the Recycle Bin (ID or prefix)
hana remove-worktree <name> [--force]  git worktree remove; --force also drops uncommitted work

hana themes · hana --theme <name> · hana config · hana --help
```

<details>
<summary><b>Keys</b></summary>

| Everywhere | |
| --- | --- |
| `↑ ↓` `j k` | move |
| `Tab` `1` `2` | switch between processes and storage |
| `t` / `T` | next / previous theme |
| `c` / `o` | copy the command, ID or path / open the folder in Explorer |
| `s` / `?` / `q` | settings / help / quit |

| Processes tab | |
| --- | --- |
| `x` `Del` | kill the selected process, or the whole session. On an open session, end it (asks first) |
| `X` | kill everything left behind (asks first) |
| `i` | end sessions idle past the limit (asks first) |
| `p` | protect / unprotect the selected program |
| `r` `F5` | scan now |

| Storage tab | |
| --- | --- |
| `space` | mark a worktree or session |
| `M` / `u` | mark sessions untouched for 30+ days / unmark all |
| `x` `Del` | delete what's marked, or the selected row (asks first) |
| `r` | scan and measure again |

</details>

Settings (scan interval, idle limit, auto-end, protected programs, theme) are in `%APPDATA%\hana\config.json`, and you can change them in the app with `s`.

---

## ✿ Safe by design

- **Ending a live session, bulk kills, and every delete ask first.** Killing a leftover process, or one closed session's leftovers, takes one keypress, as in seshka. Auto-ending idle sessions only happens if you turn it on.
- **A PID is never trusted on its own.** Windows reuses process IDs quickly, so every kill first checks the process's creation time.
- **Session data goes to the Recycle Bin**, never deleted for good. Open sessions are skipped, and so is each project's `memory/` folder.
- **Worktrees are removed through `git worktree remove`**, so their branches stay. A worktree with uncommitted changes or commits that no branch points to is only removed if you press `F`. One that a running process is working in is never removed.
- **Nothing leaves your machine.**

<details>
<summary><b>How it works</b></summary>

Claude Code sets `CLAUDECODE=1`, the session's ID and its own PID in the environment of everything it starts. seshka reads those with `ps` on macOS. Windows has no tool for that, so hana reads each process's environment, command line and working directory directly from its PEB, for processes under your own account. A process counts as left behind once the Claude Code that started it is gone.

On Windows a few things need extra care:

- **Parent IDs go stale.** Orphaned processes aren't re-parented, so a "parent" only counts if it was created before its child.
- **Claude Desktop is also `claude.exe`.** The Claude Code CLI is told apart by its session record (`~/.claude/sessions/<pid>.json`, matched by creation time) and its install path.
- **There's no SIGTERM.** Apps with a window are sent `WM_CLOSE` and given two seconds to close. Anything still running is ended.
- **Children outlive their parents.** Killing a row also ends what it started, except protected programs.

Each session's data lives in `~/.claude`:

- the transcript: `projects/<project>/<id>.jsonl`
- subagent and tool output: `projects/<project>/<id>/`
- `file-history/<id>/` and `session-env/<id>/`
- older `todos/` and `debug/` files

Worktrees are found with `git worktree list` in every repository a session worked in, plus any stray folders under `.claude\worktrees`.

Processes running elevated or as another user can't be inspected from a normal terminal. Run hana as administrator to see those too.

</details>

---

<p align="center">
  <sub>MIT · built on <a href="https://github.com/jigjoy-ai/seshka">seshka</a> by <a href="https://jigjoy.ai">JigJoy</a> · ❀</sub>
</p>
