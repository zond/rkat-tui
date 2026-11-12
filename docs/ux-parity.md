# Claude Code and rkat-tui: keys and behaviour compared

This document compares rkat-tui's keys, commands and behaviour with Claude
Code's (CC), feature by feature: what rkat-tui has the same, what differs
and what is missing. [`manual.md`](manual.md), "Keys and commands", is the
reference for rkat-tui's side.

Claude Code facts come from the official docs at code.claude.com, fetched
2026-11-06 (the pages carry no date; the newest version they mention is
v2.1.309). The rows are ordered by popularity: first what both the official
shortcut table and the community "most used" lists name, then what only the
official tables name, then niche features.

## Parity table

| # | Feature | Claude Code | rkat-tui | Status |
|---|---|---|---|---|
| 1 | Send prompt | `Enter` | `Enter` | same |
| 2 | Interrupt / clear input / quit | `Ctrl+C`: interrupts a run; when idle the 1st press clears the input and the 2nd exits | `Ctrl+C`: interrupts the turn, else clears the input, else a 2nd press within 1.5 s quits | same |
| 3 | Exit | `Ctrl+D`: 1st press shows a hint, 2nd within 800 ms exits; with text it deletes forward | `Ctrl+D` on an empty input quits on a second press within 3 s (the editor title asks for it); with text it deletes forward | partial (the second press may come within 3 s, not 800 ms) |
| 4 | Newline | `Shift+Enter`, `Ctrl+J`, `\`+`Enter`, `Option+Enter` | `Shift+Enter`, `Alt+Enter`, `Ctrl+J`, `\`+`Enter`; `Ctrl+Enter` sends now, as in CC | same |
| 5 | Prompt history | `Up/Down` (also `Ctrl+P/Ctrl+N`); moves by visual rows first | `Up/Down` and `Ctrl+P/N`, by wrapped rows first, then history | same |
| 6 | Interrupt the agent | `Esc` stops the turn; queued messages are sent next | `Esc` interrupts the turn; queued messages are sent next | same |
| 7 | Rewind / checkpoints | `Esc Esc` on an empty input, or `/rewind` (`/checkpoint`, `/undo`): restore code and/or conversation, or summarize from/up to a point | none: no `/rewind`, and `Esc Esc` on an empty input does nothing; `/help` says rewind is not available (the engine cannot rewind a mob member's session; lukacf/meerkat#2126 is a merged design, not implemented) | missing |
| 8 | Clear draft | `Esc Esc` with text: clears it and saves it to history | `Esc Esc` with text clears it and saves it to the prompt history | same |
| 9 | Permission mode cycle (manual → acceptEdits → plan → auto) | `Shift+Tab` | `Shift+Tab` cycles manual → accept edits → plan → auto; `/mode` opens a list of the modes, `/mode <name>` sets one | same |
| 10 | Reverse history search | `Ctrl+R` (again: older). Classic renderer: `Enter` runs the match, `Tab`/`Esc` accept, `Ctrl+C` cancels. Fullscreen dialog: `Enter`/`Tab` place the match, `Esc` cancels | `Ctrl+R` (again: older), the workspace's history then the global one; `Enter` accepts the match into the editor; `Esc` cancels | same key; behaves as CC's fullscreen dialog (no `Tab` accept) |
| 11 | Queue messages while the agent works | `Enter` queues; messages join after the current tool calls | `Enter` steers: the message joins the running turn at its next step (after the running tool's result) and shows as on its way until the agent reads it | same |
| 12 | Send queued now | `Ctrl+Enter` or `Ctrl+X Ctrl+S` | `Ctrl+Enter` and `Ctrl+X Ctrl+S` interrupt the turn and send the input | same keys (CC moves backgroundable work to the background instead of interrupting; rkat-tui has no background tasks) |
| 13 | Take back queued messages | `Up` from the first line | `Up` on an empty input takes back the queued messages the agent has not read yet | same |
| 14 | `/help` | `/help`; `?` on an empty input toggles a shortcut panel | `/help`, `/?`; `?` on an empty input shows the keys | same |
| 15 | `/clear` | `/clear [name]` | `/clear`, `/new` | same |
| 16 | `/compact` | `/compact [instructions]`: compacts now, with optional focus | `/compact` asks the engine to compact at the next model call; focus instructions are ignored with a warning; `/compact status` shows the policy | partial |
| 17 | `/exit`, `/quit` | `/exit` (alias `/quit`) | `/quit`, `/exit`, `/q` | same |
| 18 | `/model` | `/model [model]`, opens a picker | `/model [[endpoint:]id\|refresh]`, opens a filtered picker grouped by endpoint | same |
| 19 | `/resume`, `--continue`, `--resume` | `/resume [session]`; `claude -c`; `claude --resume [id]` (opens a picker with no id) | `/resume [ref]` and a picker; `-c`/`--continue`; `--resume [ref]` (opens the picker with no ref) | same |
| 20 | `/cost` / `/usage` | `/usage`, with `/cost` and `/stats` as aliases | `/usage`, with `/cost` and `/stats` as aliases | same |
| 21 | Expand detail (transcript viewer) | `Ctrl+O`: tool details, thinking, collapsed lines | `Ctrl+O` shows or hides the last reasoning block and the last tool call's output; a click toggles one tool | partial (not a whole-transcript view) |
| 22 | Task / todo checklist | `Ctrl+T` toggles the checklist | `Ctrl+T` shows or hides the task checklist | same |
| 23 | `@` file mention with autocomplete | `@` | `@` opens a path menu; mentioned files are attached when the prompt is sent | same |
| 24 | Shell mode | `!` prefix runs a command; its output joins the context | `!` prefix runs a command as you, with no sandbox; its output joins the context | same |
| 25 | `/` command menu and autocomplete | type `/`; `Tab` accepts | typing `/` opens a menu of matching commands; `Tab` completes | same |
| 26 | Permission prompt | Yes / Yes, don't ask again (rule) / No, and tell Claude what to do (`Esc`). Arrow keys or number keys; `Tab` adds a comment | `1`/`2`/`3`, arrows and `Enter`, a comment on No; `y`/`a`/`n`/`s` as accelerators; `Ctrl+C` stops every open prompt | same |
| 27 | Plan mode | `Shift+Tab` to plan, `/plan [desc]`, `--permission-mode plan`; plan approval dialog | `Shift+Tab`, `/mode plan`, `--set auto.mode=plan`; a "Plan ready" dialog when a turn ends in plan mode | partial (no `/plan` command) |
| 28 | Auto-accept edits | `Shift+Tab` → acceptEdits | accept edits mode | same |
| 29 | `/permissions` | `/permissions`: interactive allow/ask/deny rules | `/permissions` opens a list of the rules (`d` removes one); `/permissions [list\|add allow\|deny <tool> [pattern]\|rm <n>]` | same |
| 30 | `/context` | `/context [all]`: usage grid | `/context` prints the gauge, the compaction policy and what the context holds; the status bar shows `ctx 42% (84k/200k)` | same |
| 31 | Context warnings / auto-compact | warns, then auto-compacts; `/autocompact` sets the window | warnings at 75% and 85%; `auto.compact` and `--auto-compact` | same (no `/autocompact`) |
| 32 | `/memory` | `/memory`: edit CLAUDE.md, toggle auto memory | `/memory` opens a list of the workspace and user `AGENTS.md` and `SYSTEM.md` (path, size); Enter, or `/memory workspace\|user\|system`, edits one in the editor | same |
| 33 | `#` memory prefix | not in CC any more (the memory docs say to ask Claude or use `/memory`) | `#text` and `/remember <text>` append to the workspace `AGENTS.md` | n/a (rkat-tui keeps a control CC dropped) |
| 34 | `/init` | `/init` writes CLAUDE.md | `/init` asks the agent to write or update the workspace `AGENTS.md` | same |
| 35 | Image paste | `Ctrl+V` (`Alt+V` on Windows/WSL) inserts an `[Image #N]` chip | text paste only | missing |
| 36 | External editor | `Ctrl+G` or `Ctrl+X Ctrl+E` | `Ctrl+G` or `Ctrl+X Ctrl+E` | same |
| 37 | Redraw | `Ctrl+L` | `Ctrl+L` | same |
| 38 | Background a task | `Ctrl+B`; `/tasks` (`/bashes`) | none: the shell runs commands in the foreground (background jobs are not available); `/tasks` is an alias of `/helpers` | missing |
| 39 | Subagents: watch and expand | collapsed subagent lines, `Ctrl+O` to expand; `/tasks` lists background subagents; `Ctrl+X Ctrl+K` stops them | helper panel with `Ctrl+↑/↓` to select and `Enter` for a helper's full view; a click on a header unfolds it; `/helpers` opens a list of them (Enter: full view, `f`: fold), `/helpers [list\|expand\|fold <label>\|all]` (alias `/tasks`); `Esc` stops them with the turn | differs (more than CC: a panel and a full view per helper) |
| 40 | Thinking toggle | `Alt+T` (`Option+T`); `/effort` | none (`[thinking]` in `tui.toml`, per model) | missing |
| 41 | Switch model, keeping the prompt | `Alt+P` (`Option+P`) | `Alt+P` | same |
| 42 | Undo an input edit | `Ctrl+_` | `Ctrl+_` | same |
| 43 | Readline editing: `Ctrl+A/E/U/W`, `Alt+B/F` | same keys | `Ctrl+A/E/K/U/W`, `Alt+B/F/D`, `Ctrl/Alt+←/→`, `Ctrl/Alt+Backspace` | same (with a kill ring) |
| 44 | Kill to end of line / yank | `Ctrl+K`; `Ctrl+Y` yank; `Alt+Y` cycles | `Ctrl+K`; `Ctrl+Y` yank; `Alt+Y` cycles | same |
| 45 | Copy the last answer | `/copy [N]` | `/copy [N]`; a mouse drag copies the selection | same |
| 46 | Stash the prompt | `Ctrl+S` | `Ctrl+S` | same |
| 47 | Suspend | `Ctrl+Z` | `Ctrl+Z` | same |
| 48 | Diff display for edits | colored diffs on Edit/Write | unified diffs on the tool blocks | same |
| 49 | Markdown rendering | yes | yes | same |
| 50 | Spinner / elapsed / status | spinner + status | braille spinner, `running Ns`, `classifying…`, queued count | same |
| 51 | Close dialogs | `Esc` | `Esc` (and `q` in pickers) | same |
| 52 | `/mcp` | `/mcp`: list, reconnect, enable, disable | none; `/trust` trusts the project's `.rkat/mcp.toml` | missing |
| 53 | `/status` | `/status`: version, model, account | `/status`: version, model and endpoint, session, workspace, confinement, mode, trust state | same |
| 54 | `/config` | `/config [key=value]` | `--set KEY=VALUE` at start, `tui.toml` | partial |
| 55 | `/export` | `/export [file]` | `/export [file]` | same |
| 56 | `/diff` | `/diff`: review working-tree changes | none | missing |
| 57 | `/review`, `/security-review` | bundled skills | none | missing |
| 58 | Custom commands / skills | `.claude/commands/*.md`, `.claude/skills/*/SKILL.md` as `/name` | none | missing |
| 59 | `/add-dir`, `/cd` | `/add-dir <path>` | `--confinement-read`/`--confinement-write` and `[confinement] extra_read`/`extra_write`, at start | partial |
| 60 | `/branch`, `/fork`, `/rename` | branch or fork the conversation; name the session | none for the user; the agent forks itself with the engine's `fork_off` | missing |
| 61 | `/btw` side question | `/btw [q]` | none | missing |
| 62 | `/doctor` | setup checkup | none | missing |
| 63 | Notifications / bell | terminal bell or notification on completion or prompt | terminal bell when a turn ends or a dialog opens while the terminal is unfocused | same |
| 64 | `/statusline` | custom status line | fixed status bar | missing |
| 65 | `/agents`, `/hooks` | subagent definitions, hook config | none (rkat-tui's `/agents` is a picker of the workspace's agents: switch, start a new one, retire an idle one; it has no subagent definitions) | missing / low priority |
| 66 | Vim mode | `/config` → Editor mode (`/vim` removed in v2.1.92) | none | missing |
| 67 | `/terminal-setup` | installs Shift+Enter in some terminals | not needed: `Ctrl+J`/`Alt+Enter` work everywhere | n/a |
| 68 | `/login`, `/logout`, `/bug`, `/feedback`, `/release-notes`, `/upgrade`, `/mobile`, `/desktop`, `/install-github-app`, `/theme` | account and product commands | none | n/a |

rkat-tui extras with no CC counterpart: `Shift+↑/↓`, `PgUp/PgDn` and
`Home/End` scroll the transcript; mouse selection copies over OSC 52;
`/helpers` folds and expands helper headers; `/trust`; `/auto` and `/auto
resume`; the daemon's `/detach`, `/agents`, `/daemon`, `/rehome`,
`/schedules` and `/audit`; `//` sends a literal `/`, `\#` a literal `#` and
`\!` a literal `!`.

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
