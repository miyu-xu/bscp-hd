use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const PROCESS_SCHEMA_VERSION: u32 = 2;
const MAX_STATUS_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskStatus {
    Planned,
    InProgress,
    Blocked,
    Complete,
}

impl TaskStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskScope {
    repositories: Vec<String>,
    owned_paths: Vec<String>,
    excluded_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceCriterion {
    id: String,
    criterion: String,
    evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceState {
    code_present: bool,
    tests_authored: bool,
    contract_smoke_verified: bool,
    real_guest_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DecisionOwner {
    Human,
    Ai,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectionDecision {
    id: String,
    decision: String,
    owner: DecisionOwner,
    rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BlockerScope {
    CurrentTask,
    FutureMilestone,
    External,
}

impl BlockerScope {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::CurrentTask => "current_task",
            Self::FutureMilestone => "future_milestone",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BlockerState {
    Open,
    Resolved,
    Accepted,
}

impl BlockerState {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Accepted => "accepted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Blocker {
    id: String,
    description: String,
    scope: BlockerScope,
    state: BlockerState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HumanDecisionState {
    Open,
    Resolved,
    Deferred,
}

impl HumanDecisionState {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Deferred => "deferred",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HumanDecision {
    id: String,
    question: String,
    impact: String,
    state: HumanDecisionState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRecord {
    schema_version: u32,
    task_id: String,
    objective: String,
    status: TaskStatus,
    scope: TaskScope,
    constraints: Vec<String>,
    acceptance: Vec<AcceptanceCriterion>,
    required_gates: Vec<String>,
    evidence_state: EvidenceState,
    direction_decisions: Vec<DirectionDecision>,
    blockers: Vec<Blocker>,
    human_decisions_needed: Vec<HumanDecision>,
    next_iteration: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GateStatus {
    Pass,
    Fail,
    Skipped,
    Missing,
}

impl GateStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
            Self::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GateRecord {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) status: GateStatus,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) log_path: Option<String>,
    pub(crate) summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GateReport {
    schema_version: u32,
    generated_at: String,
    source: String,
    gates: Vec<GateRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryReadback {
    name: String,
    path: String,
    available: bool,
    head: Option<String>,
    dirty: bool,
    changes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Pass,
    Fail,
    Blocked,
    Incomplete,
}

impl Verdict {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Blocked => "blocked",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadbackReport {
    schema_version: u32,
    generated_at: String,
    task_id: String,
    objective: String,
    task_status: TaskStatus,
    verdict: Verdict,
    scope: TaskScope,
    constraints: Vec<String>,
    required_gates: Vec<String>,
    gates: Vec<GateRecord>,
    repositories: Vec<RepositoryReadback>,
    acceptance: Vec<AcceptanceCriterion>,
    evidence_state: EvidenceState,
    direction_decisions: Vec<DirectionDecision>,
    blockers: Vec<Blocker>,
    human_decisions_needed: Vec<HumanDecision>,
    next_iteration: String,
}

pub(crate) fn process_check(root: &Path) -> Result<()> {
    for relative in [
        "AGENTS.md",
        "automation/README.md",
        "automation/schemas/task.schema.json",
        "automation/schemas/gate-report.schema.json",
        "automation/schemas/readback.schema.json",
        "automation/examples/task.example.json",
        "automation/examples/gate-report.example.json",
        "automation/examples/readback.example.json",
        "docs/AI_WORKFLOW.md",
        "docs/TESTING.md",
        "scripts/integration-quality.ps1",
        ".github/workflows/ci.yml",
    ] {
        ensure!(
            root.join(relative).is_file(),
            "required AI process asset is missing: {relative}"
        );
    }

    validate_schema(
        &root.join("automation/schemas/task.schema.json"),
        "https://bscp.dev/hd/schemas/task-v2.json",
    )?;
    validate_schema(
        &root.join("automation/schemas/gate-report.schema.json"),
        "https://bscp.dev/hd/schemas/gate-report-v2.json",
    )?;
    validate_schema(
        &root.join("automation/schemas/readback.schema.json"),
        "https://bscp.dev/hd/schemas/readback-v2.json",
    )?;

    let example_task: TaskRecord = read_json(&root.join("automation/examples/task.example.json"))?;
    validate_task(&example_task, "automation/examples/task.example.json")?;
    let example_gate: GateReport =
        read_json(&root.join("automation/examples/gate-report.example.json"))?;
    validate_gate_report(
        &example_gate,
        "automation/examples/gate-report.example.json",
    )?;
    let example_readback: ReadbackReport =
        read_json(&root.join("automation/examples/readback.example.json"))?;
    validate_readback(
        &example_readback,
        "automation/examples/readback.example.json",
    )?;

    let tasks_dir = root.join("automation/tasks");
    let mut task_count = 0_u32;
    for entry in std::fs::read_dir(&tasks_dir)
        .with_context(|| format!("read task directory {}", tasks_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let task: TaskRecord = read_json(&path)?;
        validate_task(&task, &path.display().to_string())?;
        task_count = task_count.saturating_add(1);
    }
    ensure!(task_count > 0, "automation/tasks contains no task records");

    validate_agents(root)?;
    validate_no_unittest_commands(root)?;
    println!("HD AI process contract passed for {task_count} task records");
    Ok(())
}

pub(crate) fn ai_cycle(root: &Path, task_path: &Path, output_path: &Path) -> Result<()> {
    let task_path = resolve_from(root, task_path);
    let output_path = resolve_from(root, output_path);
    let task: TaskRecord = read_json(&task_path)?;
    validate_task(&task, &task_path.display().to_string())?;
    std::fs::create_dir_all(output_path.join("logs"))
        .with_context(|| format!("create AI cycle output {}", output_path.display()))?;

    let executable = std::env::current_exe().context("resolve current xtask executable")?;
    let output_argument = output_path.to_string_lossy().into_owned();
    let gate = execute_gate(
        root,
        &output_path,
        "hd-quality",
        "xtask quality --evidence-output <output>",
        &executable,
        &["quality", "--evidence-output", &output_argument],
    );
    let mut gates = vec![gate.clone()];
    if gate.status == GateStatus::Pass {
        let smoke_report = read_gate_report(&output_path.join("host-smoke-gates.json"))?;
        gates.extend(smoke_report.gates);
    }
    let report = GateReport {
        schema_version: PROCESS_SCHEMA_VERSION,
        generated_at: now_rfc3339()?,
        source: "xtask ai-cycle".to_owned(),
        gates,
    };
    let gate_report_path = output_path.join("hd-gates.json");
    write_json(&gate_report_path, &report)?;
    write_readback(root, &task, &output_path, &[report])?;

    if gate.status == GateStatus::Pass {
        println!("HD AI cycle evidence: {}", output_path.display());
        Ok(())
    } else {
        bail!(
            "HD AI cycle failed; inspect {} and rerun after repair",
            output_path.join("logs/hd-quality.log").display()
        )
    }
}

pub(crate) fn write_smoke_gate_report(output_path: &Path, gates: Vec<GateRecord>) -> Result<()> {
    ensure!(!gates.is_empty(), "smoke gate report cannot be empty");
    let report = GateReport {
        schema_version: PROCESS_SCHEMA_VERSION,
        generated_at: now_rfc3339()?,
        source: "xtask smoke".to_owned(),
        gates,
    };
    write_json(&output_path.join("host-smoke-gates.json"), &report)
}

pub(crate) fn readback(
    root: &Path,
    task_path: &Path,
    output_path: &Path,
    gate_report_paths: &[PathBuf],
) -> Result<()> {
    let task_path = resolve_from(root, task_path);
    let output_path = resolve_from(root, output_path);
    let task: TaskRecord = read_json(&task_path)?;
    validate_task(&task, &task_path.display().to_string())?;
    std::fs::create_dir_all(&output_path)
        .with_context(|| format!("create readback output {}", output_path.display()))?;

    let paths = if gate_report_paths.is_empty() {
        discover_gate_reports(&output_path)?
    } else {
        gate_report_paths
            .iter()
            .map(|path| resolve_from(root, path))
            .collect()
    };
    let reports = paths
        .iter()
        .map(|path| read_gate_report(path))
        .collect::<Result<Vec<_>>>()?;
    let report = write_readback(root, &task, &output_path, &reports)?;
    println!(
        "HD readback {}: {}",
        report.verdict.as_str(),
        output_path.display()
    );
    Ok(())
}

fn validate_schema(path: &Path, expected_id: &str) -> Result<()> {
    let schema: Value = read_json(path)?;
    ensure!(
        schema.get("$schema").and_then(Value::as_str)
            == Some("https://json-schema.org/draft/2020-12/schema"),
        "{} does not use JSON Schema 2020-12",
        path.display()
    );
    ensure!(
        schema.get("$id").and_then(Value::as_str) == Some(expected_id),
        "{} has an unexpected $id",
        path.display()
    );
    ensure!(
        schema
            .pointer("/properties/schema_version/const")
            .and_then(Value::as_u64)
            == Some(u64::from(PROCESS_SCHEMA_VERSION)),
        "{} does not pin schema_version {}",
        path.display(),
        PROCESS_SCHEMA_VERSION
    );
    Ok(())
}

fn validate_task(task: &TaskRecord, source: &str) -> Result<()> {
    ensure!(
        task.schema_version == PROCESS_SCHEMA_VERSION,
        "{source}: unsupported schema_version {}",
        task.schema_version
    );
    ensure!(valid_task_id(&task.task_id), "{source}: invalid task_id");
    ensure!(
        !task.objective.trim().is_empty(),
        "{source}: objective is empty"
    );
    ensure!(
        !task.scope.repositories.is_empty(),
        "{source}: scope.repositories is empty"
    );
    ensure!(
        task.constraints
            .iter()
            .any(|value| value == "do_not_run_unittest"),
        "{source}: do_not_run_unittest constraint is required"
    );
    ensure!(!task.acceptance.is_empty(), "{source}: acceptance is empty");
    ensure_unique(
        task.acceptance.iter().map(|item| item.id.as_str()),
        source,
        "acceptance id",
    )?;
    for acceptance in &task.acceptance {
        ensure!(
            !acceptance.criterion.trim().is_empty() && !acceptance.evidence.is_empty(),
            "{source}: acceptance {} lacks criterion or evidence",
            acceptance.id
        );
    }
    ensure!(
        task.required_gates.iter().any(|gate| gate == "hd-quality"),
        "{source}: hd-quality is a required baseline gate"
    );
    ensure_unique(
        task.required_gates.iter().map(String::as_str),
        source,
        "required gate",
    )?;
    ensure_unique(
        task.direction_decisions.iter().map(|item| item.id.as_str()),
        source,
        "direction decision id",
    )?;
    ensure_unique(
        task.blockers.iter().map(|item| item.id.as_str()),
        source,
        "blocker id",
    )?;
    ensure_unique(
        task.human_decisions_needed
            .iter()
            .map(|item| item.id.as_str()),
        source,
        "human decision id",
    )?;
    ensure!(
        !task.next_iteration.trim().is_empty(),
        "{source}: next_iteration is empty"
    );
    Ok(())
}

fn validate_readback(report: &ReadbackReport, source: &str) -> Result<()> {
    ensure!(
        report.schema_version == PROCESS_SCHEMA_VERSION,
        "{source}: unsupported readback schema"
    );
    ensure!(
        valid_task_id(&report.task_id),
        "{source}: invalid readback task_id"
    );
    ensure!(
        !report.generated_at.trim().is_empty() && !report.objective.trim().is_empty(),
        "{source}: generated_at or objective is empty"
    );
    OffsetDateTime::parse(&report.generated_at, &Rfc3339)
        .with_context(|| format!("{source}: generated_at is not RFC3339"))?;
    ensure_unique(
        report.gates.iter().map(|gate| gate.name.as_str()),
        source,
        "gate result",
    )
}

fn valid_task_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn ensure_unique<'a>(
    values: impl IntoIterator<Item = &'a str>,
    source: &str,
    label: &str,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        ensure!(!value.trim().is_empty(), "{source}: empty {label}");
        ensure!(seen.insert(value), "{source}: duplicate {label} {value}");
    }
    Ok(())
}

fn validate_agents(root: &Path) -> Result<()> {
    let text = std::fs::read_to_string(root.join("AGENTS.md")).context("read AGENTS.md")?;
    for marker in [
        "人工负责",
        "AI 负责",
        "不运行 unittest",
        "MinGW",
        "readback.json",
    ] {
        ensure!(
            text.contains(marker),
            "AGENTS.md is missing marker: {marker}"
        );
    }
    Ok(())
}

fn validate_no_unittest_commands(root: &Path) -> Result<()> {
    for relative in [
        "build.bat",
        "scripts/quality.ps1",
        "scripts/integration-quality.ps1",
        ".github/workflows/ci.yml",
    ] {
        let path = root.join(relative);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read executable process file {}", path.display()))?;
        for (line_number, line) in text.lines().enumerate() {
            let normalized = line.to_ascii_lowercase();
            for forbidden in [
                "cargo test",
                "cargo nextest",
                "ctest ",
                "python -m unittest",
                "python3 -m unittest",
            ] {
                ensure!(
                    !normalized.contains(forbidden),
                    "{relative}:{} invokes forbidden unittest command: {forbidden}",
                    line_number + 1
                );
            }
        }
    }
    Ok(())
}

fn execute_gate(
    root: &Path,
    output_path: &Path,
    name: &str,
    command_label: &str,
    program: &Path,
    arguments: &[&str],
) -> GateRecord {
    let started = Instant::now();
    let result = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .output();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let log_relative = format!("logs/{name}.log");
    let log_path = output_path.join(&log_relative);

    let (status, summary, log) = match result {
        Ok(output) => {
            let exit = output
                .status
                .code()
                .map_or_else(|| "terminated".to_owned(), |code| code.to_string());
            let mut log = format!("command: {command_label}\nexit: {exit}\n\n[stdout]\n");
            log.push_str(&String::from_utf8_lossy(&output.stdout));
            log.push_str("\n[stderr]\n");
            log.push_str(&String::from_utf8_lossy(&output.stderr));
            if output.status.success() {
                (GateStatus::Pass, format!("exit {exit}"), log)
            } else {
                (GateStatus::Fail, format!("exit {exit}"), log)
            }
        }
        Err(error) => (
            GateStatus::Fail,
            format!("failed to start: {error}"),
            format!("command: {command_label}\nstart error: {error}\n"),
        ),
    };
    let write_error = std::fs::write(&log_path, log).err();
    let summary = if let Some(error) = write_error {
        format!("{summary}; write log failed: {error}")
    } else {
        summary
    };
    GateRecord {
        name: name.to_owned(),
        command: command_label.to_owned(),
        status,
        duration_ms: Some(duration_ms),
        log_path: Some(log_relative),
        summary,
    }
}

fn discover_gate_reports(output_path: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(output_path)
        .with_context(|| format!("read gate report directory {}", output_path.display()))?
    {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("gates.json"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_gate_report(path: &Path) -> Result<GateReport> {
    let report: GateReport = read_json(path)?;
    validate_gate_report(&report, &path.display().to_string())?;
    Ok(report)
}

fn validate_gate_report(report: &GateReport, source: &str) -> Result<()> {
    ensure!(
        report.schema_version == PROCESS_SCHEMA_VERSION,
        "{source} has unsupported gate schema_version"
    );
    ensure!(
        !report.generated_at.trim().is_empty() && !report.source.trim().is_empty(),
        "{source} lacks gate report metadata"
    );
    OffsetDateTime::parse(&report.generated_at, &Rfc3339)
        .with_context(|| format!("{source} generated_at is not RFC3339"))?;
    ensure_unique(
        report.gates.iter().map(|gate| gate.name.as_str()),
        source,
        "gate result",
    )
}

fn write_readback(
    root: &Path,
    task: &TaskRecord,
    output_path: &Path,
    gate_reports: &[GateReport],
) -> Result<ReadbackReport> {
    let mut gates_by_name = BTreeMap::new();
    let mut reports_by_time = gate_reports
        .iter()
        .map(|report| {
            OffsetDateTime::parse(&report.generated_at, &Rfc3339)
                .map(|generated_at| (generated_at, report))
                .with_context(|| format!("invalid gate report time from {}", report.source))
        })
        .collect::<Result<Vec<_>>>()?;
    reports_by_time.sort_by_key(|(generated_at, _)| *generated_at);
    for (_, report) in reports_by_time {
        for gate in &report.gates {
            gates_by_name.insert(gate.name.clone(), gate.clone());
        }
    }

    let mut gates = Vec::new();
    for required in &task.required_gates {
        gates.push(
            gates_by_name
                .remove(required)
                .unwrap_or_else(|| GateRecord {
                    name: required.clone(),
                    command: String::new(),
                    status: GateStatus::Missing,
                    duration_ms: None,
                    log_path: None,
                    summary: "required gate evidence is missing".to_owned(),
                }),
        );
    }
    gates.extend(gates_by_name.into_values());

    let required_incomplete = gates
        .iter()
        .take(task.required_gates.len())
        .any(|gate| gate.status != GateStatus::Pass);
    let has_failure = gates.iter().any(|gate| gate.status == GateStatus::Fail);
    let verdict = if has_failure {
        Verdict::Fail
    } else if task.status == TaskStatus::Blocked {
        Verdict::Blocked
    } else if required_incomplete || task.status != TaskStatus::Complete {
        Verdict::Incomplete
    } else {
        Verdict::Pass
    };

    let report = ReadbackReport {
        schema_version: PROCESS_SCHEMA_VERSION,
        generated_at: now_rfc3339()?,
        task_id: task.task_id.clone(),
        objective: task.objective.clone(),
        task_status: task.status,
        verdict,
        scope: task.scope.clone(),
        constraints: task.constraints.clone(),
        required_gates: task.required_gates.clone(),
        gates,
        repositories: collect_repositories(root),
        acceptance: task.acceptance.clone(),
        evidence_state: task.evidence_state.clone(),
        direction_decisions: task.direction_decisions.clone(),
        blockers: task.blockers.clone(),
        human_decisions_needed: task.human_decisions_needed.clone(),
        next_iteration: task.next_iteration.clone(),
    };
    validate_readback(&report, "generated readback")?;
    write_json(&output_path.join("readback.json"), &report)?;
    std::fs::write(output_path.join("readback.md"), render_markdown(&report))
        .with_context(|| format!("write readback Markdown under {}", output_path.display()))?;
    Ok(report)
}

fn collect_repositories(root: &Path) -> Vec<RepositoryReadback> {
    let parent = root.parent().unwrap_or(root);
    let integrated = parent.join("external/crosvm").exists()
        || parent.join("hardware/google/gfxstream").exists();
    let workspace = if integrated { parent } else { root };
    let mut specs = vec![
        ("hd", root.to_path_buf()),
        ("external/crosvm", workspace.join("external/crosvm")),
        (
            "hardware/google/gfxstream",
            workspace.join("hardware/google/gfxstream"),
        ),
    ];
    if integrated {
        specs.push(("bscp-root", workspace.to_path_buf()));
    }
    specs
        .into_iter()
        .map(|(name, path)| repository_readback(workspace, name, &path))
        .collect()
}

fn repository_readback(workspace: &Path, name: &str, path: &Path) -> RepositoryReadback {
    let display_path = path
        .strip_prefix(workspace)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf);
    let display_path = if display_path.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        display_path.to_string_lossy().replace('\\', "/")
    };
    let head = git_output(path, &["rev-parse", "--short=12", "HEAD"]);
    let available = head.is_some();
    let status = git_output(path, &["status", "--short", "--untracked-files=all"]);
    let mut changes = status.map_or_else(
        || {
            if available {
                vec!["!! unable to read git status".to_owned()]
            } else {
                Vec::new()
            }
        },
        |status| status.lines().map(ToOwned::to_owned).collect::<Vec<_>>(),
    );
    if changes.len() > MAX_STATUS_LINES {
        let omitted = changes.len() - MAX_STATUS_LINES;
        changes.truncate(MAX_STATUS_LINES);
        changes.push(format!("... {omitted} additional status lines omitted"));
    }
    RepositoryReadback {
        name: name.to_owned(),
        path: display_path,
        available,
        head,
        dirty: !changes.is_empty(),
        changes,
    }
}

fn git_output(path: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[allow(clippy::too_many_lines)]
fn render_markdown(report: &ReadbackReport) -> String {
    let mut text = format!(
        "# AI 回读：{}\n\n- 生成时间：`{}`\n- 任务状态：`{}`\n- 回读结论：`{}`\n- 目标：{}\n\n",
        report.task_id,
        report.generated_at,
        report.task_status.as_str(),
        report.verdict.as_str(),
        report.objective
    );
    text.push_str("## 门禁\n\n| 名称 | 状态 | 耗时 | 证据 |\n|---|---|---:|---|\n");
    for gate in &report.gates {
        let duration = gate
            .duration_ms
            .map_or_else(|| "-".to_owned(), |value| format!("{value} ms"));
        let evidence = gate.log_path.as_deref().unwrap_or("-");
        let _ = writeln!(
            text,
            "| {} | `{}` | {} | {} |",
            markdown_cell(&gate.name),
            gate.status.as_str(),
            duration,
            markdown_cell(evidence)
        );
    }

    text.push_str("\n## 仓库回读\n\n| 仓库 | HEAD | 状态 | 变更数 |\n|---|---|---|---:|\n");
    for repository in &report.repositories {
        let state = if !repository.available {
            "unavailable"
        } else if repository.dirty {
            "dirty"
        } else {
            "clean"
        };
        let _ = writeln!(
            text,
            "| {} | `{}` | `{}` | {} |",
            markdown_cell(&repository.name),
            repository.head.as_deref().unwrap_or("-"),
            state,
            repository.changes.len()
        );
    }

    text.push_str("\n## 证据分层\n\n");
    let _ = writeln!(
        text,
        "- 代码存在：{}\n- 测试源码已编写：{}\n- 契约/进程烟测已验证：{}\n- 真实 Guest 已验证：{}",
        yes_no(report.evidence_state.code_present),
        yes_no(report.evidence_state.tests_authored),
        yes_no(report.evidence_state.contract_smoke_verified),
        yes_no(report.evidence_state.real_guest_verified)
    );

    text.push_str("\n## 验收条件\n\n");
    for acceptance in &report.acceptance {
        let _ = writeln!(
            text,
            "- `{}` {}（证据：{}）",
            acceptance.id,
            acceptance.criterion,
            acceptance.evidence.join("、")
        );
    }

    text.push_str("\n## 阻塞与人工决策\n\n");
    if report.blockers.is_empty() && report.human_decisions_needed.is_empty() {
        text.push_str("- 无。\n");
    } else {
        for blocker in &report.blockers {
            let _ = writeln!(
                text,
                "- 阻塞 `{}`：{}（{}/{}）",
                blocker.id,
                blocker.description,
                blocker.scope.as_str(),
                blocker.state.as_str()
            );
        }
        for decision in &report.human_decisions_needed {
            let _ = writeln!(
                text,
                "- 决策 `{}`：{}；影响：{}（{}）",
                decision.id,
                decision.question,
                decision.impact,
                decision.state.as_str()
            );
        }
    }
    let _ = writeln!(text, "\n## 下一迭代\n\n{}", report.next_iteration);
    text
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

const fn yes_no(value: bool) -> &'static str {
    if value { "是" } else { "否" }
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format UTC timestamp")
}

fn resolve_from(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("read JSON {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("decode JSON {}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).context("encode pretty JSON")?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).with_context(|| format!("write JSON {}", path.display()))
}
