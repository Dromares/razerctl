//! A `run:` step in `release.yml` that redirects output into a directory must create that directory first, unless a fresh
//! checkout already holds it (DAN-1693: `> target/release-notes.md` ran before cargo had made `target/`, so the first hosted
//! run failed with "No such file or directory"). The shell opens the redirect before the command starts.

use std::path::Path;
use std::process::Command;

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Tracked paths, i.e. what a fresh checkout holds; `None` when this is not a git checkout (a crate tarball, no git binary).
fn tracked_files() -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["ls-files", "-z"])
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    })
}

/// The `run:` blocks of the workflow, one `Vec` of trimmed lines per step (a single-line `run: cmd` too).
fn run_blocks(yml: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut lines = yml.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim_start().strip_prefix("run:") else {
            continue;
        };
        let indent = line.len() - line.trim_start().len();
        let rest = rest.trim();
        if rest != "|" {
            blocks.push(vec![rest.to_string()]);
            continue;
        }
        let mut block = Vec::new();
        while let Some(next) = lines.peek() {
            let next_indent = next.len() - next.trim_start().len();
            if !next.trim().is_empty() && next_indent <= indent {
                break;
            }
            block.push(lines.next().unwrap().trim().to_string());
        }
        blocks.push(block);
    }
    blocks
}

fn unquote(t: &str) -> &str {
    t.trim_matches(|c| c == '"' || c == '\'')
        .trim_end_matches('/')
}

/// The directory part of every `> path/file` or `>> path/file` redirect target on this line.
fn redirect_dirs(line: &str) -> Vec<String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let mut dirs = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let target = match *t {
            ">" | ">>" => toks.get(i + 1).copied(),
            _ => t
                .strip_prefix(">>")
                .or_else(|| t.strip_prefix('>'))
                .filter(|p| !p.is_empty() && !p.starts_with('&')),
        };
        if let Some((dir, _file)) = target.map(unquote).and_then(|p| p.rsplit_once('/')) {
            dirs.push(dir.to_string());
        }
    }
    dirs
}

/// The directories a `mkdir` on this line creates, with or without `-p`.
fn mkdir_dirs(line: &str) -> Vec<String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    toks.iter()
        .position(|t| *t == "mkdir")
        .map(|i| {
            toks[i + 1..]
                .iter()
                .filter(|t| !t.starts_with('-'))
                .map(|t| unquote(t).to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn every_redirect_target_directory_exists_by_the_time_the_step_writes_it() {
    let yml = read(".github/workflows/release.yml");
    let tracked = tracked_files();
    let mut redirects = 0;
    let mut missing = Vec::new();
    for block in run_blocks(&yml) {
        let mut made: Vec<String> = Vec::new();
        for line in &block {
            for dir in redirect_dirs(line) {
                redirects += 1;
                let in_checkout = tracked
                    .as_ref()
                    .is_none_or(|t| t.iter().any(|f| f.starts_with(&format!("{dir}/"))));
                let created = made
                    .iter()
                    .any(|m| dir == *m || dir.starts_with(&format!("{m}/")));
                if !in_checkout && !created {
                    missing.push(format!(
                        "`{line}` redirects into `{dir}/`, which neither a fresh checkout nor an earlier line of the step creates"
                    ));
                }
            }
            made.extend(mkdir_dirs(line));
        }
    }
    assert!(
        redirects > 0,
        "found no `> path/file` redirect in release.yml; the parser no longer sees the release-notes line"
    );
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

/// The steps of the workflow, each as its raw lines (a step starts at a `      - ` line, six spaces deep).
fn steps(yml: &str) -> Vec<Vec<&str>> {
    let mut steps: Vec<Vec<&str>> = Vec::new();
    for line in yml.lines() {
        if line.starts_with("      - ") {
            steps.push(vec![line]);
        } else if let Some(cur) = steps.last_mut() {
            cur.push(line);
        }
    }
    steps
}

fn creates_edits_or_uploads_a_release(step: &str) -> bool {
    [
        "gh release",
        "gh api",
        "upload-artifact",
        "action-gh-release",
        "git push",
        "git tag",
    ]
    .iter()
    .any(|needle| step.contains(needle))
}

/// DAN-1735: a `workflow_dispatch` dry run builds, packages and checksums but publishes nothing. The input must exist,
/// default to the safe value, and guard every step that creates, edits or uploads a release.
#[test]
fn dispatch_dry_run_input_exists_and_defaults_to_dry() {
    let yml = read(".github/workflows/release.yml");
    let at = yml
        .find("workflow_dispatch:")
        .expect("release.yml has no workflow_dispatch trigger");
    let trigger = yml[at..].split("\npermissions:").next().unwrap();
    assert!(
        trigger.contains("dry_run:"),
        "workflow_dispatch has no `dry_run` input:\n{trigger}"
    );
    assert!(
        trigger.contains("type: boolean") && trigger.contains("default: true"),
        "`dry_run` must be a boolean that defaults to true, so a bare dispatch publishes nothing:\n{trigger}"
    );
}

#[test]
fn every_step_that_publishes_is_guarded_by_dry_run() {
    let yml = read(".github/workflows/release.yml");
    let mut publish_steps = 0;
    let mut unguarded = Vec::new();
    for step in steps(&yml) {
        // comment lines may mention `gh release`; only executable text counts
        let text = step
            .iter()
            .filter(|l| !l.trim_start().starts_with('#'))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        if !creates_edits_or_uploads_a_release(&text) {
            continue;
        }
        publish_steps += 1;
        let guard = text
            .lines()
            .find(|l| l.trim_start().trim_start_matches("- ").starts_with("if:"));
        let ok = guard.is_some_and(|g| {
            g.contains("inputs.dry_run") && g.contains("github.event_name == 'push'")
        });
        if !ok {
            unguarded.push(text.lines().next().unwrap_or_default().trim().to_string());
        }
    }
    assert!(
        publish_steps > 0,
        "found no step that creates a release; the publish detector no longer sees the draft-release step"
    );
    assert!(
        unguarded.is_empty(),
        "these steps can publish on a dry run (need `if:` naming inputs.dry_run and the push event): {unguarded:?}"
    );
}

/// The job must not hold a release-capable token for the dry run: `GH_TOKEN` belongs to the guarded step only.
#[test]
fn release_token_is_not_set_at_job_level() {
    let yml = read(".github/workflows/release.yml");
    let job_env = yml
        .split("\n    steps:")
        .next()
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !job_env.contains("GH_TOKEN") && !job_env.contains("github.token"),
        "GH_TOKEN is set for every step, dry run included:\n{job_env}"
    );
}
