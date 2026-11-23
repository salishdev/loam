# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

## Delivery authority

This section overrides the Conservative profile in the Beads block below for
factory Delivery sessions only: a beads-pickup session working a greenlit
ticket in its own worktree and branch. Interactive sessions keep the
Conservative default.

A Delivery session may, without asking:

- commit on its ticket's branch;
- push that branch to `origin` (with `--force-with-lease` only on the branch
  its ticket created, after a rebase);
- open a pull request for it with `gh pr create` once the ticket has
  `review-passed`;
- squash-merge that pull request with `gh pr merge --squash --delete-branch`
  once all of these hold: the ticket has `review-passed`, every CI check on the
  head commit succeeded (skipped or neutral checks count as passing), no review
  thread is unresolved, and no review requests changes. This is for
  `approve:auto`; under `approve:user` it asks the user before merging;
- while loam still lands by merge rather than by pull request: rebase onto
  local `main`, fast-forward `main` to its branch, and push `main` with a plain
  `git push origin main`.

Reserved: these stay with the user. A Delivery session raises a `hands`
Exception instead of doing them:

- pushing a release tag (`vX.Y.Z`, or any tag): it runs the Release workflow
  and publishes binaries;
- force-pushing or rewriting `main`'s history;
- pushing or force-pushing any branch other than its own ticket branch
  (the plain push of `main` above excepted).

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:1105d646 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/core-concepts/sync-concepts.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. Factory Delivery sessions instead follow "Delivery authority" above. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->


## Build & Test

```bash
just check                      # fmt, clippy, tests: what CI runs; LLM code runs against llm::ScriptedClient
# Live Claude API tests (cost money, need ANTHROPIC_API_KEY):
cargo test --test live_api -- --ignored --nocapture
```

## Architecture Overview

_Add a brief overview of your project architecture_

## Conventions & Patterns

_Add your project-specific conventions here_
