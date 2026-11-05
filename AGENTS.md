# Language convention

**English is the default language for every artifact in this repository.**
Spanish is for conversation only.

KISS; keep it simple stupid! philosophy.

## What is English

| Artifact | Examples |
|---|---|
| Documentation | `README.md`, `docs/**`, `design/system.md`, `design/**/*.html`, author prose in `odd/tasks/**` |
| Code comments | `//!`, `///`, `//`, `/* */`, SQL `--`, `#` in YAML/Makefiles |
| Commit messages | Conventional Commits in English |
| Identifiers a human reads | enum wire values, CLI flags, route names, DB enum labels (`owner`/`manager`) |
| Runtime strings a human reads | HTTP `detail` messages, CLI help and output, log messages |
| UI copy | every rendered string in `frontend/src` |
| API documentation | OpenAPI `summary`, `description`, `example`, schema doc comments |
| Test artifacts | test names, assertion failure messages, fixtures |

## What stays as-is

- **Verbatim user quotes** are evidence, not prose. The quotes recorded in
  `odd/tasks/*.md` (`L1`–`L4`, `L16`) and in `design/admin-ui/comments.json`
  keep their original language and wording. Never translate a quote; you would
  destroy the record of what was actually asked.
- **Stable protocol values** are not prose: HTTP header names, JSON keys, SQL
  identifiers, env var names, and enum wire values. Translate the prose around
  them, never the identifier itself.
- **Free text the operator types** into the database (product names, notes) is
  user data, not an artifact of this repository.

## Generated files

`openapi.json` and `frontend/src/lib/api/schema.d.ts` are generated. Never
hand-edit either. Translate the Rust doc comments that feed them, then:

```bash
make fe-schema   # crate spec + regenerate TS types
```

## Localization

Copy is English and the document language is `<html lang="en">`. Date, number
and currency formatting use the `en-US` locale. This is a language convention,
not a locale convention for the business: values in the database are unaffected.

## Why

One language for everything means a reviewer never context-switches, a search
never needs two spellings, and an outside contributor is never locked out.
Mixed-language repositories drift: half-translated files, duplicated
terminology, and comments that disagree with the code they describe.

Conversation language is separate. Talk to the user in whatever language they
write in; write the repository in English.
