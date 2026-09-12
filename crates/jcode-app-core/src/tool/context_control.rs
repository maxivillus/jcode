use super::{Tool, ToolContext, ToolOutput, skill_state};
use crate::context::ContextRevision;
use crate::context_controller::{
    ContextActionKind, ContextController, ContextPruneForecast, ContextPruneKind,
    ContextPruneProjection, ContextPruneSpec,
};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak};

pub(crate) type ContextControllerHandle = Arc<StdMutex<ContextController>>;
pub(crate) type ContextControllerWeak = Weak<StdMutex<ContextController>>;
pub(crate) type ContextControllerBindings = Arc<StdRwLock<HashMap<String, ContextControllerWeak>>>;

/// Сколько точек среза хвоста `preview` показывает в списке видов.
const TAIL_CUT_SAMPLES: usize = 8;

/// Максимальная длина имени файла экспорта.
const EXPORT_NAME_MAX_CHARS: usize = 128;

#[derive(Clone)]
pub(crate) struct ContextTools {
    bindings: ContextControllerBindings,
}

impl ContextTools {
    pub(crate) fn new() -> Self {
        Self {
            bindings: Arc::new(StdRwLock::new(HashMap::new())),
        }
    }

    pub(crate) fn register(&self, tools: &mut HashMap<String, Arc<dyn Tool>>) {
        tools.insert(
            "context_control".into(),
            Arc::new(ContextControlTool::new(self.bindings.clone())),
        );
        tools.insert(
            "skill_state".into(),
            Arc::new(skill_state::SkillStateTool::new(self.bindings.clone())),
        );
    }

    pub(crate) fn bind(&self, session_id: &str, controller: ContextControllerWeak) {
        let Ok(mut bindings) = self.bindings.write() else {
            crate::logging::warn("Context controller bindings lock poisoned");
            return;
        };
        bindings.retain(|_, current| current.strong_count() > 0);
        bindings.insert(session_id.to_string(), controller);
    }
}

pub(crate) fn context_controller_for_session(
    bindings: &ContextControllerBindings,
    session_id: &str,
) -> Result<ContextControllerHandle> {
    let mut bindings = bindings
        .write()
        .map_err(|_| anyhow::anyhow!("context controller bindings lock poisoned"))?;
    bindings.retain(|_, controller| controller.strong_count() > 0);
    bindings
        .get(session_id)
        .and_then(Weak::upgrade)
        .ok_or_else(|| anyhow::anyhow!("context controller is not bound for this session"))
}

/// Read-only model surface for the provider-facing context manifest.
pub(crate) struct ContextControlTool {
    bindings: ContextControllerBindings,
}

impl ContextControlTool {
    pub(crate) fn new(bindings: ContextControllerBindings) -> Self {
        Self { bindings }
    }
}

#[derive(Debug, Deserialize)]
struct ContextControlInput {
    #[serde(default = "default_action")]
    action: String,
    /// Revision, которую модель увидела в status/preview. Требуется для
    /// действий, которые меняют состояние на границе turn-а.
    #[serde(default)]
    expected_revision: Option<u64>,
    /// Параметры структурной обрезки для действия `prune`.
    #[serde(default)]
    prune: Option<ContextPruneInput>,
    /// Параметры записи redacted-манифеста для действия `export`.
    #[serde(default)]
    export: Option<ContextExportInput>,
}

#[derive(Debug, Deserialize)]
struct ContextPruneInput {
    kind: String,
    #[serde(default)]
    keep_recent: Option<usize>,
    /// Последнее сохраняемое сообщение для вида `tail`.
    #[serde(default)]
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContextExportInput {
    /// Имя файла внутри каталога экспорта. Каталогов в имени быть не может.
    #[serde(default)]
    path: Option<String>,
    /// `json` или `markdown`; без него формат берётся из расширения.
    #[serde(default)]
    format: Option<String>,
    /// Заменить уже существующий файл. Без этого флага запись отклоняется.
    #[serde(default)]
    overwrite: bool,
}

fn default_action() -> String {
    "status".to_string()
}

fn context_metadata(controller: &ContextController) -> Value {
    let manifest = controller.manifest();
    json!({
        "schema_version": manifest.schema_version,
        "revision": manifest.revision,
        "provider_generation": manifest.provider_generation,
        "estimated_input_tokens": manifest.estimated_input_tokens,
        "observed_input_tokens": manifest.observed_input_tokens,
        "components": &manifest.components,
        "fingerprint": manifest.fingerprint(),
    })
}

fn preflight_metadata(controller: &ContextController) -> Value {
    match (controller.last_budget(), controller.last_plan()) {
        (Some(budget), Some(plan)) => json!({
            "status": "available",
            "budget": budget,
            "plan": plan,
            "needs_refresh": plan.needs_refresh(),
            "needs_compaction": plan.needs_compaction(),
        }),
        _ => json!({
            "status": "not_available",
            "reason": "no provider preflight has run",
        }),
    }
}

fn execution_state_metadata(controller: &ContextController) -> Value {
    let state = controller.execution_state();
    json!({
        "state_schema": state.state_schema,
        "schema_version": state.schema_version,
        "revision": state.revision,
    })
}

fn actions_metadata(controller: &ContextController) -> Value {
    json!({
        "pending": controller.pending_actions(),
        "last": controller.last_action(),
    })
}

/// Формат файла экспорта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Json,
    Markdown,
}

impl ExportFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }
}

/// Каталог, в который `export` пишет файлы: `<jcode home>/exports`, режим 0700.
fn export_dir() -> Result<PathBuf> {
    let dir = jcode_base::storage::jcode_dir()
        .map_err(|error| anyhow::anyhow!("cannot resolve the jcode home directory: {error}"))?
        .join("exports");
    std::fs::create_dir_all(&dir)
        .map_err(|error| anyhow::anyhow!("cannot create the exports directory: {error}"))?;
    jcode_core::fs::set_directory_permissions_owner_only(&dir)
        .map_err(|error| anyhow::anyhow!("cannot restrict the exports directory: {error}"))?;
    Ok(dir)
}

/// Проверяет имя файла и собирает путь внутри каталога экспорта.
fn export_file_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        bail!("export.path must not be empty");
    }
    if name.chars().count() > EXPORT_NAME_MAX_CHARS {
        bail!("export.path is too long (max {EXPORT_NAME_MAX_CHARS} characters)");
    }
    if name.starts_with('.') {
        bail!("export.path must not start with a dot");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("export.path must be a file name inside the exports directory, not a path");
    }
    let path = dir.join(name);
    if path.parent() != Some(dir) {
        bail!("export.path must stay inside the exports directory");
    }
    Ok(path)
}

/// Формат из параметра или из расширения имени файла.
fn export_format(name: &str, requested: Option<&str>) -> Result<ExportFormat> {
    match requested.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if value == "json" => Ok(ExportFormat::Json),
        Some(value) if value == "markdown" || value == "md" => Ok(ExportFormat::Markdown),
        Some(other) => bail!("unsupported export format: {other}; use json or markdown"),
        None => {
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".json") {
                Ok(ExportFormat::Json)
            } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
                Ok(ExportFormat::Markdown)
            } else {
                bail!("export.path needs a .json or .md extension, or pass export.format");
            }
        }
    }
}

/// Redacted-манифест для записи: только метрики, хеши и счётчики.
///
/// Документ собирается из полей манифеста, поэтому в него не попадают текст
/// сообщений, результаты инструментов, секреты и env.
fn export_document(controller: &ContextController, session_id: &str) -> Value {
    json!({
        "export_version": 1,
        "redacted": true,
        "note": "redacted manifest: metrics, hashes and counters only",
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "session_id": session_id,
        "context": context_metadata(controller),
        "preflight": preflight_metadata(controller),
        "execution_state": execution_state_metadata(controller),
        "actions": actions_metadata(controller),
        "prune": controller
            .prune_projections()
            .iter()
            .map(|projection| json!({
                "kind": projection.kind,
                "levels": projection.levels.len() + projection.overflow_levels,
                "items": projection.levels.iter().map(|level| level.items).sum::<usize>()
                    + projection.overflow_items,
                "tokens": projection.total_tokens,
                "source_messages": projection.source_messages,
            }))
            .collect::<Vec<Value>>(),
    })
}

/// Значение для строки markdown: строки без кавычек, остальное как в JSON.
fn inline_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Рендер документа в markdown.
///
/// Глубина ограничена: глубже значения выводятся одной строкой, чтобы файл не
/// разрастался на вложенных структурах.
fn render_markdown(value: &Value, out: &mut String, level: usize) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if (item.is_object() || item.is_array()) && level < 3 {
                    out.push_str(&format!("\n{} {key}\n\n", "#".repeat(level + 2)));
                    render_markdown(item, out, level + 1);
                } else {
                    out.push_str(&format!("- {key}: {}\n", inline_json(item)));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.is_object() || item.is_array() {
                    render_markdown(item, out, level + 1);
                } else {
                    out.push_str(&format!("- {}\n", inline_json(item)));
                }
            }
        }
        other => out.push_str(&format!("{}\n", inline_json(other))),
    }
}

fn export_markdown(document: &Value) -> String {
    let mut out = String::from("# Context export\n");
    render_markdown(document, &mut out, 1);
    out
}

/// Атомарная запись с правами 0600: сначала временный файл, затем rename.
///
/// Перезапись разрешена только явным `export.overwrite`, поэтому существование
/// файла проверяется до записи.
fn write_export_file(path: &Path, contents: &str, overwrite: bool) -> Result<()> {
    if path.exists() && !overwrite {
        bail!(
            "export file {} already exists; pass export.overwrite=true to replace it",
            path.display()
        );
    }
    let Some(dir) = path.parent() else {
        bail!("export path has no parent directory");
    };
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("context-export");
    let temporary = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, contents)
        .map_err(|error| anyhow::anyhow!("cannot write {}: {error}", temporary.display()))?;
    let result = jcode_core::fs::set_permissions_owner_only(&temporary)
        .map_err(|error| anyhow::anyhow!("cannot restrict {}: {error}", temporary.display()))
        .and_then(|()| {
            std::fs::rename(&temporary, path)
                .map_err(|error| anyhow::anyhow!("cannot publish {}: {error}", path.display()))
        });
    if result.is_err()
        && let Err(cleanup) = std::fs::remove_file(&temporary)
    {
        crate::logging::warn(&format!(
            "cannot remove temporary export {}: {cleanup}",
            temporary.display()
        ));
    }
    result
}

/// Ответ `export` без файла: только redacted-манифест.
fn export_manifest_only_metadata(controller: &ContextController) -> Value {
    json!({
        "action": "export",
        "mutated": false,
        "writes_file": false,
        "redacted": true,
        "note": "the manifest is returned here; pass export.path to write it to a file",
        "manifest": context_metadata(controller),
        "execution_state": execution_state_metadata(controller),
        "actions": actions_metadata(controller),
    })
}

/// Действие `export`: redacted-манифест в ответе и, по запросу, в файле.
fn export_action_metadata(
    controller: &ContextController,
    ctx: &ToolContext,
    input: Option<&ContextExportInput>,
) -> Result<Value> {
    let Some(input) = input else {
        return Ok(export_manifest_only_metadata(controller));
    };
    let Some(name) = input.path.as_deref() else {
        return Ok(export_manifest_only_metadata(controller));
    };
    let dir = export_dir()?;
    let path = export_file_path(&dir, name)?;
    let format = export_format(name, input.format.as_deref())?;
    let document = export_document(controller, &ctx.session_id);
    let contents = match format {
        ExportFormat::Json => format!(
            "{}\n",
            serde_json::to_string_pretty(&document)
                .map_err(|error| anyhow::anyhow!("cannot serialize the export: {error}"))?
        ),
        ExportFormat::Markdown => export_markdown(&document),
    };
    let existed = path.exists();
    write_export_file(&path, &contents, input.overwrite)?;
    Ok(json!({
        "action": "export",
        "mutated": false,
        "writes_file": true,
        "redacted": true,
        "file": {
            "path": path.display().to_string(),
            "format": format.as_str(),
            "bytes": contents.len(),
            "overwritten": existed,
            "permissions": "0600",
            "external_storage": true,
            "note": "external storage: the file is never loaded back into context automatically",
        },
        "manifest": context_metadata(controller),
        "execution_state": execution_state_metadata(controller),
        "actions": actions_metadata(controller),
    }))
}

/// Разбор параметров обрезки из аргументов инструмента.
fn parse_prune_spec(input: &ContextPruneInput) -> Result<ContextPruneSpec> {
    let kind = match input.kind.as_str() {
        "images" => ContextPruneKind::Images,
        "memory-injections" => ContextPruneKind::MemoryInjections,
        "system-reminders" => ContextPruneKind::SystemReminders,
        "tool-results" => ContextPruneKind::ToolResults,
        "turns" => ContextPruneKind::Turns,
        "tail" => ContextPruneKind::Tail,
        other => anyhow::bail!("unsupported prune kind: {other}"),
    };
    if input.kind == "tail" {
        let after = input.after.as_deref().unwrap_or("").trim();
        if after.is_empty() {
            anyhow::bail!(
                "`tail` requires `after`: the id of the last message to keep; see preview"
            );
        }
        if input.keep_recent.is_some() {
            anyhow::bail!("`keep_recent` is not supported by `tail`; pass `after` instead");
        }
        return Ok(ContextPruneSpec::new(kind).after(after));
    }
    let mut spec = ContextPruneSpec::new(kind);
    if let Some(keep_recent) = input.keep_recent {
        spec = spec.keep_recent(keep_recent);
    }
    Ok(spec)
}

/// Расчёт обрезки в виде, который читает модель.
fn forecast_metadata(forecast: &ContextPruneForecast) -> Value {
    let mut value = json!(forecast);
    if let Some(object) = value.as_object_mut() {
        object.insert("no_op".to_string(), json!(forecast.is_no_op()));
    }
    value
}

/// Что произойдёт со сжатием: `preview` показывает его триггер, а не
/// прогноз размера, потому что результат измеряется после запуска.
fn compaction_projection(controller: &ContextController) -> Value {
    match controller.last_plan() {
        Some(plan) => json!({
            "needs_compaction": plan.needs_compaction(),
            "estimated_input_tokens": plan.estimated_input_tokens,
            "max_input_tokens": plan.max_input_tokens,
            "projected_after_tokens": Value::Null,
            "projected_after_reason": "compaction is measured after it runs",
        }),
        None => json!({
            "needs_compaction": Value::Null,
            "reason": "no provider preflight has run yet",
        }),
    }
}

/// Список точек среза хвоста: у вида `tail` нет прогноза по `keep_recent`.
fn tail_projection_metadata(projection: &ContextPruneProjection) -> Value {
    json!({
        "kind": projection.kind,
        "selector": "after",
        "candidates": projection.levels.len() + projection.overflow_levels,
        "sample_after": projection.tail_cut_samples(TAIL_CUT_SAMPLES),
        "note": "pass prune.after with one of these message ids: it and everything before it are kept",
    })
}

/// Проекция обрезки: что уйдёт, что останется и сколько это освободит.
///
/// Числа берутся из снимка с границы turn-а, поэтому операция ничего не
/// меняет, не пишет файлы и не ставит заявку в очередь.
fn projection_metadata(
    controller: &ContextController,
    requested: Option<ContextPruneSpec>,
) -> Value {
    let Some(revision) = controller.prune_projection_revision() else {
        return json!({
            "status": "not_available",
            "reason": "no turn-boundary snapshot yet; it is recorded before the next provider request",
        });
    };
    let prune: Vec<Value> = controller
        .prune_projections()
        .iter()
        .map(|projection| {
            if projection.kind == ContextPruneKind::Tail {
                return tail_projection_metadata(projection);
            }
            json!({
                "kind": projection.kind,
                "default_keep_recent": projection.kind.default_keep_recent(),
                "total_items": projection.levels.iter().map(|level| level.items).sum::<usize>()
                    + projection.overflow_items,
                "forecast": forecast_metadata(
                    &projection.forecast(projection.kind.default_keep_recent())
                ),
            })
        })
        .collect();
    let requested = requested.map(|spec| {
        let kind = spec.kind;
        match controller.prune_forecast(spec) {
            Some(forecast) => json!({
                "status": if forecast.is_no_op() { "no_op" } else { "available" },
                "queues_nothing": true,
                "forecast": forecast_metadata(&forecast),
            }),
            None => json!({
                "status": "not_available",
                "reason": if kind == ContextPruneKind::Tail {
                    "the snapshot has no safe cut after this message id; pick one from preview.sample_after"
                } else {
                    "the snapshot does not cover this prune kind"
                },
            }),
        }
    });
    json!({
        "status": "available",
        "snapshot_revision": revision.0,
        "stale": controller.prune_projection_stale(),
        "stale_note": "stale=true means the transcript changed after the snapshot",
        "estimated_context_tokens": controller
            .prune_projections()
            .first()
            .map(|projection| projection.total_tokens),
        "compaction": compaction_projection(controller),
        "prune": prune,
        "requested": requested,
    })
}

fn queued_action_metadata(
    action: &str,
    controller: &ContextController,
    request: crate::context_controller::ContextActionRequest,
) -> Value {
    json!({
        "action": action,
        "mutated": false,
        "queued": true,
        "request": request,
        "current_revision": controller.manifest().revision,
        "next_step": "runtime applies queued actions on the next safe turn boundary",
        "actions": actions_metadata(controller),
    })
}

fn output(title: &str, metadata: Value) -> ToolOutput {
    let body = serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| metadata.to_string());
    ToolOutput::new(body)
        .with_title(title.to_string())
        .with_metadata(metadata)
}

#[async_trait]
impl Tool for ContextControlTool {
    fn name(&self) -> &str {
        "context_control"
    }

    fn description(&self) -> &str {
        "Read-only context status, projections and queued context actions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["status", "preview", "refresh", "compact", "reset-provider", "export", "prune", "undo-prune"],
                    "description": "Operation to perform; preview projects, queued actions apply next turn.",
                },
                "expected_revision": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Revision from status/preview; required for queued actions.",
                },
                "prune": {
                    "type": "object",
                    "description": "Prune parameters for prune and for a preview dry-run.",
                    "required": ["kind"],
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["images", "memory-injections", "system-reminders", "tool-results", "turns", "tail"],
                            "description": "Structural data to prune.",
                        },
                        "keep_recent": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "How many recent items to keep; not used by tail.",
                        },
                        "after": {
                            "type": "string",
                            "description": "tail only: id of the last message to keep, one of preview.sample_after; everything after it is removed.",
                        },
                    },
                    "additionalProperties": false,
                },
                "export": {
                    "type": "object",
                    "description": "Export parameters; without them the redacted manifest is only returned in the response.",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File name inside the exports directory, e.g. context.json or context.md. Directories are not allowed.",
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "markdown"],
                            "description": "File format; defaults to the file extension.",
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "Replace an existing file; without it an existing file is refused.",
                        },
                    },
                    "additionalProperties": false,
                },
            },
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: ContextControlInput = serde_json::from_value(input)?;
        let controller = context_controller_for_session(&self.bindings, &ctx.session_id)?;
        let mut controller = controller
            .lock()
            .map_err(|_| anyhow::anyhow!("context controller lock poisoned"))?;

        let action = params.action.as_str();
        // `preview` принимает spec обрезки как dry-run: заявка не ставится.
        let requested_prune = match (&params.prune, action) {
            (Some(input), "preview") => Some(parse_prune_spec(input)?),
            _ => None,
        };
        let metadata = match action {
            "status" => json!({
                "action": action,
                "mutated": false,
                "context": context_metadata(&controller),
                "preflight": preflight_metadata(&controller),
                "actions": actions_metadata(&controller),
            }),
            "preview" => json!({
                "action": action,
                "mutated": false,
                "queues_nothing": true,
                "context": context_metadata(&controller),
                "preflight": preflight_metadata(&controller),
                "projection": projection_metadata(&controller, requested_prune),
                "actions": actions_metadata(&controller),
            }),
            "refresh" | "compact" | "reset-provider" => {
                let expected = params.expected_revision.ok_or_else(|| {
                    anyhow::anyhow!(
                        "expected_revision is required for `{action}`; read status first"
                    )
                })?;
                let kind = match action {
                    "refresh" => ContextActionKind::Refresh,
                    "compact" => ContextActionKind::Compact,
                    _ => ContextActionKind::ResetProvider,
                };
                let request = controller
                    .request_action(kind, ContextRevision(expected))
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                queued_action_metadata(action, &controller, request)
            }
            "export" => export_action_metadata(&controller, &ctx, params.export.as_ref())?,
            "prune" | "undo-prune" => {
                let expected = params.expected_revision.ok_or_else(|| {
                    anyhow::anyhow!(
                        "expected_revision is required for `{action}`; read status first"
                    )
                })?;
                let request = if action == "prune" {
                    let input = params.prune.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "prune spec is required for `prune`; pass kind images|memory-injections|system-reminders|tool-results|turns|tail"
                        )
                    })?;
                    let spec = parse_prune_spec(input)?;
                    controller
                        .request_prune(spec, ContextRevision(expected))
                        .map_err(|error| anyhow::anyhow!("{error}"))?
                } else {
                    controller
                        .request_action(ContextActionKind::UndoPrune, ContextRevision(expected))
                        .map_err(|error| anyhow::anyhow!("{error}"))?
                };
                queued_action_metadata(action, &controller, request)
            }
            _ => anyhow::bail!("unsupported context_control action: {action}"),
        };

        Ok(output("context_control", metadata))
    }
}

#[cfg(test)]
#[path = "context_control_tests.rs"]
mod tests;
