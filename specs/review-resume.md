# Review Phase Resume — Implementation Spec

## Problem

The `review` command always starts from scratch. If a run is interrupted (Ctrl-C, agent crash, timeout), all partial review artifacts (`.dex/review-<name>.md` files) are deleted on the next run because `run_review_fanout` unconditionally calls `remove_dex_file` for every reviewer before running them. This wastes whatever work completed successfully.

## Current Behavior (What Changes)

### Filename convention
- Review files: `.dex/review-<name>.md` (e.g., `review-quality.md`, `review-critical-correctness.md`)
- No round information in the filename — broad and focused reviewers use the same naming scheme.

### Loop logic (`review_phase` in `src/phases.rs:351-407`)
1. Load reviewers from `.dex/reviewers.json` (or built-in defaults).
2. Run broad reviewers via `run_review_fanout(r, ..., &reviewers.broad, "broad", 1, 1, parallel)`.
3. If any broad review has issues → run fixer.
4. Loop `round` from 1 to `MAX_FOCUSED_ROUNDS` (3):
   - Run focused reviewers via `run_review_fanout(r, ..., &reviewers.focused, "focused", round, MAX_FOCUSED_ROUNDS, parallel)`.
   - If all clean → early exit.
   - If issues → run fixer.
5. If max rounds reached → warn and accept.

### `run_review_fanout` behavior (`src/phases.rs:410-521`)
1. **Deletes all review files** for the given reviewer list: `remove_dex_file(&format!("review-{}.md", rv.name))` — this is the core problem.
2. Prepares prompts (renders `review.txt` template with `ReviewName` = `review-<name>.md`).
3. Runs reviewers in parallel batches.
4. Reads back each `review-<name>.md`, checks for "ZERO FINDINGS" / "no issues" pattern.
5. Returns `None` if all clean, `Some(issues)` if any reviewer found issues.

### `reset_dex_runtime_artifacts` in `src/core.rs:261-282`
- Deletes all `review-*.md` files in `.dex/` (called by `plan --force` and `import --force`).
- This is fine — those commands intentionally start fresh.

### Review prompt template (`prompts/review.txt`)
- Uses `{{ReviewName}}` to tell the agent where to write the review.
- Currently `ReviewName` = `review-<name>.md`.

---

## New Design

### 1. Filename Convention Change

**Current:** `.dex/review-<name>.md`
**New:** `.dex/review-<name>-r<N>.md`

| Reviewer type | Round | Filename |
|---|---|---|
| Broad | 1 | `review-quality-r1.md` |
| Broad | 1 | `review-implementation-r1.md` |
| Focused | 1 | `review-critical-correctness-r1.md` |
| Focused | 2 | `review-critical-correctness-r2.md` |
| Focused | 3 | `review-critical-correctness-r3.md` |

Broad reviewers always write round 1. Focused reviewers write round N (the focused round number).

### 2. New Loop Logic (replaces `review_phase`)

```
for each broad reviewer:
    if .dex/review-<name>-r1.md exists → skip (already ran)
    else → run reviewer → writes review-<name>-r1.md

if any broad review-<name>-r1.md has issues → run fixer

for round in 1..=MAX_FOCUSED_ROUNDS:
    for each focused reviewer:
        if .dex/review-<name>-r{round}.md exists → skip
        else → run reviewer → writes review-<name>-r{round}.md

    if any focused review-<name>-r{round}.md has issues → run fixer
    else → all clean, exit early (skip remaining rounds)
```

Key differences from current:
- **No upfront deletion of review files.** Instead, each reviewer checks if its output file already exists and skips if so.
- **Round number is embedded in the filename**, so different rounds never collide.
- **Resume is automatic.** If a previous run completed broad reviews and one focused review before crashing, the next run will skip the broad reviews and the completed focused review, picking up where it left off.

### 3. Detailed Changes by File

#### `src/phases.rs`

##### `review_phase` function (lines 351-407)

Replace the entire function body:

```rust
pub fn review_phase(
    r: &Runner,
    plan_path: &str,
    base_ref: &str,
    parallel: Option<usize>,
    force: bool,
) -> Result<(), String> {
    let reviewers = { /* same loading logic */ };

    if force {
        remove_review_artifacts();
        info("Cleared existing review artifacts (--force).");
    }

    // Broad reviews (always round 1)
    let issues = run_review_fanout(
        r, plan_path, base_ref, &reviewers.broad, "broad", 1, 1, parallel,
    );
    if let Some(ref issues) = issues {
        run_fixer(r, plan_path, base_ref, issues)?;
    }

    // Focused reviews (rounds 1..=MAX_FOCUSED_ROUNDS)
    for round in 1..=MAX_FOCUSED_ROUNDS {
        let issues = run_review_fanout(
            r, plan_path, base_ref, &reviewers.focused, "focused", round, MAX_FOCUSED_ROUNDS, parallel,
        );
        match issues {
            None => {
                info("All focused reviewers report ZERO ISSUES. Review phase complete!");
                return Ok(());
            }
            Some(ref issues) => run_fixer(r, plan_path, base_ref, issues)?,
        }
    }

    warn(&format!(
        "Focused review cap of {} rounds reached, accepting current state.",
        MAX_FOCUSED_ROUNDS
    ));
    Ok(())
}
```

The outer structure stays the same — the resume logic lives inside `run_review_fanout`.

##### `run_review_fanout` function (lines 410-521)

This is where the core change happens:

1. **Remove the upfront deletion loop** (lines 425-427):
   ```rust
   // DELETE THIS:
   for rv in reviewers {
       remove_dex_file(&format!("review-{}.md", rv.name));
   }
   ```

2. **Add a helper function** to build the review filename:
   ```rust
   fn review_filename(name: &str, round: usize) -> String {
       format!("review-{}-r{}.md", name, round)
   }
   ```

3. **Filter out reviewers whose output already exists** before running:
   ```rust
   let pending: Vec<&ReviewRole> = reviewers
       .iter()
       .filter(|rv| !dex_file_exists(&review_filename(&rv.name, round)))
       .collect();
   ```

   Where `dex_file_exists` is a new thin helper in `core.rs` (see below).

4. **Log skipped reviewers** for visibility:
   ```rust
   for rv in &reviewers {
       if !pending.contains(&rv) {
           info(&format!("[{}] review already exists, skipping", rv.name));
       }
   }
   ```

5. **Prepare prompts only for pending reviewers**, using the new filename:
   ```rust
   let prepared: Vec<PreparedReview> = pending
       .iter()
       .map(|rv| PreparedReview {
           prompt: render_prompt(
               "review.txt",
               &serde_json::json!({
                   "PlanPath": plan_path,
                   "BaseRef": base_ref,
                   "RoleName": rv.name,
                   "RoleScope": rv.scope,
                   "RolePrompt": rv.prompt,
                   "ReviewName": review_filename(&rv.name, round),
               }),
           ),
           role_name: rv.name.clone(),
           role_scope: rv.scope.clone(),
       })
       .collect();
   ```

6. **Run pending reviewers** (same parallel batch logic, but only for `pending`).

7. **Read back ALL reviewers' files** (not just pending) when collecting issues — this is critical for resume correctness. If a reviewer was skipped because its file already existed, we still need to read that file to determine if it had issues:
   ```rust
   let mut all_clean = true;
   let mut issues = Vec::new();
   let clean_review_re = regex::Regex::new(r"(?i)[-*]\s*(zero|no)\s+(findings|issues)").unwrap();

   for rv in reviewers {
       let filename = review_filename(&rv.name, round);
       let review = read_dex_file(&filename);
       match review {
           None => {
               // Reviewer produced no output (was pending but failed, or file is empty)
               warn(&format!("Reviewer {:?} produced no output", rv.name));
               all_clean = false;
           }
           Some(review) => {
               if clean_review_re.is_match(&review) {
                   info(&format!("[{}] ZERO ISSUES", rv.name));
               } else {
                   err_msg(&format!("[{}] issues found", rv.name));
                   show_markdown(&format!("Review: {}", rv.name), &review);
                   all_clean = false;
                   issues.push(format!(
                       "\u{2500}\u{2500} {} \u{2500}\u{2500}\n{}",
                       rv.name, review
                   ));
               }
           }
       }
   }
   ```

   Note: we iterate over the **original** `reviewers` list (not `pending`) so that previously-completed reviews are included in the issue collection.

8. Return `None` if all clean, `Some(issues)` otherwise (same as current).

##### Summary of `run_review_fanout` changes

| Aspect | Before | After |
|---|---|---|
| Pre-run | Delete all `review-<name>.md` | Check which `review-<name>-r<N>.md` exist; skip those reviewers |
| Prompt `ReviewName` | `review-<name>.md` | `review-<name>-r<N>.md` |
| Who gets run | All reviewers | Only reviewers without an existing output file |
| Issue collection | Read all reviewers' files | Read all reviewers' files (same — includes pre-existing ones) |
| Skipped reviewer logging | None | `info("[<name>] review already exists, skipping")` |

#### `src/core.rs`

##### Add `dex_file_exists` helper

```rust
pub fn dex_file_exists(name: &str) -> bool {
    PathBuf::from(DEX_DIR).join(name).is_file()
}
```

This is a simple, safe check. Used by `run_review_fanout` to decide whether to skip a reviewer.

##### Add `remove_review_artifacts` helper

Extract the review-file deletion logic from `reset_dex_runtime_artifacts` into a standalone function:

```rust
pub fn remove_review_artifacts() {
    let entries = match fs::read_dir(DEX_DIR) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.is_file() && name.starts_with("review-") && name.ends_with(".md") {
            fs::remove_file(path).ok();
        }
    }
}
```

Called by `review_phase` when `force` is true, and also by `reset_dex_runtime_artifacts` to avoid duplicating the same glob logic.

##### Update `reset_dex_runtime_artifacts` (lines 261-282)

Replace the inline review-file deletion loop with a call to the new helper:

```rust
// Before (inline):
let entries = match fs::read_dir(DEX_DIR) { ... };
for entry in entries.flatten() { ... if starts_with("review-") ... }

// After (delegated):
remove_review_artifacts();
```

The rest of `reset_dex_runtime_artifacts` (deleting plan.md, request.txt, etc.) stays the same.

#### `prompts/review.txt`

No changes needed. The template already uses `{{ReviewName}}` which will now receive `review-<name>-r<N>.md` instead of `review-<name>.md`. The prompt text itself doesn't need to change — it just tells the agent where to write.

#### `prompts/fix.txt`

No changes needed. The fixer doesn't reference review filenames directly — it receives the issues as text via the `{{Issues}}` template variable.

#### `src/main.rs`

##### Add `--force` flag to `ReviewCmd`

```rust
struct ReviewCmd {
    /// max reviewers to run in parallel (default: all)
    #[argh(option)]
    parallel: Option<usize>,

    /// base ref for the review diff (used when no impl_commits.jsonl exists)
    #[argh(option)]
    from: Option<String>,

    /// remove existing review artifacts and start from scratch
    #[argh(switch)]
    force: bool,
}
```

##### Pass `force` through to `review_phase`

In the `SubCommand::Review(cmd)` handler, change:

```rust
review_phase(&runner, &plan_path, &base_ref, cmd.parallel)?;
```

to:

```rust
review_phase(&runner, &plan_path, &base_ref, cmd.parallel, cmd.force)?;
```

##### Add example to `ReviewCmd` doc comment

```rust
#[argh(
    subcommand,
    name = "review",
    example = "Review the current implementation:\n  {command_name} --parallel 2",
    example = "Review against a specific base:\n  {command_name} --from main",
    example = "Force a fresh review from scratch:\n  {command_name} --force"
)]
```

### 4. Edge Cases and Robustness

#### Partially-written review file (agent crashed mid-write)

If an agent crashes while writing `review-quality-r1.md`, the file may exist but be incomplete or empty. The current `read_dex_file` already handles this — it returns `None` for empty/whitespace-only files. So:

- **File exists but empty/whitespace:** `dex_file_exists` returns `true` (file is present), so the reviewer is skipped. But `read_dex_file` returns `None` when collecting issues, so the reviewer is treated as "produced no output" → `all_clean = false` → issues list doesn't include it, but the phase doesn't report all-clean either.

This is a problem. We need to handle this case:

**Solution:** Change the skip condition to check both file existence AND non-empty content:

```rust
fn review_already_completed(name: &str, round: usize) -> bool {
    let filename = review_filename(name, round);
    dex_file_exists(&filename) && read_dex_file(&filename).is_some()
}
```

This way:
- File doesn't exist → run reviewer
- File exists but empty/corrupt → run reviewer again (overwrite)
- File exists with content → skip

Since the agent writes the file atomically (writes to the path at the end), a crash mid-write typically leaves an empty or partial file, which `read_dex_file` will treat as `None` (if empty) or as valid content (if partial but non-empty). For the partial-but-non-empty case, we accept it as a completed review — it's better to have a partial review than to re-run the entire reviewer. The fixer will see whatever issues were reported.

#### Stale review files from a previous `dex apply` + `dex review` cycle

If the user runs `dex apply` (which creates new commits), then `dex review` (which creates review files), then `dex apply` again (more commits), then `dex review` again — the old review files are now stale because they reviewed different code.

**Solution:** Use `dex review --force` to delete all existing review artifacts and start from scratch. This is the explicit escape hatch for stale state. Between `apply` and `review` cycles, the user would typically re-plan or the review files would be from the same codebase state.

If we want to be extra safe, we could embed the HEAD commit hash in the review filename (e.g., `review-quality-r1-abc1234.md`), but this adds complexity and the current workflow doesn't require it. **Deferring this to a future enhancement.**

#### Parallel reviewer crash leaves some files written, some not

If 3 of 5 broad reviewers complete before a crash, the next run will skip those 3 and run the remaining 2. This is exactly the desired behavior.

#### Fixer ran but reviewer files still exist

After the fixer runs, the focused reviewers in the next round will review the updated code. The previous round's review files remain (e.g., `review-critical-correctness-r1.md` stays even after the fixer addresses its issues). This is correct — round 2 reviewers write to `review-critical-correctness-r2.md`, so there's no collision.

### 5. Migration / Backward Compatibility

Old review files (`review-<name>.md`) from previous dex versions will not interfere with the new naming scheme (`review-<name>-r<N>.md`). They'll just sit in `.dex/` harmlessly until `reset_dex_runtime_artifacts` cleans them up (they match the `review-*.md` glob).

No migration step needed. Old files are inert.

### 6. Testing Plan

#### Unit tests to add in `src/phases.rs`

1. **`review_filename_formats_correctly`** — verify `review_filename("quality", 1)` returns `"review-quality-r1.md"` and `review_filename("critical-correctness", 3)` returns `"review-critical-correctness-r3.md"`.

2. **`review_already_completed_detects_existing_file`** — create a temp `.dex/review-quality-r1.md` with content, verify `review_already_completed("quality", 1)` returns `true`.

3. **`review_already_completed_ignores_empty_file`** — create a temp `.dex/review-quality-r1.md` that's empty/whitespace, verify `review_already_completed("quality", 1)` returns `false`.

4. **`review_already_completed_returns_false_for_missing_file`** — verify `review_already_completed("quality", 1)` returns `false` when no file exists.

#### Integration-level verification

1. **Resume test:** run `dex review`, Ctrl-C after some reviewers complete, run `dex review` again, verify:
   - Skipped reviewers log `[<name>] review already exists, skipping`
   - Only pending reviewers are run
   - Review phase completes correctly

2. **Force test:** run `dex review` (let it complete partially), then run `dex review --force`, verify:
   - All `review-*.md` files are deleted before reviewers start
   - All reviewers run from scratch
   - No "already exists, skipping" messages

### 7. Summary of All Code Changes

| File | Change | Lines Affected |
|---|---|---|
| `src/main.rs` | Add `force: bool` to `ReviewCmd` with `#[argh(switch)]` | 2 lines added |
| `src/main.rs` | Pass `cmd.force` to `review_phase` | 1 line changed |
| `src/main.rs` | Add `--force` example to `ReviewCmd` doc | 1 line added |
| `src/phases.rs` | Add `review_filename()` helper | New function (~3 lines) |
| `src/phases.rs` | Add `review_already_completed()` helper | New function (~4 lines) |
| `src/phases.rs` | `review_phase`: add `force: bool` param, call `remove_review_artifacts()` when true | ~4 lines added |
| `src/phases.rs` | `run_review_fanout`: remove upfront deletion loop | Remove lines 425-427 |
| `src/phases.rs` | `run_review_fanout`: filter to pending reviewers, log skips | ~10 lines added |
| `src/phases.rs` | `run_review_fanout`: use `review_filename()` for `ReviewName` in prompt | 1 line changed |
| `src/phases.rs` | `run_review_fanout`: iterate original `reviewers` list for issue collection (not `pending`) | Already iterates `reviewers` — no change needed |
| `src/core.rs` | Add `dex_file_exists()` helper | New function (~3 lines) |
| `src/core.rs` | Add `remove_review_artifacts()` helper | New function (~10 lines) |
| `src/core.rs` | `reset_dex_runtime_artifacts`: delegate review-file deletion to `remove_review_artifacts()` | Replace inline loop with 1 call |
| `prompts/review.txt` | No change | — |
| `prompts/fix.txt` | No change | — |
