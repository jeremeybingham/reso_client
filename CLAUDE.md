# CLAUDE.md

@AGENTS.md

This file holds the process we follow with Claude Code; the rules and commands are in `AGENTS.md` (imported above).
Machine facts and standing preferences are in `~/.claude/CLAUDE.md`.

## Workflow

- Before finishing a task run `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test`; report anything that fails or
  was not run.
- Keep `AGENTS.md` and `README.md` in step with the code: when commands, rules, layout or the roadmap change, update
  them in the same change.
- Work from the release tracker (#179) and its milestones (`0.4.0`, then `0.5.0`), one PR per issue; say in the PR body which part is unconfirmed against a
  real server (#32). Use "Part of #N" rather than a closing keyword unless the PR finishes the issue.
- Before every commit or push, `git fetch origin` and check the branch's PR is not already merged and `origin/main` has
  not moved (restart from `origin/main` if it has merged).
- Do not add dependencies, publish to crates.io or tag releases without the owner's go-ahead.
- Put decisions you made where the request left one open in the PR body, so the owner can overrule them.

## Local notes

Project state (where things stand, next step, open decisions) is in `SESSION.md` (git-ignored); rewrite its "Where things
stand" block at the end of a session (`/handoff`).

@SESSION.md
