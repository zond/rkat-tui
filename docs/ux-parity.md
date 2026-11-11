# Claude Code ↔ rkat-tui UX parity: an audit of 2026-11-06, status of 2026-11-10

Claude Code (CC) facts come from the official docs at code.claude.com, fetched 2026-11-06. The pages carry no date. The newest version they mention is v2.1.309. The popularity order puts first what both the official shortcut table and the community "most used" lists (Octave HQ, claudecode-lab) name, then what only the official tables name, then niche features.

This is a dated audit. It was made on 2026-11-06 in the Meerkat monorepo, before rkat-tui moved to this repository, and drove the key and command changes that followed. The **rkat-tui control** column and sections 2 and 3 record what the audit found and proposed that day; the **Status** column is refreshed as of 2026-11-10 against `src/ui/commands.rs` (`COMMANDS`, `KEYS_HELP`) and `src/ui/keymap.rs`. [`manual.md`](manual.md), "Keys and commands", is the current reference.

Paths are relative to `src/` (the audit's `crates/meerkat-tui/src/`): `keymap` = `ui/keymap.rs`, `mod` = `ui/mod.rs` (since split by concern into `ui/keys.rs`, `ui/turns.rs`, `ui/delivery.rs` and others), `cmd` = `ui/commands.rs`, `render` = `ui/render.rs`, `modal` = `ui/modal.rs`, `search` = `ui/search.rs`. The line numbers are the audit's and no longer match.

## 1. Parity table

| # | Feature | Claude Code control | rkat-tui control (2026-11-06) | Status (2026-11-10) |
|---|---|---|---|---|
| 1 | Send prompt | `Enter` | `Enter` (keymap:93; mod:1531 → `submit` mod:1726) | same |
| 2 | Interrupt / clear input / quit | `Ctrl+C`: interrupts a run; when idle the 1st press clears the input and the 2nd exits | `Ctrl+C`: interrupts, else clears the input, else a 2nd press within 1.5 s quits (keymap:95; mod:2188; window mod:59) | same |
| 3 | Exit | `Ctrl+D`: 1st press shows a hint, 2nd within 800 ms exits; with text it deletes forward | `Ctrl+D` on an empty input quits **at once** when idle and asks again within 3 s only while busy; with text it deletes forward (keymap:96; mod:1545, 2213) | partial (the second press may come within 3 s, not 800 ms) |
| 4 | Newline | `Shift+Enter`, `Ctrl+J`, `\`+`Enter`, `Option+Enter` | `Shift/Alt/Ctrl+Enter`, `Ctrl+J` (keymap:92,94) | same (`Shift/Alt+Enter`, `Ctrl+J`, `\`+`Enter`; `Ctrl+Enter` sends now, as in CC) |
| 5 | Prompt history | `Up/Down` (also `Ctrl+P/Ctrl+N`); moves by visual rows first | `Up/Down`, by wrapped rows, then history (keymap:122-123; `manual.md` "Keys and commands") | same (`Ctrl+P/N` too) |
| 6 | Interrupt the agent | `Esc` stops the turn; queued messages are sent next | `Esc` first **recalls** queued messages and interrupts only when nothing is queued (keymap:110; mod:1940-1951) | same (`Esc` interrupts and sends the queued messages next) |
| 7 | Rewind / checkpoints | `Esc Esc` on an empty input, or `/rewind` (`/checkpoint`, `/undo`): restore code and/or conversation, or summarize from/up to a point | none | missing: `/rewind` (`/checkpoint`, `/undo`) and `Esc Esc` on an empty input only explain why it is not available (the engine cannot rewind a mob member's session; lukacf/meerkat#2126 is a merged design, not implemented) |
| 8 | Clear draft | `Esc Esc` with text: clears it and saves it to history | `Ctrl+C` clears without saving to history (mod:2192); `Esc` does nothing with text | same (`Esc Esc` with text clears it into the history) |
| 9 | Permission mode cycle (manual → acceptEdits → plan → auto) | `Shift+Tab` | `/auto on\|off\|resume` (cmd:261); `Shift+Tab`/BackTab is unbound (keymap:130) | same (`Shift+Tab` cycles manual → accept edits → plan → auto; `/mode` sets one) |
| 10 | Reverse history search | `Ctrl+R` (again: older). Classic renderer: `Enter` runs the match, `Tab`/`Esc` accept, `Ctrl+C` cancels. Fullscreen dialog: `Enter`/`Tab` place the match, `Esc` cancels | `Ctrl+R` (again: older); `Enter` or any arrow/Home/End accepts into the editor; `Esc`/`Ctrl+G`/`Ctrl+C` cancel (keymap:99; search:153-172; hint render:189) | same key; behaviour matches CC's fullscreen dialog (no `Tab` accept) |
| 11 | Queue messages while the agent works | `Enter` queues; messages join after the current tool calls | `Enter` steers into the run at the next boundary (mod `enqueue`; hint render:197) | same |
| 12 | Send queued now | `Ctrl+Enter` or `Ctrl+X Ctrl+S` | `Ctrl+X Ctrl+S`: interrupt and send the input (mod:1513-1517, 1593) | same (`Ctrl+Enter` and `Ctrl+X Ctrl+S`) |
| 13 | Take back queued messages | `Up` from the first line | `Up` on an empty input edits the newest queued message (mod:1553); `Esc` recalls all of them (mod:1940) | same (`Up` on an empty input) |
| 14 | `/help` | `/help`; `?` on an empty input toggles a shortcut panel | `/help`, `/?` (cmd:240, text cmd:137) | same (`?` panel) |
| 15 | `/clear` | `/clear [name]` | `/clear`, `/new` (cmd:239) | same |
| 16 | `/compact` | `/compact [instructions]`: compacts now, with optional focus | `/compact` **shows the policy**; `/compact now` compacts on the next call (cmd:245) | partial: `/compact` asks the engine to compact at the next model call; focus instructions are ignored with a notice; `/compact status` shows the policy |
| 17 | `/exit`, `/quit` | `/exit` (alias `/quit`) | `/quit`, `/exit`, `/q` (cmd:238) | same |
| 18 | `/model` | `/model [model]`, opens a picker | `/model [[endpoint:]id\|refresh]`, opens a filtered picker (cmd:255; picker mod:1670-1695) | same |
| 19 | `/resume`, `--continue`, `--resume` | `/resume [session]`; `claude -c`; `claude --resume [id]` (opens a picker with no id) | `/resume [ref]` (cmd:250), picker (mod:1655-1668); `-c/--continue` (cli.rs:68); `--resume <ref>` needs a value (cli.rs:64) | same (`--resume` alone opens the picker) |
| 20 | `/cost` / `/usage` | `/usage`, with `/cost` and `/stats` as aliases | `/cost` (cmd:241) | same (`/usage`, aliases `/cost`, `/stats`) |
| 21 | Expand detail (transcript viewer) | `Ctrl+O`: tool details, thinking, collapsed lines | `Ctrl+O` toggles the **last reasoning** block (keymap:97; mod:1556; hint render:562) | partial (`Ctrl+O` shows the last reasoning block and the last tool output together, not a whole view) |
| 22 | Task / todo checklist | `Ctrl+T` toggles the checklist | `Ctrl+T` toggles the **last tool output** (keymap:98; mod:1559). No checklist is rendered, though the task tools exist (approval/policy.rs:130-132) | same (`Ctrl+T` task checklist) |
| 23 | `@` file mention with autocomplete | `@` | none | same |
| 24 | Shell mode | `!` prefix runs a command; its output joins the context | none (`!` is sent as text) | same |
| 25 | `/` command menu and autocomplete | type `/`; `Tab` accepts | none: only an "unknown command" error (cmd:126); `Tab` unbound | same |
| 26 | Permission prompt | Yes / Yes, don't ask again (rule) / No, and tell Claude what to do (`Esc`). Arrow keys or number keys; `Tab` adds a comment | `y` allow once, `a` allow + rule, `n`/`Esc` deny, `s` stop the turn, `Ctrl+C` stop all (mod:1640-1648; modal:283-291) | same (`1`/`2`/`3`, arrows and `Enter`, a comment on No; `y`/`a`/`n`/`s` as accelerators) |
| 27 | Plan mode | `Shift+Tab` to plan, `/plan [desc]`, `--permission-mode plan`; plan approval dialog | none | partial (`Shift+Tab` and `/mode plan`; no `/plan` command and no plan-approval dialog) |
| 28 | Auto-accept edits | `Shift+Tab` → acceptEdits | auto mode (classifier) covers edits (`/auto on`, cmd:261) | same (accept edits mode) |
| 29 | `/permissions` | `/permissions`: interactive allow/ask/deny rules | `/rules [list\|add allow\|deny <tool> [pattern]\|rm <n>\|trust]` (cmd:268, 290-313) | same (`/permissions`, alias `/rules`) |
| 30 | `/context` | `/context [all]`: usage grid | status bar gauge `ctx 42% (84k/200k)` (render:366-390) | same (`/context`) |
| 31 | Context warnings / auto-compact | warns, then auto-compacts; `/autocompact` sets the window | warnings at 75%/85%, `auto.compact` and `--auto-compact` (`manual.md` "Context and compaction"; cmd text :157) | same (no `/autocompact`) |
| 32 | `/memory` | `/memory`: edit CLAUDE.md, toggle auto memory | none (`/remember` only appends) | same (`/memory`) |
| 33 | `#` memory prefix | **Removed** from CC (no longer in the quick-commands table; the memory docs say to ask Claude or use `/memory`) | `#text` and `/remember <text>` append to AGENTS.md (cmd:192, 219) | n/a (legacy CC control rkat-tui still has) |
| 34 | `/init` | `/init` writes CLAUDE.md | none | same (`/init`) |
| 35 | Image paste | `Ctrl+V` (`Alt+V` on Windows/WSL) inserts an `[Image #N]` chip | text bracketed paste only (mod:1277) | missing |
| 36 | External editor | `Ctrl+G` or `Ctrl+X Ctrl+E` | none (`Ctrl+X` then `Ctrl+E` cancels the chord, mod:1519) | same |
| 37 | Redraw | `Ctrl+L` | none (keymap:108 → None) | same |
| 38 | Background a task | `Ctrl+B`; `/tasks` (`/bashes`) | none | missing |
| 39 | Subagents: watch and expand | collapsed subagent lines, `Ctrl+O` to expand; `/tasks` lists background subagents; `Ctrl+X Ctrl+K` stops them | helper panel with `Ctrl+↑/↓` select and `Enter` for the full view (keymap:118-119; mod:1531; render:191-201); click a header to unfold; `/helpers [expand\|fold]` (cmd:273); `Ctrl+C` cancels helpers | differs (rkat-tui is richer; `/tasks` is an alias of `/helpers`) |
| 40 | Thinking toggle | `Alt+T` (`Option+T`); `/effort` | none (only `Ctrl+O` display) | missing (`[thinking]` in `tui.toml` per model only) |
| 41 | Switch model, keeping the prompt | `Alt+P` (`Option+P`) | none (`Alt+P` → None, keymap:108) | same |
| 42 | Undo an input edit | `Ctrl+_` | none | same |
| 43 | Readline editing: `Ctrl+A/E/U/W`, `Alt+B/F` | same keys | `Ctrl+A/E/W/U` (keymap:102-105), `Alt+B/F` (keymap:106-107), `Ctrl/Alt+←/→` and `Ctrl/Alt+Backspace` (keymap:111-115) | same (with a kill ring) |
| 44 | Kill to end of line / yank | `Ctrl+K`; `Ctrl+Y` yank; `Alt+Y` cycles | `Ctrl+K` unbound; `Ctrl+Y` = **copy** the selection or the last answer (keymap:100; mod:1186) | same (`Ctrl+K`, `Ctrl+Y` yank, `Alt+Y` cycles) |
| 45 | Copy the last answer | `/copy [N]` | `Ctrl+Y` (mod:1186-1196), mouse drag (`manual.md` "Keys and commands") | same (`/copy [N]`, mouse drag) |
| 46 | Stash the prompt | `Ctrl+S` | none (`Ctrl+S` acts only after `Ctrl+X`) | same |
| 47 | Suspend | `Ctrl+Z` | none | same |
| 48 | Diff display for edits | colored diffs on Edit/Write | unified diffs attached to tool blocks (render:494; transcript.rs:244, 944) | same |
| 49 | Markdown rendering | yes | yes (ui/markdown.rs) | same |
| 50 | Spinner / elapsed / status | spinner + status | braille spinner, `running Ns`, `classifying…`, queued count (render:29, 421-471) | same |
| 51 | Close dialogs | `Esc` | `Esc` (and `q`) closes pickers (mod:1667, 1674) | same |
| 52 | `/mcp` | `/mcp`: list, reconnect, enable, disable | `/mcp trust` only (cmd:269) | partial (`/mcp trust` only) |
| 53 | `/status` | `/status`: version, model, account | status bar only (render:421-471) | same (`/status`) |
| 54 | `/config` | `/config [key=value]` | `--set KEY=VALUE` at start (cli.rs:36), `tui.toml` | partial |
| 55 | `/export` | `/export [file]` | none | same (`/export [file]`) |
| 56 | `/diff` | `/diff`: review working-tree changes | none | missing |
| 57 | `/review`, `/security-review` | bundled skills | none | missing |
| 58 | Custom commands / skills | `.claude/commands/*.md`, `.claude/skills/*/SKILL.md` as `/name` | none | missing |
| 59 | `/add-dir`, `/cd` | `/add-dir <path>` | `--confinement-read/--confinement-write` at start (cli.rs:128,134) | partial |
| 60 | `/branch`, `/fork`, `/rename` | branch or fork the conversation; name the session | agent-facing `fork` tool only (`manual.md` "Helpers") | missing (the agent forks itself with the engine's `fork_off`; there is no user-facing fork) |
| 61 | `/btw` side question | `/btw [q]` | none | missing |
| 62 | `/doctor` | setup checkup | none | missing |
| 63 | Notifications / bell | terminal bell or notification on completion or prompt | none | same (terminal bell while unfocused) |
| 64 | `/statusline` | custom status line | fixed status bar (render:421) | missing |
| 65 | `/agents`, `/hooks` | subagent definitions, hook config | none | missing / low priority |
| 66 | Vim mode | `/config` → Editor mode (`/vim` removed in v2.1.92) | none | missing |
| 67 | `/terminal-setup` | installs Shift+Enter in some terminals | not needed: `Ctrl+J`/`Alt+Enter` work everywhere | n/a |
| 68 | `/login`, `/logout`, `/bug`, `/feedback`, `/release-notes`, `/upgrade`, `/mobile`, `/desktop`, `/install-github-app`, `/theme` | account and product commands | — | n/a |

rkat-tui extras with no CC counterpart: `Shift+↑/↓`, `PgUp/PgDn` and `Home/End` scroll the transcript (keymap:120-129); mouse selection copies over OSC 52; `/helpers` fold and expand; `/rules trust` and `/trust` (alias `/mcp trust`); `/auto resume`; `//` sends a literal slash (cmd:214); `\#` sends a literal `#`.

## 2. Rebind to match

As proposed on 2026-11-06. Items 1, 2, 4 to 13 and 15 are done (see the Status column); 3 (send queued plus draft without interrupting) and 14 (classic history search) were left as they were.

1. **`Esc` while running** (mod:1940): recall queued messages → **interrupt, and send the queued messages next** (CC). Recall stays on `Up` (mod:1553), which already matches CC.
   - Conflict: today an interrupt returns undelivered steers to the editor (`manual.md` "Sessions"). CC sends them next. The runner's interrupt path must re-admit them instead of withdrawing them.
   - The hints at render:195-197 must change.
2. **`Ctrl+Enter`**: newline (keymap:92, `ctrl` arm) → **send queued now**, the same action as `Ctrl+X Ctrl+S` (CC `chat:sendNow`).
   - Keep `Shift+Enter`, `Alt+Enter` and `Ctrl+J` for newlines.
   - Add **`\`+`Enter`** as a newline (submit checks for a trailing backslash).
   - Conflict: anyone who uses `Ctrl+Enter` for newlines. Many terminals send it as plain `Enter`, as CC's docs note.
3. **`Ctrl+X Ctrl+S` semantics** (mod:1593): today it interrupts and sends the input. CC sends the queued messages plus the draft, and moves backgroundable work to the background instead of interrupting.
   - Same key; only the description differs. Aligning it needs background tasks (candidate 13), so keep it for now.
4. **`Ctrl+T`** → **`Ctrl+O`**: today `Ctrl+O` toggles the last reasoning block (keymap:97) and `Ctrl+T` the last tool output (keymap:98). In CC, `Ctrl+O` is the single "show me detail" key (tool output and thinking), and `Ctrl+T` is the todo checklist.
   - Merge both into `Ctrl+O`: toggle a verbose mode that expands every reasoning block and tool output in the active view, or, as a minimal step, toggle the last reasoning block and the last tool output together.
   - That frees `Ctrl+T` for a task checklist.
   - Conflict: `Ctrl+T` loses its "last tool output" meaning. A mouse click still toggles a single tool (`manual.md` "Keys and commands").
   - Update hints render:562 and render:700, and HELP_TEXT cmd:175-176.
5. **`Ctrl+Y`**: copy (keymap:100; mod:1186) → **yank** the last killed text (CC).
   - Move copying to **`/copy [N]`** (CC: copy the Nth-latest answer). Mouse drag already copies the selection on release.
   - This needs a kill ring in `ui/input.rs` fed by `Ctrl+W`, `Ctrl+U` and a new `Ctrl+K`.
   - Conflict: the "copy again" path for selections (mod:1259 special-cases `Ctrl+Y` so it does not clear the selection). Drop that path, or keep copy-on-release as the only selection copy.
6. **`/compact`** (cmd:245): show the policy → **compact now, with optional focus instructions** (`/compact [instructions]`).
   - Move the policy display to `/context` or `/compact status`.
   - Conflict: `/compact now` would parse `now` as instructions, so keep `now` as a recognised no-op word for one release.
   - Meerkat has no instruction-guided compaction, so the instructions would need the CompactionCurator seam, or rkat-tui ignores them with a notice.
7. **`/rules`** → **`/permissions`** (cmd:268). Add `/permissions` as the primary name and keep `/rules` as an alias.
8. **`/auto on|off`** → **`Shift+Tab`** cycles the mode, with `/auto` kept for status and resume.
   - Map `KeyCode::BackTab` in keymap.rs. The cycle is off → on, and later off → acceptEdits → plan → auto once those modes exist.
   - No current binding conflicts: BackTab falls through to `None` (keymap:130).
9. **Approval prompt** (mod:1645-1648; modal:283-291): add **`1`/`2`/`3`** (yes / yes + rule / no) and an arrow-plus-`Enter` selection, as in CC. Keep `y`/`a`/`n`/`s` as accelerators. `Esc` = deny already matches. No conflicts.
10. **`/helpers`** → add **`/tasks`** as an alias (CC's command for running subagents and background shells). Keep `/helpers expand|fold`. No conflicts.
11. **`/cost`** → add **`/usage`** as the primary name (CC made `/cost` an alias). No conflict.
12. **`Ctrl+D`**: add the idle confirmation (a hint on the 1st press, quit on the 2nd). Optionally shorten the window to CC's 800 ms; rkat-tui uses 3 s (mod:63). No conflict.
13. **`--resume`** with no value should open the picker (cli.rs:64: make it `Option<Option<String>>` or `num_args = 0..=1`).
14. **History search** (search:161-167): rkat-tui matches CC's fullscreen dialog. If the classic behaviour is wanted, make `Tab` accept too. `Enter` currently accepts without running, which is the safer choice; leave it.
15. **`Ctrl+P`/`Ctrl+N`** as aliases for history Up/Down (both unbound now, keymap:108).

## 3. Candidate new features (by popularity)

As proposed on 2026-11-06; the Status column says which exist now.

1. **`/` command menu and `Tab` completion** (S–M). As you type `/x`, a filtered list of commands appears; `Tab` inserts the match. Builds on the command table in `cmd` (move HELP_TEXT to structured entries) and on the picker widget in `modal` and `mod:1670`.
2. **`Esc Esc` rewind (`/rewind`) and draft clear** (conversation: M; code: L).
   - The draft-clear half is S: clear the input and push it to history.
   - Conversation rewind was proposed as a rebuild of the session from a truncated transcript. Done as Esc Esc draft clear; rewind is explanation-only today: the agent is a mob member in the daemon, and the engine refuses a transcript fork of a member's session (`/compact now` no longer rebuilds anything either: it asks the engine to compact at the next model call).
   - Code restore needs per-turn file snapshots taken in `tools/write.rs` and `tools/edit.rs`.
3. **`!` shell mode** (M). Runs a command and adds its output to the context. Reuse the confined shell runner (Confinement) and send the output as a user block, the way `begin_turn` sends a prompt (mod `begin_turn`).
4. **`@` file mentions with path autocomplete** (M). Reuse `tools/glob.rs` for candidates and the picker UI; on submit, insert a reference or the file contents.
5. **`Shift+Tab` modes, plan mode and acceptEdits** (M–L).
   - acceptEdits: an approval-policy flag that tiers edit tools as allowed (approval/policy.rs).
   - Plan mode: deny write and shell tiers, add a system note, and show an approve-plan dialog (reuse the escalation modal).
6. **Permission-prompt comment ("No, and tell Claude what to do")** (S). A text field in the escalation modal; the deny reason goes back as the tool error text. Builds on `EscalationAnswer::Deny` and modal.rs.
7. **Todo checklist on `Ctrl+T`** (M). Render Meerkat's `task_*` tool state (approval/policy.rs:130) as a checklist panel. The helper panel's layout (render `draw_input`) is the template.
8. **`/memory` and `/init`** (S each). `/memory` opens AGENTS.md, notes and SYSTEM.md in `$EDITOR` (shares code with `Ctrl+G`). `/init` is a canned prompt that drafts AGENTS.md; protected-path writes still ask the user.
9. **`Ctrl+G` / `Ctrl+X Ctrl+E` external editor** (S). Suspend the terminal, run `$VISUAL`/`$EDITOR` on a temp file, and load the result into `ui/input.rs`. The chord prefix already exists (mod:1505).
10. **`Ctrl+V` image paste** (M–L). Read the clipboard image (`arboard` or `wl-paste`/`xclip`), attach multimodal content to the prompt, and show an `[Image #N]` chip. Meerkat supports multimodal content; `Effect::StartTurn(String)` would need a content type.
11. **`Ctrl+L` redraw** (S). `terminal.clear()` in ui/runtime.rs.
12. **`/context` command** (S). Prints the gauge with a breakdown (system prompt, AGENTS.md, tools, history) and the policy, absorbing `/compact`'s current output.
13. **`Ctrl+B` background tasks and `/tasks`** (L). Detach a running shell or helper call. Builds on Meerkat's shell jobs, which the engine cannot run for a daemon member yet (`docs/evidence.md`, "Background shell jobs"), and on the helper panel.
14. **`Alt+T` thinking toggle and `/effort`** (S–M). A model-parameter override on the session build (`/model` switch path, mod `switch_model`).
15. **`Alt+P` model switch** (S). Opens the `/model` picker without clearing the input.
16. **`?` shortcut panel on an empty input** (S). Render HELP_TEXT's "Keys" section as an overlay.
17. **Editor polish: `Ctrl+_` undo, `Ctrl+K` with a `Ctrl+Y`/`Alt+Y` kill ring, `Ctrl+S` stash, `Alt+D`** (S each). All in `ui/input.rs` and keymap.rs.
18. **Notifications / bell** (S). Emit BEL or OSC 9/777 when a turn ends or an escalation opens while the terminal is unfocused (crossterm focus events).
19. **`/export [file]` and `/copy [N]`** (S). Serialize transcript blocks to text; reuse `copy_text` (mod:1199).
20. **`/diff`** (S–M). Show `git diff` in a scrollable view, reusing the helper-view scroll and `diff_line_style` (render:494).
21. **Custom commands and skills** (`.rkat/commands/*.md` → `/name`, plus `/review` and `/security-review` as bundled prompts) (M). Use meerkat-skills for loading; prompt templates with `$ARGUMENTS`.
22. **`/branch` and `/rename`** (M / S). A user-facing fork of the session; Meerkat's session fork and the `fork` tool were the basis (the TUI's `fork` tool has since been removed for the engine's `fork_off`).
23. **`/mcp` list and reconnect** (M). Builds on the McpRouter that `/mcp trust` already touches.
24. **`/status`, `/doctor`** (S). Version, model, endpoint, realm, confinement and trust state; a doctor that checks OSC 52, sandbox availability and endpoint keys.
25. **`/btw` side question** (M). A one-off model call with the transcript as context that does not enter history (classifier-style side call).
26. **`Ctrl+Z` suspend** (S). Leave raw mode and the alternate screen, `raise(SIGTSTP)`, and restore on SIGCONT.
27. **`/statusline` custom status line** (M). Run a configured command and show its output in the status bar.
28. **Vim editor mode** (L). Modal editing in `ui/input.rs`.

## 4. Sources

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
