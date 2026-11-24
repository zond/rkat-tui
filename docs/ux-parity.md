# Claude Code and rkat-tui: keys and behaviour compared

This document compares rkat-tui's keys, commands and behaviour with Claude
Code's (CC), feature by feature: what is the same, what differs and what
is missing. The manual's [Keys and commands](manual.md#keys-and-commands)
is the reference for rkat-tui's side.

Claude Code facts come from the official docs at code.claude.com, fetched
2026-11-06 (the pages carry no date; the newest version they mention is
v2.1.309). The rows are ordered by popularity: first what both the official
shortcut table and the community "most used" lists name, then what only the
official tables name, then niche features.

## Parity table

| # | Feature | Claude Code | rkat-tui | Status |
|---|---|---|---|---|
| 1 | Send prompt | `Enter` | `Enter` | same |
| 2 | Interrupt / clear input / quit | `Ctrl+C`: interrupts a run; when idle the 1st press clears the input and the 2nd exits | `Ctrl+C` interrupts the turn, else clears the input, else quits on a second press within 1.5 s | same |
| 3 | Exit | `Ctrl+D`: 1st press shows a hint, 2nd within 800 ms exits; with text it deletes forward | `Ctrl+D` on an empty input quits on a second press within 3 s; with text it deletes forward | partial (3 s, not 800 ms) |
| 4 | Newline | `Shift+Enter`, `Ctrl+J`, `\`+`Enter`, `Option+Enter` | `Shift+Enter`, `Alt+Enter`, `Ctrl+J`, `\`+`Enter` | same |
| 5 | Prompt history | `Up/Down` (also `Ctrl+P/Ctrl+N`); moves by visual rows first | `Up/Down` and `Ctrl+P/N`: wrapped rows first, then the history | same |
| 6 | Interrupt the agent | `Esc` stops the turn; queued messages are sent next | `Esc` stops the agent's run; typed messages it has not read come back to the input. Helpers keep working ([Interrupting and stopping](manual.md#interrupting-and-stopping)) | differs (unread messages come back; helpers go on) |
| 7 | Rewind / checkpoints | `Esc Esc` on an empty input, or `/rewind` (`/checkpoint`, `/undo`): restore code and/or conversation, or summarize from/up to a point | none; `/new` starts over | missing (the engine cannot rewind yet) |
| 8 | Clear draft | `Esc Esc` with text: clears it and saves it to history | `Esc Esc` with text clears it into the prompt history | same |
| 9 | Permission mode cycle (manual → acceptEdits → plan → auto) | `Shift+Tab` | `Shift+Tab` cycles manual → accept edits → plan → auto; `/mode` picks one ([Permission modes](manual.md#permission-modes)) | same |
| 10 | Reverse history search | `Ctrl+R` (again: older). Classic renderer: `Enter` runs the match, `Tab`/`Esc` accept, `Ctrl+C` cancels. Fullscreen dialog: `Enter`/`Tab` place the match, `Esc` cancels | `Ctrl+R` (again: older) searches the workspace's history, then the global one; `Enter` accepts, `Esc` cancels | partial (like CC's fullscreen dialog, no `Tab`) |
| 11 | Queue messages while the agent works | `Enter` queues; messages join after the current tool calls | `Enter` queues; every message typed so far joins the agent's next model request | same |
| 12 | Send queued now | `Ctrl+X Ctrl+S` | `Ctrl+X Ctrl+S` interrupts the turn and sends the waiting messages, then the input | partial (CC backgrounds work instead of interrupting) |
| 13 | Take back queued messages | `Up` from the first line | `Up` on an empty input takes back unread messages and waiting commands | same |
| 14 | `/help` | `/help`; `?` on an empty input toggles a shortcut panel | `/help`, `/?` and `?` on an empty input open one scrolling help dialog over the transcript, on the commands or the keys (Tab switches; Esc, `q` or Ctrl-C closes) | same |
| 15 | `/clear` | `/clear [name]` | `/clear`, `/new` start a new session | partial (no name) |
| 16 | `/compact` | `/compact [instructions]`: compacts now, with optional focus | `/compact` compacts before the next model request; instructions are ignored. `/compact prepare` sets what the agent does first ([Context and compaction](manual.md#context-and-compaction)) | partial (no focus instructions) |
| 17 | `/exit`, `/quit` | `/exit` (alias `/quit`) | `/quit`, `/exit`, `/q`; `/detach` leaves the agent running | same |
| 18 | `/model` | `/model [model]`, opens a picker | `/model [[endpoint:]id \| refresh]`; alone, a picker grouped by endpoint | same |
| 19 | `/resume`, `--continue`, `--resume` | `/resume [session]`; `claude -c`; `claude --resume [id]` (opens a picker with no id) | `/resume [ref]`, `-c`/`--continue`, `--resume [ref]`; no ref opens a picker | same |
| 20 | `/cost` / `/usage` | `/usage`, with `/cost` and `/stats` as aliases | `/usage`, with `/cost` and `/stats` as aliases | same |
| 21 | Expand detail (transcript viewer) | `Ctrl+O`: tool details, thinking, collapsed lines | `Ctrl+O` shows every tool call, reasoning block and outcome in full, or folds them back; a click toggles one | partial (in place, no separate viewer) |
| 22 | Task / todo checklist | `Ctrl+T` toggles the checklist | `Ctrl+T` shows or hides the task checklist | same |
| 23 | `@` file mention with autocomplete | `@` | `@` opens a path menu; mentioned files are attached on send | same |
| 24 | Shell mode | `!` prefix runs a command; its output joins the context | `!` runs a command as you, unsandboxed; its output joins the context | same |
| 25 | `/` command menu and autocomplete | type `/`; `Tab` accepts | `/` opens a menu of matching commands; `Tab` completes | same |
| 26 | Permission prompt | Yes / Yes, don't ask again (rule) / No, and tell Claude what to do (`Esc`). Arrow keys or number keys; `Tab` adds a comment | `1`/`y` allow once, `2`/`t` allow for this session, `3`/`a` allow and add a rule, `4`/`n`/`Esc` deny with an optional instruction; `s` stops the turn ([The approval dialog](manual.md#the-approval-dialog)) | differs (an extra "this session" choice) |
| 27 | Plan mode | `Shift+Tab` to plan, `/plan [desc]`, `--permission-mode plan`; plan approval dialog | `Shift+Tab`, `/mode plan`, `--set auto.mode=plan`; a "Plan ready" dialog when a turn ends in plan mode | partial (no `/plan`) |
| 28 | Auto-accept edits | `Shift+Tab` → acceptEdits | `Shift+Tab` or `/mode accept-edits` | same |
| 29 | `/permissions` | `/permissions`: interactive allow/ask/deny rules | `/permissions`: a dialog with three sections, Blocked (recent blocked calls to approve), In force (rules, grants, statements) and Hosts ([The permissions dialog](manual.md#the-permissions-dialog)) | differs (blocked calls and trusted hosts, no ask tab) |
| 30 | `/context` | `/context [all]`: usage grid | `/context` shows the gauge, the compaction policy and what the context holds; the status bar shows `ctx 42%` | same |
| 31 | Context warnings / auto-compact | warns, then auto-compacts; `/autocompact` sets the window | warns at 75% and 85% and compacts on its own; `/set auto.compact` picks when | same (`/set` for `/autocompact`) |
| 32 | `/memory` | `/memory`: edit CLAUDE.md, toggle auto memory | `/memory` picks the workspace's or your `AGENTS.md`, or `SYSTEM.md`, to edit in your editor | partial (no auto memory) |
| 33 | `#` memory prefix | not in CC any more (the memory docs say to ask Claude or use `/memory`) | `#text` and `/remember <text>` add a line to the workspace's `AGENTS.md` | n/a (CC dropped it) |
| 34 | `/init` | `/init` writes CLAUDE.md | `/init` asks the agent to write or update `AGENTS.md` | same |
| 35 | Image paste | `Ctrl+V` (`Alt+V` on Windows/WSL) inserts an `[Image #N]` chip; a long text paste is a `[Pasted text #N +C lines]` chip | a long text paste becomes a `[Pasted text #N, C lines]` chip; no image paste | partial (no images) |
| 36 | External editor | `Ctrl+G` or `Ctrl+X Ctrl+E` | `Ctrl+G` or `Ctrl+X Ctrl+E` | same |
| 37 | Redraw | `Ctrl+L` | `Ctrl+L` | same |
| 38 | Background a task | `Ctrl+B`; `/tasks` (`/bashes`) | no key; the agent can `assign` a helper to run a command while it works; `/tasks` is `/helpers` | missing (no background shell jobs) |
| 39 | Subagents: watch and expand | collapsed subagent lines, `Ctrl+O` to expand; `/tasks` lists background subagents; `Ctrl+X Ctrl+K` stops them | a helper panel: `Ctrl+↑/↓` selects, `Enter` opens a helper's full view; `/helpers` lists them to open, fold or stop ([Watching helpers](manual.md#watching-helpers)) | differs (a full view per helper; stopped one at a time) |
| 40 | Thinking toggle | `Alt+T` (`Option+T`); `/effort` | none; set per model under `[thinking]` in `tui.toml` ([Thinking](manual.md#thinking)) | missing |
| 41 | Switch model, keeping the prompt | `Alt+P` (`Option+P`) | `Alt+P` | same |
| 42 | Undo an input edit | `Ctrl+_` | `Ctrl+_` | same |
| 43 | Readline editing: `Ctrl+A/E/U/W`, `Alt+B/F` | same keys | the same, plus `Ctrl+K`, `Alt+D`, `Ctrl/Alt+←/→`, `Ctrl/Alt+Backspace` | same |
| 44 | Kill to end of line / yank | `Ctrl+K`; `Ctrl+Y` yank; `Alt+Y` cycles | `Ctrl+K`; `Ctrl+Y` yank; `Alt+Y` cycles | same |
| 45 | Copy the last answer | `/copy [N]` | `/copy [N]`; a mouse drag copies the selection | same |
| 46 | Stash the prompt | `Ctrl+S` | `Ctrl+S`; on an empty input it restores the stash | same |
| 47 | Suspend | `Ctrl+Z` | `Ctrl+Z`; the agent keeps working | same |
| 48 | Diff display for edits | colored diffs on Edit/Write | unified diffs on the tool blocks | same |
| 49 | Markdown rendering | yes | yes; wide tables wrap their cells instead of being cut off | same |
| 50 | Spinner / elapsed / status | spinner + status | a spinner while the agent works; after 20 s of silence a yellow pulse, and the status bar says what is waited for ([Working or waiting](manual.md#working-or-waiting)) | differs (tells activity from silence) |
| 51 | Close dialogs | `Esc` | `Esc` (and `q` in lists); a list stays open after an action ([List dialogs](manual.md#list-dialogs)) | differs (lists stay open) |
| 52 | `/mcp` | `/mcp`: list, reconnect, enable, disable | none; `/trust` trusts the project's `.rkat/mcp.toml` | missing |
| 53 | `/status` | `/status`: version, model, account | `/status`: version, model, session, workspace, confinement, mode, classifier, trust | same |
| 54 | `/config` | `/config [key=value]` | `/settings` lists every setting to edit; `/set` and `/unset` change one ([Settings](manual.md#settings)) | same |
| 55 | `/export` | `/export [file]` | `/export [file]` | same |
| 56 | `/diff` | `/diff`: review working-tree changes | none | missing |
| 57 | `/review`, `/security-review` | bundled skills | none | missing |
| 58 | Custom commands / skills | `.claude/commands/*.md`, `.claude/skills/*/SKILL.md` as `/name` | skills in `~/.rkat/tui/skills/` and the project's `.rkat/skills/`, read by the agent with `skill_read` ([Skills](manual.md#skills)) | partial (no user slash commands) |
| 59 | `/add-dir`, `/cd` | `/add-dir <path>` | `--confinement-read`/`--confinement-write` or `[confinement]` in `tui.toml` ([More directories](manual.md#more-directories)) | partial (no command) |
| 60 | `/branch`, `/fork`, `/rename` | branch or fork the conversation; name the session | none for you; the agent can fork itself into a helper with `assign` | missing |
| 61 | `/btw` side question | `/btw [q]` | `/btw <question>`: a side answer the agent never sees, also while it works ([A side question](manual.md#a-side-question-btw)) | same |
| 62 | `/doctor` | setup checkup | none | missing |
| 63 | Notifications / bell | terminal bell or notification on completion or prompt | the terminal bell when a turn ends or a prompt opens while the terminal is unfocused | same |
| 64 | `/statusline` | custom status line | fixed status bar | missing |
| 65 | `/agents`, `/hooks` | subagent definitions, hook config | none (rkat-tui's `/agents` switches between the workspace's agents) | missing (low priority) |
| 66 | Vim mode | `/config` → Editor mode (`/vim` removed in v2.1.92) | none | missing |
| 67 | `/terminal-setup` | installs Shift+Enter in some terminals | not needed: `Ctrl+J` and `Alt+Enter` work everywhere | n/a |
| 68 | `/login`, `/logout`, `/bug`, `/feedback`, `/release-notes`, `/upgrade`, `/mobile`, `/desktop`, `/install-github-app`, `/theme` | account and product commands | none | n/a (CC product commands) |
| 69 | Tool call lines | `Read(src/a.rs)`, `Bash(cargo build)` with a one-line result summary | one line per call: state, verb, object, outcome, time (`✓ read src/a.rs  lines 1–40`); arguments only when expanded | same |
| 70 | Commands during a turn | slash commands typed while the agent works are queued with the messages | `/model`, `/clear`, `/resume`, `/init`, `/agents <name>` and `/rehome <ref>` wait for the turn's end, in order with the messages ([Commands during a turn](manual.md#commands-during-a-turn)) | same |
| 71 | List dialogs' confirm step | a question with yes/no choices | a question naming what happens; only `y`, `n` or `Esc` answer it | same |
| 72 | The user's message in the transcript | on a light grey background, so it stands out when scrolling | `› ` and the message on a grey band across the width | same |

rkat-tui has these with no CC counterpart:

- `Shift+↑/↓`, `PgUp/PgDn` and `Home/End` scroll the transcript;
- `/trust`, `/auto` (on, off, resume, stats, check, model);
- `/detach`, `/daemon`, `/agents`, `/rehome`, `/schedules`, `/audit`
  ([The workspace daemon](manual.md#the-workspace-daemon));
- `/wire` and `/unwire` connect agents across workspaces
  ([Wiring agents across workspaces](manual.md#wiring-agents-across-workspaces));
- `//`, `\#` and `\!` send a literal `/`, `#` or `!`.

## Sources

- https://code.claude.com/docs/en/interactive-mode — shortcuts, multiline, quick commands, history, `!` shell mode, queueing, vim
- https://code.claude.com/docs/en/commands — full command table
- https://code.claude.com/docs/en/slash-commands — custom commands and skills
- https://code.claude.com/docs/en/permission-modes — modes, `Shift+Tab` cycle, plan approval
- https://code.claude.com/docs/en/checkpointing — `/rewind`, `Esc Esc`
- https://code.claude.com/docs/en/memory — `/memory`, auto memory (no `#` prefix)
- https://www.octavehq.com/post/claude-code-cli-cheatsheet-essential-commands — "most used" list, Feb 2026
- https://claudecode-lab.pages.dev/en/blog/claude-code-keyboard-shortcuts/ — daily keys, updated Jun 2026
- https://angelo-lima.fr/en/claude-code-cheatsheet-2026-update/ — recent additions, Mar 2026

Caveats:
- The exact permission-prompt labels ("Yes / Yes, and don't ask again … / No, and tell Claude what to do differently (esc)") are not spelled out on the permission-modes page. They come from the interactive-mode `Tab` row (a comment field on Yes or No) and from general CC knowledge.
- Community cheat sheets vary in reliability; the official docs take precedence.
