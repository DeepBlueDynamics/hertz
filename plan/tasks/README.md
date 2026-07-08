# Hertz build — worker coordination rules

Two worker agents build this project in parallel under a supervisor (Claude, pane
"Overwhelming Albatross 🥨"). Read `plan/PLAN.md` (the architecture + phases) and
`plan/gnosis-feature-inventory.md` (parity checklist) before starting your task.
Reference source from the old project is vendored at `plan/reference/gnosis-radio/`.

## Rules (both workers)

1. **Path ownership.** Only create/edit files inside the paths your task brief assigns you.
   Never touch the other worker's paths, `plan/` briefs, or `.claude/`.
2. **No git.** Do not run `git commit`, `git push`, `git checkout`, or anything that mutates
   git state. The supervisor reviews and commits. `git status`/`git diff` are fine.
3. **Quality gate before reporting done:** `cargo fmt`, `cargo clippy --all-targets` clean
   (warnings ok if justified in status), `cargo test` passing for your crate(s).
4. **Status file.** Maintain `plan/tasks/STATUS-<yourname>.md` — overwrite it with: what's
   done, what's in progress, what's blocked (and why), and how you verified. Update it when
   you finish, hit a blocker, or change direction. This is how the supervisor checks work.
5. **Blocked?** Write the blocker in your status file and stop that thread of work; pick up
   the next item in your brief rather than idling or improvising outside your paths.
6. **Rust edition 2021, workspace deps.** Keep dependencies minimal and pinned in the
   workspace root; no new heavyweight deps without noting it in the status file.
7. If a tool you need is missing in your container (rustup/cargo), install it yourself
   (rustup.rs, default stable toolchain) and note it in status.

## Current assignments

| Worker | Brief |
|---|---|
| Cognitive Haddock 🌀 | `plan/tasks/T1-haddock-scaffold.md` |
| Prior Sloth 🫖 | `plan/tasks/T2-sloth-dsp.md` |

Later phases (daemon, TUI, MCP, transcription, container) are assigned by the supervisor
after these land.
