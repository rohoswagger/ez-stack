use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(repo_root())
        .env("NO_COLOR", "1")
        .output()
        .expect("run command")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn read_repo_file(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path))
        .unwrap_or_else(|err| panic!("read {path}: {err}"))
}

fn parse_help_commands(help: &str) -> BTreeSet<String> {
    let mut commands = BTreeSet::new();
    let mut in_commands = false;

    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed == "Commands:" {
            in_commands = true;
            continue;
        }
        if in_commands && trimmed == "Options:" {
            break;
        }
        if !in_commands || trimmed.is_empty() {
            continue;
        }
        let Some(command) = trimmed.split_whitespace().next() else {
            continue;
        };
        if command != "help" {
            commands.insert(command.to_string());
        }
    }

    commands
}

fn parse_listed_commands(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn job_block(workflow: &str, job_name: &str) -> String {
    let lines: Vec<&str> = workflow.lines().collect();
    let header = format!("{job_name}:");
    let start = lines
        .iter()
        .position(|line| leading_spaces(line) == 2 && line.trim() == header)
        .unwrap_or_else(|| panic!("workflow job {job_name} is missing"));
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            leading_spaces(line) == 2 && line.trim_end().ends_with(':') && !line.trim().is_empty()
        })
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());

    lines[start..end].join("\n")
}

fn contains_yaml_token(text: &str, token: &str) -> bool {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .any(|part| part == token)
}

fn job_needs(job: &str, dependency: &str) -> bool {
    let lines: Vec<&str> = job.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("needs:") {
            let indent = leading_spaces(line);
            let value = line.trim_start()["needs:".len()..].trim();
            if contains_yaml_token(value, dependency) {
                return true;
            }
            for child in &lines[index + 1..] {
                if !child.trim().is_empty() && leading_spaces(child) <= indent {
                    break;
                }
                if contains_yaml_token(child.trim(), dependency) {
                    return true;
                }
            }
        }
    }
    false
}

fn assert_job_needs(workflow: &str, job_name: &str, dependency: &str) {
    let job = job_block(workflow, job_name);
    assert!(
        job_needs(&job, dependency),
        "expected job {job_name} to declare needs: {dependency}\njob block:\n{job}"
    );
}

#[test]
fn canary_lists_every_operational_top_level_command() {
    let help_output = run(env!("CARGO_BIN_EXE_ez"), &["--help"]);
    assert!(
        help_output.status.success(),
        "ez --help failed:\nstdout:\n{}\nstderr:\n{}",
        stdout_text(&help_output),
        stderr_text(&help_output)
    );
    let help_commands = parse_help_commands(&stdout_text(&help_output));
    assert!(
        !help_commands.is_empty(),
        "expected ez --help to include a Commands: block"
    );

    let canary_output = run("bash", &["scripts/github-canary.sh", "--list-commands"]);
    assert!(
        canary_output.status.success(),
        "github canary command listing failed:\nstdout:\n{}\nstderr:\n{}",
        stdout_text(&canary_output),
        stderr_text(&canary_output)
    );
    let canary_commands = parse_listed_commands(&stdout_text(&canary_output));

    assert_eq!(
        help_commands, canary_commands,
        "github canary command list must exactly match operational top-level ez commands"
    );
}

#[test]
fn canary_script_has_valid_bash_syntax() {
    let output = run("bash", &["-n", "scripts/github-canary.sh"]);
    assert!(
        output.status.success(),
        "github canary script has invalid bash syntax:\nstdout:\n{}\nstderr:\n{}",
        stdout_text(&output),
        stderr_text(&output)
    );
}

#[test]
fn canary_workflow_exposes_manual_reusable_write_enabled_job() {
    let workflow = read_repo_file(".github/workflows/github-canary.yml");
    for expected in [
        "workflow_call",
        "workflow_dispatch",
        "ubuntu-latest",
        "contents: write",
        "pull-requests: write",
        "persist-credentials: false",
        "scripts/github-canary.sh",
    ] {
        assert!(
            workflow.contains(expected),
            "expected github-canary.yml to contain {expected:?}"
        );
    }
}

#[test]
fn release_workflows_run_canary_before_build_jobs() {
    let release = read_repo_file(".github/workflows/release.yml");
    assert!(
        release.contains("uses: ./.github/workflows/github-canary.yml"),
        "release.yml must invoke the github canary reusable workflow"
    );
    assert_job_needs(&release, "build", "canary");

    let python_wheel = read_repo_file(".github/workflows/python-wheel.yml");
    assert!(
        python_wheel.contains("uses: ./.github/workflows/github-canary.yml"),
        "python-wheel.yml must invoke the github canary reusable workflow"
    );
    assert_job_needs(&python_wheel, "build-wheel", "canary");
}

#[test]
fn release_builds_are_locked_to_the_committed_dependency_graph() {
    let release = read_repo_file(".github/workflows/release.yml");
    assert!(
        job_block(&release, "build")
            .contains("cargo build --release --locked --target ${{ matrix.target }}"),
        "release binaries must be built from Cargo.lock"
    );

    let python_wheel = read_repo_file(".github/workflows/python-wheel.yml");
    assert!(
        job_block(&python_wheel, "build-wheel")
            .contains("cargo build --release --locked --target ${{ matrix.target }}"),
        "Python wheel binaries must be built from Cargo.lock"
    );
}
