use serde::Deserialize;
use similar::TextDiff;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::core::{
    append_impl_commits, dex_path, ensure_dex_dir, git_commits_between, git_head,
    git_trimmed_output, impl_commit_history_summary, read_dex_file, remove_dex_file, render_prompt,
    save_feedbacks, save_plan_request,
};
use crate::plan::{first_open_checkbox, next_open_task, plan_step_counts};
use crate::runner::Runner;
use crate::ui::{
    banner, err_msg, info, phase_detail, prompt_choice, prompt_multiline, show_markdown, warn,
};

const IMPLEMENTATION_STALEMATE_LIMIT: usize = 4;

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

// ── Phase 1: Planning ──

pub fn resume_plan(r: &Runner) -> Result<Option<String>, String> {
    let plan_path = dex_path("plan.md");
    let Some(plan) = read_dex_file("plan.md") else {
        return Ok(None);
    };

    banner("EXISTING PLAN");
    show_markdown("Plan", &plan);

    let request = read_dex_file("request.txt").unwrap_or_default();
    let mut feedbacks = crate::core::load_feedbacks();

    match plan_review_loop(&plan, &mut feedbacks)? {
        PlanReviewResult::Accepted => Ok(Some(plan_path)),
        PlanReviewResult::Rejected => Ok(None),
        PlanReviewResult::Loop => run_planning_loop(r, request, feedbacks, plan_path),
    }
}

pub fn plan_phase(
    r: &Runner,
    request: &str,
    feedbacks: Vec<String>,
) -> Result<Option<String>, String> {
    banner("PLANNING");
    ensure_dex_dir();

    let plan_path = dex_path("plan.md");
    save_plan_request(request);
    save_feedbacks(&feedbacks);

    run_planning_loop(r, request.to_string(), feedbacks, plan_path)
}

enum PlanReviewResult {
    Accepted,
    Rejected,
    Loop,
}

fn plan_review_loop(plan: &str, feedbacks: &mut Vec<String>) -> Result<PlanReviewResult, String> {
    loop {
        let choice = prompt_choice(
            "Accept, edit, revise, or reject?",
            &["accept", "edit", "revise", "reject"],
        );
        match choice.as_str() {
            "accept" => {
                info("Plan accepted!");
                return Ok(PlanReviewResult::Accepted);
            }
            "reject" => {
                warn("Plan rejected.");
                return Ok(PlanReviewResult::Rejected);
            }
            "edit" => match edit_plan_in_editor(plan) {
                Ok(Some(diff)) => {
                    let feedback = format!(
                        "user provided feedback in the form of a unified diff: \n\n{}",
                        diff
                    );
                    feedbacks.push(feedback);
                    save_feedbacks(feedbacks);
                    return Ok(PlanReviewResult::Loop);
                }
                Ok(None) => {
                    info("No changes detected in the plan.");
                }
                Err(e) => {
                    err_msg(&format!("Editor error: {}", e));
                }
            },
            "revise" => {
                let feedback = prompt_multiline("Your revision feedback:");
                feedbacks.push(feedback);
                save_feedbacks(feedbacks);
                return Ok(PlanReviewResult::Loop);
            }
            _ => {}
        }
    }
}

fn run_planning_loop(
    r: &Runner,
    request: String,
    mut feedbacks: Vec<String>,
    plan_path: String,
) -> Result<Option<String>, String> {
    let mut iteration = 1;
    loop {
        let iter_start = Instant::now();
        phase_detail("iteration", &iteration.to_string());
        iteration += 1;

        remove_dex_file("questions.md");

        let p = render_prompt(
            "plan.txt",
            &serde_json::json!({
                "Request": request,
                "Feedbacks": feedbacks,
            }),
        );

        if let Err(e) = r.run(&p) {
            phase_detail("elapsed", &format_duration(iter_start.elapsed()));
            err_msg(&format!("CLI error: {}", e));
            return Err(format!("planning failed after automatic retries: {}", e));
        }

        phase_detail("elapsed", &format_duration(iter_start.elapsed()));

        if let Some(questions) = read_dex_file("questions.md") {
            show_markdown("Questions from CLI", &questions);
            let answer = prompt_multiline("Your answers:");
            feedbacks.push(format!("Questions:\n{}\n\nAnswers:\n{}", questions, answer));
            save_feedbacks(&feedbacks);
            continue;
        }

        let plan = match read_dex_file("plan.md") {
            Some(p) => p,
            None => {
                warn("CLI did not produce a plan or questions. Retrying...");
                feedbacks.push(format!(
                    "You did not produce a plan in {} or questions in {}. Please do so.",
                    dex_path("plan.md"),
                    dex_path("questions.md")
                ));
                save_feedbacks(&feedbacks);
                continue;
            }
        };

        show_markdown("Plan", &plan);

        match plan_review_loop(&plan, &mut feedbacks)? {
            PlanReviewResult::Accepted => return Ok(Some(plan_path)),
            PlanReviewResult::Rejected => return Ok(None),
            PlanReviewResult::Loop => continue,
        }
    }
}

fn edit_plan_in_editor(plan: &str) -> Result<Option<String>, String> {
    let tmp_path = std::env::temp_dir().join(format!(
        "dex-plan-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::write(&tmp_path, plan).map_err(|e| format!("write temp file: {}", e))?;

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let cmd = parts.next().unwrap_or("vi");
    let editor_args: Vec<&str> = parts.collect();
    let status = Command::new(cmd)
        .args(&editor_args)
        .arg(&tmp_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("editor {:?}: {}", editor, e))?;

    if !status.success() {
        return Err(format!("editor {:?} exited with {}", editor, status));
    }

    let edited = fs::read_to_string(&tmp_path).map_err(|e| format!("read temp file: {}", e))?;
    let _ = fs::remove_file(&tmp_path);

    if edited == plan {
        warn("No changes detected.");
        return Ok(None);
    }

    let text_diff = TextDiff::from_lines(plan, &edited);
    let unified = format!(
        "{}",
        text_diff
            .unified_diff()
            .context_radius(5)
            .header("original", "edited")
    );

    Ok(Some(unified))
}

// ── Phase 2: Implementation ──

pub fn impl_phase(r: &Runner, plan_path: &str) -> Result<(), String> {
    banner("IMPLEMENTATION");

    let mut iteration = 1;
    let mut unchanged_iterations = 0;
    let mut stall_note: Option<String> = None;
    loop {
        let iter_start = Instant::now();

        let Some(task) = next_open_task(plan_path)? else {
            info("All tasks complete!");
            return Ok(());
        };

        let (plan_steps_open, plan_steps_total) = plan_step_counts(plan_path)?;
        let header = task.header.trim().trim_start_matches('#').trim();
        let header = if header.is_empty() {
            "(unnamed task)"
        } else {
            header
        };
        let iteration_detail = format!(
            "{} of {} plan steps remaining",
            plan_steps_open, plan_steps_total
        );
        phase_detail(&format!("Iteration {}", iteration), &iteration_detail);
        phase_detail("Job", header);
        if let Some(ref note) = stall_note {
            warn(&format!("Prior iteration had no progress: {}", note));
        }

        let commit_history = impl_commit_history_summary().unwrap_or_default();
        let state_a = git_head().ok();

        let p = render_prompt(
            "impl.txt",
            &serde_json::json!({
                "PlanPath": plan_path,
                "TaskHeader": task.header,
                "TaskBody": task.body(),
                "CommitHistory": commit_history,
                "StallNote": stall_note,
            }),
        );

        if let Err(e) = r.run(&p) {
            let iter_elapsed = iter_start.elapsed();
            phase_detail("elapsed", &format_duration(iter_elapsed));
            err_msg(&format!("CLI error: {}", e));
            return Err(format!(
                "implementation failed after automatic retries: {}",
                e
            ));
        }

        let iter_elapsed = iter_start.elapsed();
        phase_detail("elapsed", &format_duration(iter_elapsed));

        let mut made_commits = false;
        if let Some(ref before) = state_a {
            if let Ok(after) = git_head() {
                let new_commits = git_commits_between(before, &after);
                made_commits = !new_commits.is_empty();
                append_impl_commits(&new_commits);
            }
        }

        let after_counts = plan_step_counts(plan_path)?;
        if after_counts.0 == 0 {
            info("All tasks complete!");
            return Ok(());
        }

        let made_plan_progress = (plan_steps_open, plan_steps_total) != after_counts;
        stall_note = match (made_commits, made_plan_progress) {
            (false, false) => Some(
                "the previous iteration produced no git commits and ticked no checkboxes. \
You must either (a) finish the current task and commit, or (b) if it is genuinely blocked, \
mark the offending checkbox `- [x] <description> (skipped — blocked: <reason>)` so the loop can \
move on. Do NOT silently re-attempt the same approach without changes."
                    .to_string(),
            ),
            (true, false) => Some(
                "the previous iteration committed code but did not tick any checkboxes. \
After completing work, edit the plan file to mark the corresponding `- [ ]` items as `- [x]` \
in the SAME iteration. If a checkbox cannot be completed automatically, mark it skipped per \
the plan's non-automatable rule rather than leaving it open."
                    .to_string(),
            ),
            (false, true) => Some(
                "the previous iteration made plan progress without committing. \
Always commit the implementation changes (excluding plan file edits is OK) so review and \
finalize phases can see them."
                    .to_string(),
            ),
            (true, true) => None,
        };

        unchanged_iterations = if (plan_steps_open, plan_steps_total) == after_counts {
            unchanged_iterations + 1
        } else {
            0
        };
        if unchanged_iterations >= IMPLEMENTATION_STALEMATE_LIMIT {
            return Err(format!(
                "STALEMATE: total plan steps ({}) and remaining plan steps ({}) were unchanged for {} consecutive implementation iterations.",
                after_counts.1, after_counts.0, IMPLEMENTATION_STALEMATE_LIMIT
            ));
        }

        iteration += 1;
    }
}

// ── Phase 3: Review ──

#[derive(Deserialize)]
struct ReviewRole {
    name: String,
    scope: String,
    prompt: String,
}

#[derive(Deserialize)]
pub struct Reviewers {
    broad: Vec<ReviewRole>,
    focused: Vec<ReviewRole>,
}

impl Reviewers {
    pub fn builtin() -> Self {
        serde_json::from_str(include_str!("../prompts/reviewers.json"))
            .expect("invalid built-in reviewers.json")
    }
}

const MAX_FOCUSED_ROUNDS: usize = 3;

fn generate_review_plan(base_ref: &str, reviewers: &Reviewers) -> String {
    let mut lines = vec![format!("<!-- base_ref: {} -->", base_ref)];

    lines.push(String::new());
    lines.push("## Broad Review".to_string());
    for rv in &reviewers.broad {
        lines.push(format!("- [ ] {}", rv.name));
    }

    lines.push(String::new());
    lines.push("## Broad Fixer".to_string());
    lines.push("- [ ] fix broad review findings".to_string());

    for round in 1..=MAX_FOCUSED_ROUNDS {
        lines.push(String::new());
        lines.push(format!(
            "## Focused Review — Round {}/{}",
            round, MAX_FOCUSED_ROUNDS
        ));
        for rv in &reviewers.focused {
            lines.push(format!("- [ ] {}", rv.name));
        }

        lines.push(String::new());
        lines.push(format!("## Focused Fixer — Round {}", round));
        lines.push(format!("- [ ] fix focused round {} findings", round));
    }

    lines.join("\n")
}

fn read_review_plan_base_ref() -> Option<String> {
    let content = read_dex_file("review-plan.md")?;
    let first_line = content.lines().next()?;
    let re = regex::Regex::new(r"^<!-- base_ref: (.+?) -->$").unwrap();
    re.captures(first_line).map(|caps| caps[1].to_string())
}

fn mark_review_step_done(heading: &str, name: &str) {
    let Some(content) = read_dex_file("review-plan.md") else {
        return;
    };
    let heading_re = regex::Regex::new(r"^#{1,6}\s+.*$").unwrap();
    let checkbox_re = regex::Regex::new(&format!(
        r"^(- \[) \](\s+{})$",
        regex::escape(name)
    )).unwrap();

    let mut in_target_heading = false;
    let updated: Vec<String> = content
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if heading_re.is_match(trimmed) {
                in_target_heading = trimmed.starts_with(heading);
                return line.to_string();
            }
            if in_target_heading {
                if let Some(caps) = checkbox_re.captures(line) {
                    return format!("- [x]{}", &caps[2]);
                }
            }
            line.to_string()
        })
        .collect();
    let _ = fs::write(dex_path("review-plan.md"), updated.join("\n"));
}

fn mark_remaining_skipped(note: &str) {
    let Some(content) = read_dex_file("review-plan.md") else {
        return;
    };
    let re = regex::Regex::new(r"^(- \[) \](\s+.+)$").unwrap();
    let updated: Vec<String> = content
        .lines()
        .map(|line| {
            if let Some(caps) = re.captures(line) {
                format!("- [x]{} ({})", &caps[2], note)
            } else {
                line.to_string()
            }
        })
        .collect();
    let _ = fs::write(dex_path("review-plan.md"), updated.join("\n"));
}

fn plans_have_same_structure(a: &str, b: &str) -> bool {
    let checkbox_re = regex::Regex::new(r"^-\s+\[[ xX]\]\s+(.+)$").unwrap();
    let heading_re = regex::Regex::new(r"^#{1,6}\s+.*$").unwrap();

    fn extract_structure(content: &str, checkbox_re: &regex::Regex, heading_re: &regex::Regex) -> Vec<(String, Vec<String>)> {
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        let mut cur_heading = String::new();
        let mut cur_names: Vec<String> = Vec::new();

        for line in content.lines() {
            if heading_re.is_match(line.trim()) {
                if !cur_names.is_empty() || !cur_heading.is_empty() {
                    sections.push((cur_heading.clone(), cur_names.clone()));
                }
                cur_heading = line.trim().to_string();
                cur_names.clear();
                continue;
            }
            if let Some(caps) = checkbox_re.captures(line) {
                cur_names.push(caps[1].trim().to_string());
            }
        }
        if !cur_names.is_empty() || !cur_heading.is_empty() {
            sections.push((cur_heading, cur_names));
        }
        sections
    }

    extract_structure(a, &checkbox_re, &heading_re) == extract_structure(b, &checkbox_re, &heading_re)
}

struct PreparedReview {
    prompt: String,
    role_name: String,
    role_scope: String,
}

pub fn review_phase(
    r: &Runner,
    plan_path: &str,
    base_ref: &str,
    parallel: Option<usize>,
    force: bool,
) -> Result<(), String> {
    let reviewers = {
        let path = dex_path("reviewers.json");
        match fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|_| {
                warn("Invalid .dex/reviewers.json, falling back to defaults.");
                Reviewers::builtin()
            }),
            Err(_) => Reviewers::builtin(),
        }
    };

    let review_plan_path = dex_path("review-plan.md");

    if force || !std::path::PathBuf::from(&review_plan_path).exists() {
        for entry in fs::read_dir(crate::core::DEX_DIR).unwrap_or_else(|_| {
            fs::create_dir_all(crate::core::DEX_DIR).ok();
            fs::read_dir(crate::core::DEX_DIR).unwrap_or_else(|_| panic!("cannot create .dex dir"))
        }).flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("review-") && name.ends_with(".md") {
                remove_dex_file(&name);
            }
        }

        info(&format!("Generating review-plan.md (base_ref={})...", base_ref));
        let plan_content = generate_review_plan(base_ref, &reviewers);
        ensure_dex_dir();
        fs::write(&review_plan_path, &plan_content)
            .map_err(|e| format!("write review-plan.md: {}", e))?;
        info(&format!("Wrote review-plan.md to {}", review_plan_path));
    } else {
        info("review-plan.md already exists, resuming from plan");
        let existing_plan = read_dex_file("review-plan.md").unwrap_or_default();
        let expected = generate_review_plan(
            read_review_plan_base_ref().as_deref().unwrap_or(base_ref),
            &reviewers,
        );
        if !plans_have_same_structure(&existing_plan, &expected) {
            warn("reviewers.json changed since review-plan.md was generated. Use --force to regenerate.");
        }
    }

    let effective_base_ref = read_review_plan_base_ref()
        .unwrap_or_else(|| base_ref.to_string());

    loop {
        let Some((task, checkbox_name)) = first_open_checkbox(&review_plan_path)? else {
            info("All review steps complete!");
            return Ok(());
        };

        let header = task.header.trim().trim_start_matches('#').trim();

        let step_start = Instant::now();

        if header.starts_with("Broad Review") {
            let rv = reviewers.broad.iter().find(|rv| rv.name == checkbox_name);
            if let Some(rv) = rv {
                run_review_fanout(r, plan_path, &effective_base_ref, std::slice::from_ref(rv), "broad", 1, 1, parallel);
            }
            mark_review_step_done("## Broad Review", &checkbox_name);
            phase_detail("elapsed", &format_duration(step_start.elapsed()));
        } else if header.starts_with("Broad Fixer") {
            let issues = collect_issues_from_reviewers(&reviewers.broad);
            match issues {
                None => {
                    mark_review_step_done("## Broad Fixer", "fix broad review findings");
                }
                Some(ref issues) => {
                    run_fixer(r, plan_path, &effective_base_ref, issues)?;
                    mark_review_step_done("## Broad Fixer", "fix broad review findings");
                }
            }
            phase_detail("elapsed", &format_duration(step_start.elapsed()));
        } else if header.starts_with("Focused Review") {
            let rv = reviewers.focused.iter().find(|rv| rv.name == checkbox_name);
            let round = extract_round_from_heading(header);
            if let Some(rv) = rv {
                let issues = run_review_fanout(
                    r,
                    plan_path,
                    &effective_base_ref,
                    std::slice::from_ref(rv),
                    "focused",
                    round,
                    MAX_FOCUSED_ROUNDS,
                    parallel,
                );
                mark_review_step_done(&task.header, &checkbox_name);
                phase_detail("elapsed", &format_duration(step_start.elapsed()));
                if issues.is_none() {
                    let all_in_round_done = {
                        let updated_task = first_open_checkbox(&review_plan_path).ok().flatten();
                        updated_task.is_none_or(|(t, _)| {
                            let h = t.header.trim().trim_start_matches('#').trim();
                            !h.starts_with(&format!("Focused Review — Round {}/", round))
                        })
                    };
                    if all_in_round_done {
                        mark_remaining_skipped("skipped — prior round clean");
                        info("All focused reviewers report ZERO ISSUES. Review phase complete!");
                        return Ok(());
                    }
                }
            }
        } else if header.starts_with("Focused Fixer") {
            let round = extract_round_from_heading(header);
            let issues = collect_issues_from_reviewers(&reviewers.focused);
            match issues {
                None => {
                    mark_review_step_done(&task.header, &format!("fix focused round {} findings", round));
                }
                Some(ref issues) => {
                    run_fixer(r, plan_path, &effective_base_ref, issues)?;
                    mark_review_step_done(&task.header, &format!("fix focused round {} findings", round));
                }
            }
            phase_detail("elapsed", &format_duration(step_start.elapsed()));
        }
    }
}

fn extract_round_from_heading(header: &str) -> usize {
    let re = regex::Regex::new(r"Round\s+(\d+)").unwrap();
    re.captures(header)
        .and_then(|caps| caps[1].parse::<usize>().ok())
        .unwrap_or(1)
}

fn collect_issues_from_reviewers(reviewers: &[ReviewRole]) -> Option<Vec<String>> {
    let mut all_clean = true;
    let mut issues = Vec::new();
    let clean_review_re = regex::Regex::new(r"(?i)[-*]\s*(zero|no)\s+(findings|issues)").unwrap();

    for rv in reviewers {
        let review = read_dex_file(&format!("review-{}.md", rv.name));
        match review {
            None => {
                all_clean = false;
            }
            Some(review) => {
                if clean_review_re.is_match(&review) {
                } else {
                    all_clean = false;
                    issues.push(format!("\u{2500}\u{2500} {} \u{2500}\u{2500}\n{}", rv.name, review));
                }
            }
        }
    }

    if all_clean { None } else { Some(issues) }
}

#[allow(clippy::too_many_arguments)]
fn run_review_fanout(
    r: &Runner,
    plan_path: &str,
    base_ref: &str,
    reviewers: &[ReviewRole],
    label: &str,
    round: usize,
    max_rounds: usize,
    parallel: Option<usize>,
) -> Option<Vec<String>> {
    banner(&format!(
        "{}-review | round {}/{}",
        label, round, max_rounds
    ));

    let prepared: Vec<PreparedReview> = reviewers
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
                    "ReviewName": format!("review-{}.md", rv.name),
                }),
            ),
            role_name: rv.name.clone(),
            role_scope: rv.scope.clone(),
        })
        .collect();

    let max_concurrent = parallel.unwrap_or(reviewers.len()).max(1);
    for batch in prepared.chunks(max_concurrent) {
        let batch_start = Instant::now();
        let handles: Vec<_> = batch
            .iter()
            .map(|review| {
                info(&format!(
                    "[parallel:{}] running {} review",
                    review.role_name, review.role_scope
                ));

                let runner = r.labeled(&review.role_name);
                let prompt = review.prompt.clone();
                let role_name = review.role_name.clone();
                let role_scope = review.role_scope.clone();
                std::thread::spawn(move || {
                    let result = runner.run(&prompt);
                    match &result {
                        Ok(()) => info(&format!(
                            "[parallel:{}] done {} review (exit=0)",
                            role_name, role_scope
                        )),
                        Err(_) => err_msg(&format!(
                            "[parallel:{}] done {} review (exit=1)",
                            role_name, role_scope
                        )),
                    }
                    result
                })
            })
            .collect();

        for (review, handle) in batch.iter().zip(handles) {
            if handle.join().is_err() {
                err_msg(&format!(
                    "[parallel:{}] review thread panicked",
                    review.role_name
                ));
            }
        }
        phase_detail("elapsed", &format_duration(batch_start.elapsed()));
    }

    let mut all_clean = true;
    let mut issues = Vec::new();
    let clean_review_re = regex::Regex::new(r"(?i)[-*]\s*(zero|no)\s+(findings|issues)").unwrap();

    for rv in reviewers {
        let review = read_dex_file(&format!("review-{}.md", rv.name));
        match review {
            None => {
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

    if all_clean {
        None
    } else {
        Some(issues)
    }
}

fn run_fixer(r: &Runner, plan_path: &str, base_ref: &str, issues: &[String]) -> Result<(), String> {
    warn("Running fixer...");
    let fix_prompt = render_prompt(
        "fix.txt",
        &serde_json::json!({
            "PlanPath": plan_path,
            "BaseRef": base_ref,
            "Issues": issues.join("\n\n"),
        }),
    );
    let fixer_start = Instant::now();
    if let Err(e) = r.run(&fix_prompt) {
        phase_detail("elapsed", &format_duration(fixer_start.elapsed()));
        err_msg(&format!("Fixer error: {}", e));
        return Err(format!("fixer failed after automatic retries: {}", e));
    }
    phase_detail("elapsed", &format_duration(fixer_start.elapsed()));
    Ok(())
}

// ── Bare Mode ──

pub fn bare_phase(r: &Runner, request_file: &str, max_iterations: usize) -> Result<(), String> {
    banner("BARE");
    for iteration in 1..=max_iterations {
        let iter_start = Instant::now();
        phase_detail("iteration", &format!("{}/{}", iteration, max_iterations));
        let request = match fs::read_to_string(request_file) {
            Ok(request) => {
                let request = request.trim().to_string();
                if request.is_empty() {
                    info(&format!(
                        "Bare loop stopped: request file {:?} is empty after trimming.",
                        request_file
                    ));
                    return Ok(());
                }
                request
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info(&format!(
                    "Bare loop stopped: request file {:?} is missing.",
                    request_file
                ));
                return Ok(());
            }
            Err(e) => return Err(format!("read bare request {:?}: {}", request_file, e)),
        };

        let p = render_prompt("bare.txt", &serde_json::json!({"Request": request}));
        if let Err(e) = r.run(&p) {
            phase_detail("elapsed", &format_duration(iter_start.elapsed()));
            return Err(format!("bare iteration {} failed: {}", iteration, e));
        }
        phase_detail("elapsed", &format_duration(iter_start.elapsed()));
    }
    Ok(())
}

// ── Finalize Phase ──

pub fn finalize_phase(r: &Runner, plan_path: &str, finalize_target: &str) -> Result<(), String> {
    banner("FINALIZE");

    let branch = git_trimmed_output(&["symbolic-ref", "--short", "HEAD"])?;
    if branch.is_empty() {
        return Err(
            "finalize requires a named branch (detached HEAD is not supported)".to_string(),
        );
    }

    let finalize_target = finalize_target.trim();
    if finalize_target.is_empty() {
        return Err(
            "finalize requires a rebase target: dex --finalize <target-for-rebase>".to_string(),
        );
    }

    git_trimmed_output(&["rev-parse", "--verify", finalize_target])?;

    let range = format!("{}..HEAD", finalize_target);
    let count = git_trimmed_output(&["rev-list", "--count", &range])?;
    let commits_ahead = count
        .parse::<u64>()
        .map_err(|e| format!("parse git rev-list count {:?}: {}", count, e))?;
    if commits_ahead == 0 {
        return Err(format!(
            "finalize: branch {:?} has no commits to finalize relative to {:?}; run this on your feature branch or choose a different target",
            branch, finalize_target
        ));
    }

    let p = render_prompt(
        "finalize.txt",
        &serde_json::json!({
            "PlanPath": plan_path,
            "FinalizeTarget": finalize_target,
            "FinalizeNeedsFetch": finalize_target.starts_with("origin/"),
        }),
    );

    let start = Instant::now();
    if let Err(e) = r.run(&p) {
        phase_detail("elapsed", &format_duration(start.elapsed()));
        err_msg(&format!("Finalize error: {}", e));
        return Err(format!("finalize failed after automatic retries: {}", e));
    }
    phase_detail("elapsed", &format_duration(start.elapsed()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{generate_review_plan, mark_review_step_done, mark_remaining_skipped, plans_have_same_structure, read_review_plan_base_ref, extract_round_from_heading, Reviewers, format_duration};
    use crate::core::{dex_path, ensure_dex_dir};
    use regex::Regex;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    enum BareRequestState {
        Ready(String),
        Missing,
        Empty,
    }

    fn write_temp_file(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dex-phases-test-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::write(&path, contents).unwrap();
        path
    }

    fn clean_review_re() -> Regex {
        Regex::new(r"(?i)[-*]\s*(zero|no)\s+(findings|issues)").unwrap()
    }

    fn bare_request_state(path: &str) -> Result<BareRequestState, String> {
        let request = match fs::read_to_string(path) {
            Ok(request) => request,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BareRequestState::Missing);
            }
            Err(e) => return Err(format!("read bare request {:?}: {}", path, e)),
        };

        let request = request.trim().to_string();
        if request.is_empty() {
            Ok(BareRequestState::Empty)
        } else {
            Ok(BareRequestState::Ready(request))
        }
    }

    fn format_header_for_display(header: &str) -> String {
        let header = header.trim();
        if header.is_empty() {
            "(unnamed task)".to_string()
        } else {
            let label = header.trim_start_matches('#').trim();
            if label.is_empty() {
                "(unnamed task)".to_string()
            } else {
                label.to_string()
            }
        }
    }

    fn setup_dex_dir() {
        ensure_dex_dir();
        let _ = fs::remove_file(dex_path("review-plan.md"));
    }

    fn teardown_dex_dir() {
        let _ = fs::remove_file(dex_path("review-plan.md"));
    }

    fn write_review_plan(content: &str) {
        setup_dex_dir();
        fs::write(dex_path("review-plan.md"), content).unwrap();
    }

    #[test]
    fn focused_reviewer_names_do_not_overlap_with_broad_reviewers() {
        let r = Reviewers::builtin();
        for focused in &r.focused {
            for broad in &r.broad {
                assert_ne!(focused.name, broad.name);
                assert!(!focused.name.contains(&broad.name));
                assert!(!broad.name.contains(&focused.name));
            }
        }
    }

    #[test]
    fn is_clean_review_accepts_variations() {
        let re = clean_review_re();
        assert!(re.is_match("- ZERO FINDINGS"));
        assert!(re.is_match("- zero findings"));
        assert!(re.is_match("- Zero Findings"));
        assert!(re.is_match("* No issues"));
        assert!(re.is_match("- No findings"));
        assert!(re.is_match("* ZERO ISSUES"));
        assert!(re.is_match("  - zero  findings  "));
    }

    #[test]
    fn is_clean_review_rejects_dirty() {
        let re = clean_review_re();
        assert!(!re.is_match("Found 3 issues"));
        assert!(!re.is_match("Some problems detected"));
        assert!(!re.is_match(""));
    }

    #[test]
    fn bare_request_file_is_trimmed() {
        let path = write_temp_file("  hello from file  \n");
        let request = bare_request_state(path.to_str().unwrap());
        let _ = fs::remove_file(&path);

        assert!(matches!(
            request.unwrap(),
            BareRequestState::Ready(request) if request == "hello from file"
        ));
    }

    #[test]
    fn bare_request_file_stops_on_empty_trimmed_content() {
        let path = write_temp_file(" \n\t ");
        let request = bare_request_state(path.to_str().unwrap());
        let _ = fs::remove_file(&path);

        assert!(matches!(request.unwrap(), BareRequestState::Empty));
    }

    #[test]
    fn bare_request_file_stops_on_missing_file() {
        let path = std::env::temp_dir().join(format!(
            "dex-phases-missing-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        assert!(matches!(
            bare_request_state(path.to_str().unwrap()).unwrap(),
            BareRequestState::Missing
        ));
    }

    #[test]
    fn format_task_label_strips_markdown_heading_prefix() {
        assert_eq!(
            format_header_for_display("### Task 2: Build API"),
            "Task 2: Build API"
        );
        assert_eq!(format_header_for_display("## Overview"), "Overview");
        assert_eq!(format_header_for_display(""), "(unnamed task)");
    }

    #[test]
    fn generate_review_plan_contains_correct_structure() {
        let reviewers = Reviewers::builtin();
        let plan = generate_review_plan("abc123", &reviewers);

        assert!(plan.starts_with("<!-- base_ref: abc123 -->"));
        assert!(plan.contains("## Broad Review"));
        assert!(plan.contains("- [ ] quality"));
        assert!(plan.contains("- [ ] implementation"));
        assert!(plan.contains("## Broad Fixer"));
        assert!(plan.contains("- [ ] fix broad review findings"));
        assert!(plan.contains("## Focused Review — Round 1/3"));
        assert!(plan.contains("- [ ] critical-correctness"));
        assert!(plan.contains("- [ ] critical-coverage"));
        assert!(plan.contains("## Focused Fixer — Round 1"));
        assert!(plan.contains("- [ ] fix focused round 1 findings"));
        assert!(plan.contains("## Focused Fixer — Round 3"));
        assert!(plan.contains("- [ ] fix focused round 3 findings"));
    }

    #[test]
    fn read_review_plan_base_ref_extracts_base_ref() {
        write_review_plan("<!-- base_ref: feature-x -->\n\n## Broad Review\n- [ ] quality\n");
        let result = read_review_plan_base_ref();
        assert_eq!(result, Some("feature-x".to_string()));
        teardown_dex_dir();
    }

    #[test]
    fn read_review_plan_base_ref_returns_none_for_missing_comment() {
        write_review_plan("## Broad Review\n- [ ] quality\n");
        let result = read_review_plan_base_ref();
        assert_eq!(result, None);
        teardown_dex_dir();
    }

    #[test]
    fn read_review_plan_base_ref_returns_none_for_missing_file() {
        setup_dex_dir();
        teardown_dex_dir();
        let result = read_review_plan_base_ref();
        assert_eq!(result, None);
    }

    #[test]
    fn mark_review_step_done_checks_correct_heading() {
        write_review_plan("<!-- base_ref: main -->\n\n## Broad Review\n- [ ] quality\n- [ ] security\n\n## Broad Fixer\n- [ ] fix broad review findings\n");

        mark_review_step_done("## Broad Review", "quality");

        let updated = fs::read_to_string(dex_path("review-plan.md")).unwrap();
        assert!(updated.contains("- [x] quality"));
        assert!(updated.contains("- [ ] security"));
        assert!(updated.contains("- [ ] fix broad review findings"));
        teardown_dex_dir();
    }

    #[test]
    fn mark_review_step_done_does_not_cross_headings() {
        write_review_plan("<!-- base_ref: main -->\n\n## Broad Review\n- [ ] quality\n\n## Focused Review — Round 1/3\n- [ ] quality\n");

        mark_review_step_done("## Broad Review", "quality");

        let updated = fs::read_to_string(dex_path("review-plan.md")).unwrap();
        let lines: Vec<&str> = updated.lines().collect();
        let broad_quality_checked = lines.iter().position(|l| *l == "- [x] quality");
        let focused_quality_unchecked = lines.iter().position(|l| *l == "- [ ] quality");
        assert!(broad_quality_checked.is_some());
        assert!(focused_quality_unchecked.is_some());
        assert!(broad_quality_checked.unwrap() < focused_quality_unchecked.unwrap());
        teardown_dex_dir();
    }

    #[test]
    fn mark_review_step_done_noop_if_name_not_found() {
        write_review_plan("<!-- base_ref: main -->\n\n## Broad Review\n- [ ] quality\n");

        mark_review_step_done("## Broad Review", "nonexistent");

        let updated = fs::read_to_string(dex_path("review-plan.md")).unwrap();
        assert!(updated.contains("- [ ] quality"));
        assert!(!updated.contains("- [x]"));
        teardown_dex_dir();
    }

    #[test]
    fn mark_remaining_skipped_marks_all_open() {
        write_review_plan("<!-- base_ref: main -->\n\n## Broad Review\n- [x] quality\n- [ ] security\n\n## Focused Review — Round 2/3\n- [ ] critical-correctness\n");

        mark_remaining_skipped("skipped — prior round clean");

        let updated = fs::read_to_string(dex_path("review-plan.md")).unwrap();
        assert!(updated.contains("- [x] quality"));
        assert!(updated.contains("- [x] security (skipped — prior round clean)"));
        assert!(updated.contains("- [x] critical-correctness (skipped — prior round clean)"));
        teardown_dex_dir();
    }

    #[test]
    fn plans_have_same_structure_matches_identical() {
        let a = "## Broad Review\n- [ ] quality\n- [ ] security\n";
        let b = "## Broad Review\n- [x] quality\n- [x] security\n";
        assert!(plans_have_same_structure(a, b));
    }

    #[test]
    fn plans_have_same_structure_rejects_different_names() {
        let a = "## Broad Review\n- [ ] quality\n";
        let b = "## Broad Review\n- [ ] security\n";
        assert!(!plans_have_same_structure(a, b));
    }

    #[test]
    fn plans_have_same_structure_rejects_different_headings() {
        let a = "## Broad Review\n- [ ] quality\n";
        let b = "## Focused Review\n- [ ] quality\n";
        assert!(!plans_have_same_structure(a, b));
    }

    #[test]
    fn extract_round_from_heading_parses_round() {
        assert_eq!(extract_round_from_heading("Focused Review — Round 2/3"), 2);
        assert_eq!(extract_round_from_heading("Focused Review — Round 1/3"), 1);
        assert_eq!(extract_round_from_heading("Focused Fixer — Round 3"), 3);
    }

    #[test]
    fn extract_round_from_heading_defaults_to_one() {
        assert_eq!(extract_round_from_heading("Focused Review"), 1);
    }

    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_duration_hours_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 0m 0s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m 1s");
        assert_eq!(format_duration(Duration::from_secs(7384)), "2h 3m 4s");
    }
}
