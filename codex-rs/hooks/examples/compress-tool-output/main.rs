use std::collections::BTreeSet;
use std::io::Read;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

static WORD_COUNT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b\d+\s+(passed|failed|error|warning|test|tests|file|files)\b")
        .expect("valid word-count regex")
});
static TEST_RUNNER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(PASS|FAIL|ok|not ok)\b").expect("valid test runner regex"));
static SIGNAL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(error|enoent|fatal|panic|warning|warn|fail|reject|denied|refused|forbidden)")
        .expect("valid signal regex")
});
static DIFF_ALERT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^[+-].*(error|warn)").expect("valid diff alert regex"));
static DIFF_STAT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\|\s*\d+)|((files?|insertions?|deletions?)\b)").expect("valid diff stat regex")
});
static CARGO_ERROR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"error\[E\d+\]").expect("valid cargo error regex"));
static CARGO_WARNING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"warning(\[|:)").expect("valid cargo warning regex"));

#[derive(Deserialize)]
struct HookInput {
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_output: Option<String>,
}

#[derive(Serialize)]
struct HookOutput {
    #[serde(rename = "updatedToolOutput")]
    updated_tool_output: String,
}

fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}

fn append_footer(compressed: String, original_lines: usize, original_tokens: usize) -> String {
    let compressed_tokens = estimate_tokens(&compressed);
    format!(
        "{compressed}\n\n[Compressed by hook: {original_lines} lines / ~{original_tokens} tok → ~{compressed_tokens} tok]"
    )
}

fn is_git_status(command: &str) -> bool {
    command.starts_with("git status") || command == "git"
}

fn is_git_log(command: &str) -> bool {
    command.starts_with("git log")
}

fn is_git_diff(command: &str) -> bool {
    command.starts_with("git diff")
}

fn is_npm(command: &str) -> bool {
    command.starts_with("npm install") || command.starts_with("npm run")
}

fn is_cargo(command: &str) -> bool {
    command.starts_with("cargo build") || command.starts_with("cargo test")
}

fn is_pip_install(command: &str) -> bool {
    command.starts_with("pip install")
}

fn is_docker_logs(command: &str) -> bool {
    command.starts_with("docker logs")
}

fn is_ls_or_find(command: &str) -> bool {
    command.starts_with("ls") || command.starts_with("find")
}

fn is_make_or_cmake(command: &str) -> bool {
    command.starts_with("make") || command.starts_with("cmake")
}

fn compress_git_status(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let summary = lines
        .iter()
        .find(|line| line.starts_with("On branch") || line.starts_with("HEAD detached"))
        .copied()
        .or_else(|| lines.iter().find(|line| !line.trim().is_empty()).copied())?;

    let changed: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| {
            line.starts_with('\t')
                || line.contains("modified:")
                || line.contains("new file:")
                || line.contains("deleted:")
                || line.contains("renamed:")
                || line.contains("both modified:")
        })
        .collect();

    let mut out = String::new();
    out.push_str(summary);
    out.push('\n');
    out.push_str(&format!("Changed files: {}\n", changed.len()));

    for line in changed.iter().take(20) {
        out.push_str(line.trim_end());
        out.push('\n');
    }

    if changed.len() > 20 {
        out.push_str(&format!("[... {} more files]", changed.len() - 20));
    }

    Some(out.trim_end().to_string())
}

fn compress_git_log(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut i = 0;
    let mut kept = 0;
    let mut out = Vec::new();

    while i < lines.len() && kept < 15 {
        if !lines[i].starts_with("commit ") {
            i += 1;
            continue;
        }

        out.push(lines[i].to_string());
        kept += 1;

        let mut j = i + 1;
        let mut message: Option<&str> = None;
        while j < lines.len() && !lines[j].starts_with("commit ") {
            let trimmed = lines[j].trim();
            if !trimmed.is_empty()
                && !trimmed.starts_with("Author:")
                && !trimmed.starts_with("Date:")
                && message.is_none()
            {
                message = Some(trimmed);
            }
            j += 1;
        }

        if let Some(message) = message {
            out.push(format!("    {message}"));
        }

        i = j;
    }

    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

fn compress_git_diff(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut keep = BTreeSet::new();
    let mut hunk_count = 0;

    for (idx, line) in lines.iter().enumerate() {
        if DIFF_STAT_RE.is_match(line) {
            keep.insert(idx);
        }

        if (line.starts_with("diff --git")
            || line.starts_with("@@")
            || line.starts_with('+')
            || line.starts_with('-'))
            && hunk_count < 30
        {
            keep.insert(idx);
            hunk_count += 1;
        }

        if DIFF_ALERT_RE.is_match(line) {
            keep.insert(idx);
        }
    }

    if keep.is_empty() {
        return None;
    }

    let mut out = Vec::with_capacity(keep.len());
    for idx in keep {
        out.push(lines[idx]);
    }

    Some(out.join("\n"))
}

fn compress_npm(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut out: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.contains("ERR!") || line.contains("WARN"))
        .collect();

    if let Some(summary) = lines.iter().rev().find(|line| !line.trim().is_empty()).copied()
        && !out.contains(&summary)
    {
        out.push(summary);
    }

    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

fn compress_cargo(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut out = Vec::new();
    let mut compiling_kept = 0;

    for line in &lines {
        if line.starts_with("Compiling") {
            if compiling_kept < 5 {
                out.push(*line);
                compiling_kept += 1;
            }
            continue;
        }

        if CARGO_ERROR_RE.is_match(line)
            || CARGO_WARNING_RE.is_match(line)
            || line.contains("test result:")
            || line.starts_with("Finished")
            || line.starts_with("Running")
        {
            out.push(*line);
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

fn compress_pip(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut out = Vec::new();
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if line.contains("Successfully installed")
            || lower.contains("error")
            || lower.contains("failed")
        {
            out.push(line);
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

fn compress_docker_logs(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut keep = BTreeSet::new();
    let tail_start = lines.len().saturating_sub(20);
    for idx in tail_start..lines.len() {
        keep.insert(idx);
    }

    for (idx, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("error") || lower.contains("warn") || lower.contains("fatal") {
            keep.insert(idx);
        }
    }

    let out: Vec<&str> = keep.into_iter().map(|idx| lines[idx]).collect();
    Some(out.join("\n"))
}

fn compress_ls_find(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let total = lines.len();
    let mut out: Vec<&str> = lines.iter().copied().take(30).collect();
    if total > 30 {
        out.push("");
    }

    let mut rendered = out.join("\n");
    if total > 30 {
        rendered.push_str(&format!("[... {total} total entries]"));
    }

    Some(rendered)
}

fn compress_make(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut out: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.to_ascii_lowercase().contains("error"))
        .collect();

    if let Some(status_line) = lines.iter().rev().find(|line| !line.trim().is_empty()).copied()
        && !out.contains(&status_line)
    {
        out.push(status_line);
    }

    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

fn tier1_compress(command: &str, output: &str) -> Option<String> {
    let command = command.trim().to_ascii_lowercase();

    if is_git_status(&command) {
        return compress_git_status(output);
    }
    if is_git_log(&command) {
        return compress_git_log(output);
    }
    if is_git_diff(&command) {
        return compress_git_diff(output);
    }
    if is_npm(&command) {
        return compress_npm(output);
    }
    if is_cargo(&command) {
        return compress_cargo(output);
    }
    if is_pip_install(&command) {
        return compress_pip(output);
    }
    if is_docker_logs(&command) {
        return compress_docker_logs(output);
    }
    if is_ls_or_find(&command) {
        return compress_ls_find(output);
    }
    if is_make_or_cmake(&command) {
        return compress_make(output);
    }

    None
}

fn is_signal_line(line: &str) -> bool {
    if SIGNAL_RE.is_match(line)
        || TEST_RUNNER_RE.is_match(line)
        || WORD_COUNT_RE.is_match(line)
        || line.starts_with('+')
        || line.starts_with('-')
        || line.starts_with('>')
        || line.contains("Caused by:")
        || line.contains("Traceback")
        || line.contains("  File \"")
    {
        return true;
    }

    let lower = line.to_ascii_lowercase();
    lower.contains("at ")
        || lower.contains("exit code")
        || lower.contains("status:")
        || lower.contains("returned")
        || lower.contains("exited")
}

fn normalize_for_similarity(line: &str) -> String {
    line.trim()
        .chars()
        .map(|ch| if ch.is_ascii_digit() { '#' } else { ch })
        .collect()
}

fn dedup_similar(lines: Vec<&str>) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let key = normalize_for_similarity(lines[i]);
        let mut run_end = i + 1;

        while run_end < lines.len() && normalize_for_similarity(lines[run_end]) == key {
            run_end += 1;
        }

        let run_len = run_end - i;
        if run_len >= 3 {
            out.push(lines[i].to_string());
            out.push(format!("[... repeated {run_len} similar lines ...]"));
        } else {
            for line in &lines[i..run_end] {
                out.push((*line).to_string());
            }
        }

        i = run_end;
    }

    out
}

fn tier2_compress(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut keep = BTreeSet::new();

    for idx in 0..lines.len().min(15) {
        keep.insert(idx);
    }

    let tail_start = lines.len().saturating_sub(10);
    for idx in tail_start..lines.len() {
        keep.insert(idx);
    }

    for (idx, line) in lines.iter().enumerate() {
        if is_signal_line(line) {
            keep.insert(idx);
        }
    }

    let selected: Vec<&str> = keep.into_iter().map(|idx| lines[idx]).collect();
    let deduped = dedup_similar(selected);
    deduped.join("\n")
}

fn tier3_compress(output: &str, original_lines: usize, original_tokens: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut out = Vec::new();
    let head_count = lines.len().min(30);
    out.extend(lines.iter().take(head_count).copied());

    let tail_start = lines.len().saturating_sub(10);
    if tail_start > head_count {
        out.push("[... hard-capped output ...]");
        out.extend(lines.iter().skip(tail_start).copied());
    }

    let mut rendered = out.join("\n");
    rendered.push_str(&format!(
        "\n[... original {original_lines} lines / ~{original_tokens} tok ...]"
    ));
    rendered
}

fn compress_read_output(output: &str) -> Option<String> {
    let original_tokens = estimate_tokens(output);
    if original_tokens <= 1000 {
        return None;
    }

    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let total = lines.len();
    let mut out: Vec<&str> = lines.iter().copied().take(100).collect();
    out.push("");

    let mut rendered = out.join("\n");
    rendered.push_str(&format!(
        "[... truncated read output: {total} lines / ~{original_tokens} tok]"
    ));
    Some(rendered)
}

fn compress(tool_name: &str, command: Option<&str>, output: &str) -> Option<String> {
    let original_tokens = estimate_tokens(output);
    let original_lines = output.lines().count();

    if tool_name != "Bash" {
        return compress_read_output(output)
            .map(|compressed| append_footer(compressed, original_lines, original_tokens));
    }

    if original_tokens < 500 {
        return None;
    }

    if let Some(command) = command
        && let Some(mut compressed) = tier1_compress(command, output)
    {
        if original_tokens > 5000 && estimate_tokens(&compressed) > 1500 {
            compressed = tier3_compress(output, original_lines, original_tokens);
        }
        return Some(append_footer(compressed, original_lines, original_tokens));
    }

    if original_tokens > 1500 {
        let mut compressed = tier2_compress(output);
        if original_tokens > 5000 && estimate_tokens(&compressed) > 1500 {
            compressed = tier3_compress(output, original_lines, original_tokens);
        }
        return Some(append_footer(compressed, original_lines, original_tokens));
    }

    None
}

fn extract_bash_command(tool_input: Option<&Value>) -> Option<&str> {
    tool_input?.get("command")?.as_str()
}

fn process_input(raw: &str) -> Option<String> {
    let parsed: HookInput = serde_json::from_str(raw).ok()?;
    let tool_name = parsed.tool_name.as_deref()?;
    let tool_output = parsed.tool_output.as_deref()?;

    let command = if tool_name == "Bash" {
        extract_bash_command(parsed.tool_input.as_ref())
    } else {
        None
    };

    let updated_tool_output = compress(tool_name, command, tool_output)?;
    let response = HookOutput {
        updated_tool_output,
    };

    serde_json::to_string(&response).ok()
}

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }

    if let Some(output) = process_input(&input) {
        print!("{output}");
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde::Deserialize;
    use serde_json::json;

    use super::process_input;

    #[derive(Deserialize)]
    struct TestOutput {
        #[serde(rename = "updatedToolOutput")]
        updated_tool_output: String,
    }

    fn hook_input(tool_name: &str, command: Option<&str>, output: &str) -> String {
        let payload = if tool_name == "Bash" {
            json!({
                "session_id": "s1",
                "hook_event_name": "AfterToolUse",
                "tool_name": tool_name,
                "tool_input": { "command": command.unwrap_or("") },
                "tool_output": output,
                "executed": true,
                "success": true
            })
        } else {
            json!({
                "session_id": "s1",
                "hook_event_name": "AfterToolUse",
                "tool_name": tool_name,
                "tool_input": {},
                "tool_output": output,
                "executed": true,
                "success": true
            })
        };

        payload.to_string()
    }

    fn run_hook(raw: &str) -> Option<String> {
        let encoded = process_input(raw)?;
        let decoded: TestOutput = serde_json::from_str(&encoded).ok()?;
        Some(decoded.updated_tool_output)
    }

    #[test]
    fn pass_through_small_output() {
        let raw = hook_input("Bash", Some("git status"), "small output");
        assert_eq!(None, process_input(&raw));
    }

    #[test]
    fn tier1_git_status_compression() {
        let mut body = String::from("On branch main\nChanges not staged for commit:\n");
        for i in 0..80 {
            body.push_str(&format!("\tmodified:   src/file_{i}.rs\n"));
        }

        let raw = hook_input("Bash", Some("git status"), &body);
        let output = run_hook(&raw).expect("compressed output expected");

        assert!(output.contains("On branch main"));
        assert!(output.contains("Changed files: 80"));
        assert!(output.contains("file_0.rs"));
        assert!(!output.contains("file_40.rs"));
        assert!(output.contains("Compressed by hook"));
    }

    #[test]
    fn tier1_npm_install_compression() {
        let mut body = String::new();
        for i in 0..400 {
            body.push_str(&format!("fetching package {i}\n"));
        }
        body.push_str("npm WARN deprecated package-x@1.0.0\n");
        body.push_str("npm ERR! code ERESOLVE\n");
        body.push_str("added 32 packages, and audited 32 packages in 1s\n");

        let raw = hook_input("Bash", Some("npm install"), &body);
        let output = run_hook(&raw).expect("compressed output expected");

        assert!(output.contains("npm WARN"));
        assert!(output.contains("npm ERR!"));
        assert!(output.contains("added 32 packages"));
        assert!(!output.contains("fetching package 12"));
    }

    #[test]
    fn tier1_cargo_build_compression() {
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&format!("Compiling crate_{i} v0.1.0\n"));
        }
        body.push_str("warning: unused import: `foo`\n");
        body.push_str("error[E0425]: cannot find value `x` in this scope\n");
        body.push_str("Finished dev [unoptimized + debuginfo] target(s) in 3.21s\n");

        let raw = hook_input("Bash", Some("cargo build"), &body);
        let output = run_hook(&raw).expect("compressed output expected");

        assert!(output.contains("error[E0425]"));
        assert!(output.contains("warning:"));
        assert!(output.contains("Finished dev"));
        assert!(!output.contains("crate_299"));
    }

    #[test]
    fn tier2_generic_large_output_compression() {
        let mut body = String::new();
        for i in 0..900 {
            body.push_str(&format!("line {i}: normal output\n"));
        }
        for i in 0..6 {
            body.push_str(&format!("ERROR connection refused attempt {i}\n"));
        }
        body.push_str("Traceback (most recent call last):\n");
        body.push_str("  File \"main.py\", line 3, in <module>\n");
        body.push_str("status: exited with code 1\n");

        let raw = hook_input("Bash", Some("custom_command --long"), &body);
        let output = run_hook(&raw).expect("compressed output expected");

        assert!(output.contains("ERROR connection refused"));
        assert!(output.contains("Traceback"));
        assert!(output.contains("status: exited with code 1"));
        assert!(output.contains("[... repeated"));
    }

    #[test]
    fn tier3_hard_cap_fallback() {
        let mut body = String::new();
        for i in 0..80 {
            let marker = char::from(b'a' + (i % 26) as u8);
            let huge_line = format!("{}{}", marker.to_string().repeat(800), "tail");
            body.push_str(&format!("{i}: {huge_line}\n"));
        }

        let raw = hook_input("Bash", Some("unknowncmd"), &body);
        let output = run_hook(&raw).expect("compressed output expected");

        assert!(output.contains("[... hard-capped output ...]"));
        assert!(output.contains("original 80 lines"));
        assert!(output.contains("Compressed by hook"));
    }

    #[test]
    fn read_tool_handling() {
        let mut body = String::new();
        for i in 0..600 {
            body.push_str(&format!("line {i} long long long\n"));
        }

        let raw = hook_input("Read", None, &body);
        let output = run_hook(&raw).expect("compressed output expected");

        assert!(output.contains("line 0 long long long"));
        assert!(output.contains("line 99 long long long"));
        assert!(!output.contains("line 200 long long long"));
        assert!(output.contains("truncated read output"));
    }

    #[test]
    fn malformed_input_passes_through_silently() {
        assert_eq!(None, process_input("{not-json}"));
        assert_eq!(None, process_input("{}"));
    }
}
