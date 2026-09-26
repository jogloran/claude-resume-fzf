# claude-resume-fzf

> Jump back into any Claude Code session, from anywhere, in two keystrokes.

[![Rust](https://img.shields.io/badge/built%20with-Rust-000000?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

This is [jogloran](https://github.com/jogloran)'s fork of
[antlis/claude-resume-fzf](https://github.com/antlis/claude-resume-fzf), with a few
changes on top — see [What's different in this fork](#whats-different-in-this-fork).

`claude-resume-fzf` is a tiny, fast Rust CLI that finds **every** [Claude Code](https://claude.ai/code)
session on your machine and lets you fuzzy-search and resume any of them with
[`fzf`](https://github.com/junegunn/fzf) — no matter which directory you're currently in.

```
session> auth
  2h   ~/work/api        ▸ add JWT refresh + rotate the signing keys
  1d   ~/dotfiles        ▸ why does my zsh prompt lag on git repos
  3d   ~/work/api        ▸ the login handler returns 500 on empty body
┌──────────────────────────────────────────────────────────────────┐
│ dir:    ~/work/api                                                 │
│ branch: feat/jwt                                                   │
│ claude: 2.1.197                                                    │
│ ─────────────────────────────────────                             │
│ you:    add JWT refresh + rotate the signing keys                  │
│ claude: I'll add a /auth/refresh endpoint and a rotation job...    │
└──────────────────────────────────────────────────────────────────┘
```

## Why

Claude Code stores each session under `~/.claude/projects/<encoded-cwd>/<id>.jsonl`,
and `claude --resume` only lists sessions for the folder you're standing in. If you
remember *what* you were working on but not *where*, or you want to hop between
projects, you're stuck `cd`-ing around and squinting at UUIDs.

This tool flips it around: **search by what you said, not where you were.** Pick a
session and it drops you into the right directory and resumes it for you.

## Features

- 🔎 **Global search** — every session across every project, newest first.
- ⌨️ **Fuzzy find** by title and directory — or the *entire transcript* with `-a`.
- 👁️ **Live preview** — read the actual conversation, rendered as Markdown
  (headers, bold/italic, code blocks, lists), scrolled to the newest message.
- 🚀 **Zero-config** — reads Claude Code's own session files; nothing to set up.
- 🦀 **Fast & tiny** — single static-ish Rust binary.

## Install this fork

This fork isn't published to crates.io — install it straight from git (requires a
Rust toolchain):

```sh
cargo install --git https://github.com/jogloran/claude-resume-fzf
```

Already have a build installed and want to pick up a newer commit? Add `--force`:

```sh
cargo install --git https://github.com/jogloran/claude-resume-fzf --force
```

Or clone and build locally:

```sh
git clone https://github.com/jogloran/claude-resume-fzf
cd claude-resume-fzf
cargo install --path .
```

Either way installs two binaries: `claude-resume-fzf` and the short alias **`ccresume`**.

To install on a remote machine you're already authenticated to GitHub on, just run
the `cargo install --git ...` command there directly — no need to copy anything over.

## Usage

```sh
ccresume          # search by title + directory (default)
ccresume -a       # also fuzzy-search the full conversation transcript
```

- **Type** to fuzzy-search. By default this matches each session's title (the one
  Claude generates) and its directory.
- Pass **`-a`** / **`--all`** to also search everything ever said in the session —
  your prompts and Claude's replies. Handy when you remember *what* you discussed
  but not the title (e.g. "that time I was messing with `yazi`"). To keep results
  meaningful over long transcripts, this mode matches the query as an **exact
  substring** (fuzzy matching would match almost every session).
- The **preview pane** shows the conversation for the highlighted session,
  rendered as Markdown, scrolled to the most recent message.
- **`Enter`** — `cd` into the session's directory and run `claude --resume <id>`.
- **`ctrl-x`**, pressed twice — delete the highlighted session. The first press
  arms it (the header prompts for confirmation); the second press within a few
  seconds deletes it and refreshes the list. Selecting a different session, or
  waiting too long, re-arms instead of deleting.
- **`Esc`** — quit, do nothing.

## Requirements

- [`fzf`](https://github.com/junegunn/fzf) on your `PATH`
- [`claude`](https://claude.ai/code) (Claude Code CLI) on your `PATH`
- Linux/macOS (uses `exec` to hand off to `claude`)

## How it works

1. Scans `~/.claude/projects/*/*.jsonl`.
2. For each session file, extracts the working directory, Claude's generated
   title (falling back to the first user prompt), and the last-modified time.
   Sessions with no recorded `cwd` fall back to decoding the folder name, but only
   if that path still exists.
3. Feeds a formatted, tab-delimited list into `fzf`; hidden columns carry the raw
   `cwd`, session id, and file path. With `-a`, a flattened transcript blob is
   appended off-screen so `fzf` searches the whole conversation.
4. The preview pane is rendered by re-invoking the binary (`--preview <file>`),
   which renders the conversation turns as Markdown via
   [`termimad`](https://github.com/Canop/termimad).
5. On selection it `chdir`s to the session's directory and `exec`s
   `claude --resume <id>`, replacing itself with Claude Code.
6. `ctrl-x` re-invokes the binary (`--arm-or-delete <file>`), which records an
   "armed" session to a small per-user state file; a second `ctrl-x` on that
   same session within the confirm window deletes the file (and its now-empty
   parent project folder) and reloads the list.

## What's different in this fork

Compared to [antlis/claude-resume-fzf](https://github.com/antlis/claude-resume-fzf):

- **Markdown-rendered preview** — message bodies render as real Markdown
  (headers, bold/italic, code blocks, lists) via `termimad`, instead of plain
  wrapped text.
- **Word-wrapped, scroll-to-bottom preview** — text wraps at word boundaries to
  the actual preview pane width, and the pane opens scrolled to the newest
  message instead of the start of the transcript.
- **Git branch shown in the list** — in its own color, next to the directory.
- **Correct relative timestamps** — sorts/displays by the last real
  conversation activity, not the file's OS mtime (which trailing bookkeeping
  writes, like cost tracking, can bump long after a session actually ended).
- **`ctrl-x` to delete a session** — press twice to confirm; see
  [Usage](#usage).

## Contributing

Issues and PRs welcome, on [this fork](https://github.com/jogloran/claude-resume-fzf)
or [upstream](https://github.com/antlis/claude-resume-fzf). It's a small,
single-file codebase (`src/lib.rs`) — easy to read and hack on.

## License

[MIT](LICENSE)
