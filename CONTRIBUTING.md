# Contributing

This repository is primarily developed by its owner, so the process below is
deliberately light. It exists so that the history stays readable and so that
someone joining later does not have to guess how work is expected to arrive.

## How work arrives

1. **Open an issue first.** One issue is one intent. Describe what is wrong or
   missing and how you would know it is fixed; leave the implementation out.
2. **The owner triages it** and applies `status:approved` when it is agreed and
   scoped. Anything unlabelled is not yet approved, and a PR against it may be
   asked to wait.
3. **Open the PR against `main`** and link the issue: `Closes #12` if the PR
   finishes it, `Refs #12` if it only advances it.
4. **Apply exactly one `type:*` label** from the list in the PR template.

There is **no automated gate on any of this.** CI checks formatting, lints,
tests, the frontend type-check and the committed generated files; it does not
check issue linkage or labels. The convention is enforced by review, not by a
workflow, and a forgotten label is a comment rather than a red build.

## Why there is no gate

A workflow that validates "every PR links an approved issue" would be correct in
a repository with several contributors and an external issue tracker. Here it
would cost a required status check and a browser round trip before every branch
for the same person, and it would still be satisfiable by opening an issue and a
PR in the same minute. The `odd/tasks/*.md` feature documents already record
what was asked (verbatim), what was decided and what is missing.

This is revisit-able, not a permanent position: the moment a second person can
open a PR, or an issue arrives from outside, add the gate. The pieces that
make that cheap are already in place — the templates, the `status:*` labels and
the `type:*` labels.

## Labels

| Label | Meaning |
|---|---|
| `status:approved` | Triaged and agreed; a PR may close it |
| `status:needs-info` | Waiting on the reporter |
| `status:wontfix` | Decided against. Do not implement |
| `type:feature` | New capability or behavior |
| `type:bug` | Fixes broken behavior |
| `type:docs` | Documentation only |
| `type:refactor` | Same behavior, different structure |
| `type:chore` | Maintenance, tooling, build or CI |
| `type:breaking-change` | Breaks an existing contract |
| `dependencies`, `rust`, `javascript` | Applied by Dependabot |

Exactly one `type:*` per PR.

## Commits

Conventional Commits, in English:

```
type(scope): description
```

`type` is one of `build`, `chore`, `ci`, `docs`, `feat`, `fix`, `perf`,
`refactor`, `revert`, `style`, `test`. A work unit is a commit a reviewer can
judge alone: the behavior, its tests and the docs that describe it, together.
Tests and docs are never a separate commit from the code they describe.

Do not add `Co-Authored-By` trailers.

## Language

English for everything that lives in the repository: code, comments, commit
messages, docs, UI copy, test names. The reasoning is in `AGENTS.md`.
Conversation language is separate and personal.

## Before opening a PR

```bash
make check
```

That runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and the
frontend type-check. It needs the database up (`make db-up`); `make migrate`
first time.

If you changed a query, run `make prepare` and commit the regenerated `.sqlx/`
cache. If you changed an API type, run `make fe-schema` and commit the
regenerated `openapi.json` and `schema.d.ts`. CI fails when either is stale.

## What a PR should say

Fill the template. The parts that matter:

- **The test plan lists commands that were run**, with their results. A check
  that did not apply says so; it does not get a green tick.
- **Unavailable proof is named.** If a verifier could not run, or a check is
  pending, it appears in the PR instead of being silently omitted.
- **Advisory findings you decided not to act on are written down**, with the
  reason. The next person should not have to rediscover them.
