# Review Command

## Overview

`dex review` performs a multi-round code review of the current implementation. It runs specialized AI reviewers in parallel, collects findings, applies fixes via a fixer agent, and iterates through focused review rounds until the code is clean or a maximum number of rounds is reached.

The design mirrors `dex apply`: a persistent checkbox-based plan file (`.dex/plan-review.md`) tracks progress. Re-running `dex review` resumes from the first incomplete step — no special `--resume` flag needed.

## Overall Flow

```
dex review [--parallel N] [--from REF] [--force]
        |
        v
  1. Resolve base ref (impl_commits.jsonl or --from)
        |
        v
  2. Load or generate plan-review.md
        |
        v
  3. Loop: pick next open checkbox from plan-review.md
        |
        +-- Broad Review checkbox --> run broad reviewer, mark done
        |
        +-- Broad Fixer checkbox --> collect broad issues, run fixer, mark done
        |
        +-- Focused Review checkbox --> run focused reviewer, mark done
        |       (if all reviewers in round report ZERO FINDINGS --> early exit)
        |
        +-- Focused Fixer checkbox --> collect focused issues, run fixer, mark done
        |
        v
  4. No open checkboxes remaining --> "All review steps complete!"
```

Source: `src/phases.rs:468-586` (`review_phase`), `src/main.rs:525-548` (dispatch).

## Step-by-Step Detail

### 1. CLI Entry and Base Ref Resolution

**Source**: `src/main.rs:525-548`

The `ReviewCmd` struct defines three flags:

| Flag | Purpose |
|------|---------|
| `--parallel N` | Max reviewers to run concurrently (default: all) |
| `--from REF` | Base git ref for the review diff (when no impl history exists) |
| `--force` | Delete all review artifacts and regenerate from scratch |

Base ref resolution order:
1. Read `impl_commits.jsonl` — the `before` SHA of the first commit recorded during `dex apply` (`src/core.rs:541-551`)
2. Fall back to `--from` flag value (validated via `git rev-parse --verify`)
3. Error if neither is available

### 2. Review Plan Generation / Loading

**Source**: `src/phases.rs:345-374` (`generate_review_plan`), `src/phases.rs:488-512`

On first run (or `--force`), `review_phase` generates `.dex/plan-review.md` from the reviewer definitions. The plan uses the same heading + checkbox format as `plan.md`, so existing `plan.rs` parsing functions work without modification.

**Plan format**:

```markdown
<!-- base_ref: abc123def -->

## Broad Review
- [ ] quality
- [ ] implementation
- [ ] simplification
- [ ] testing
- [ ] documentation

## Broad Fixer
- [ ] fix broad review findings

## Focused Review — Round 1/3
- [ ] critical-correctness
- [ ] critical-coverage

## Focused Fixer — Round 1
- [ ] fix focused round 1 findings

## Focused Review — Round 2/3
...
## Focused Fixer — Round 3
- [ ] fix focused round 3 findings
```

Key behaviors:
- The `<!-- base_ref: ... -->` HTML comment on line 1 stores the base ref used when the plan was generated, ensuring consistency across re-runs (`src/phases.rs:376-381`)
- If `plan-review.md` already exists and `reviewers.json` has changed, a warning is printed but the plan is not regenerated — the user must use `--force` (`src/phases.rs:509-511`)
- On `--force`, all `review-*.md` files in `.dex/` are deleted before regenerating the plan (`src/phases.rs:489-497`)

### 3. Broad Review

**Source**: `src/phases.rs:620-727` (`run_review_fanout`), `src/phases.rs:525-530`

The main loop calls `first_open_checkbox()` (`src/plan.rs:83-97`) to find the next unchecked item. For each broad reviewer checkbox, it:

1. **Prepares the prompt** — Renders `prompts/review.txt` with the reviewer's name, scope, and specialized prompt (`src/phases.rs:635-652`)
2. **Runs reviewers in parallel** — Spawns one thread per reviewer (bounded by `--parallel`), each invoking the AI agent via `Runner::run()` (`src/phases.rs:655-693`)
3. **Collects results** — Reads each `.dex/review-<name>.md` output file, classifies as "clean" (matches `ZERO FINDINGS` / `no issues` pattern) or "has issues" (`src/phases.rs:695-726`)
4. **Marks the checkbox done** — Calls `mark_review_step_done()` which replaces `- [ ] <name>` with `- [x] <name>` under the correct heading, without crossing heading boundaries (`src/phases.rs:383-411`)

The review prompt (`prompts/review.txt`) instructs the agent to:
- Read the implementation plan
- Run `git log` and `git diff` against the base ref
- Read source files in full context
- Report problems only (no positive observations)
- Write findings to `.dex/review-<name>.md` in a structured format

### 4. Broad Fixer

**Source**: `src/phases.rs:531-541`, `src/phases.rs:729-744` (`run_fixer`)

After all broad reviewers complete, the fixer step:

1. **Collects issues** — `collect_issues_from_reviewers()` (`src/phases.rs:595-617`) re-reads all broad review files, skipping those that match the clean pattern, and joins remaining findings with `──` separators
2. **Skips if clean** — If all broad reviewers reported zero issues, the fixer checkbox is auto-checked without running an agent (`src/phases.rs:534-535`)
3. **Runs the fixer** — Renders `prompts/fix.txt` with the collected issues and invokes the AI agent (`src/phases.rs:729-744`)
4. **Agent commits fixes** — The fixer prompt instructs the agent to verify each finding, fix confirmed issues, run tests/linter, and commit with `git commit -m "fix: address code review findings"`

The fixer prompt (`prompts/fix.txt`) enforces a strict workflow:
- Collect and deduplicate findings across reviewers
- Verify every finding against actual code (reject false positives)
- Fix only confirmed issues — no new features or refactoring
- Always commit fixes

### 5. Focused Review Rounds

**Source**: `src/phases.rs:542-571`

Up to 3 rounds of focused review run after the broad fixer. Each round uses 2 focused reviewers (`critical-correctness` and `critical-coverage`) that target only critical and major issues.

The round number is extracted from the heading via `extract_round_from_heading()` (`src/phases.rs:588-593`).

**Early exit**: After each focused reviewer completes, if all reviewers in the current round reported zero issues, `mark_remaining_skipped()` (`src/phases.rs:413-429`) marks all remaining unchecked checkboxes across the entire plan as `- [x] ... (skipped — prior round clean)` and the review phase returns immediately (`src/phases.rs:557-569`).

### 6. Focused Fixer

**Source**: `src/phases.rs:572-584`

Same mechanism as the broad fixer, but scoped to focused reviewer findings. The checkbox name includes the round number (e.g., `fix focused round 1 findings`) to ensure uniqueness.

### 7. Progress Tracking and Resume

**Source**: `src/phases.rs:517-521`, `src/plan.rs:83-97` (`first_open_checkbox`)

Progress is tracked entirely through the checkbox state in `plan-review.md`:

- `first_open_checkbox()` scans the plan file for the first `- [ ]` line and returns the associated heading and checkbox name
- `mark_review_step_done()` checks a specific checkbox under a specific heading (heading-scoped to avoid cross-heading name collisions)
- `mark_remaining_skipped()` bulk-checks all remaining open checkboxes with a skip note

**Resume behavior**: Re-running `dex review` (without `--force`) reads the existing `plan-review.md` and continues from the first open checkbox. Completed reviewers are not re-invoked. This matches how `dex apply` works — no special `--resume` flag needed.

**Base ref consistency**: If `plan-review.md` exists, the stored `base_ref` (from the HTML comment) overrides the CLI-provided one, ensuring the diff doesn't change between invocations (`src/phases.rs:514-515`).

## Key Files

| File | Purpose |
|------|---------|
| `.dex/plan-review.md` | Checkbox-based progress tracker (auto-generated) |
| `.dex/review-<name>.md` | Individual reviewer output (one per reviewer) |
| `.dex/reviewers.json` | Reviewer role definitions (seeded from built-in defaults) |
| `prompts/review.txt` | Handlebars template for reviewer prompts |
| `prompts/fix.txt` | Handlebars template for fixer prompts |
| `prompts/reviewers.json` | Built-in default reviewer definitions |

## Reviewer Roles

### Broad Reviewers (5)

| Name | Scope | Focus |
|------|-------|-------|
| `quality` | bugs, security, correctness, simplicity | Logic errors, security vulnerabilities, error handling, resource management, concurrency, simplicity |
| `implementation` | goal coverage, wiring, completeness, logic flow | Requirement coverage, correctness of approach, wiring/integration, completeness, edge cases |
| `simplification` | unnecessary complexity, over-engineering | Excessive abstraction, premature generalization, unnecessary indirection, future-proofing excess |
| `testing` | coverage, test quality, edge cases | Missing tests, fake test detection, test independence, edge case coverage |
| `documentation` | README, internal docs, plan alignment | Missing README updates, missing internal docs, plan file status |

### Focused Reviewers (2)

| Name | Scope | Focus |
|------|-------|-------|
| `critical-correctness` | critical and major correctness, security, reliability | Logic errors, security vulnerabilities, data loss risks, race conditions, resource leaks |
| `critical-coverage` | critical and major goal coverage, integration, completeness | Unimplemented requirements, integration bugs, critical logic flow errors |

Broad and focused reviewer names are guaranteed non-overlapping (enforced by test `focused_reviewer_names_do_not_overlap_with_broad_reviewers`).

## Configuration

```
dex review [--parallel N] [--from REF] [--force]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--parallel` | all | Max reviewers to run concurrently |
| `--from` | (from impl_commits.jsonl) | Base git ref for the review diff |
| `--force` | false | Delete all review artifacts and regenerate the plan from scratch |

## Clean Detection

A review file is considered "clean" (zero findings) if it matches the regex `(?i)[-*]\s*(zero|no)\s+(findings|issues)`. Examples that match:

- `- ZERO FINDINGS`
- `- zero findings`
- `* No issues`
- `- No findings`

Source: `src/phases.rs:598`, `src/phases.rs:697`.
