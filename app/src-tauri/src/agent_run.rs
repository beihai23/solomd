//! v4.0 pillar 1 — agent run persistence.
//!
//! Every agent run (Panel chat OR Recipe execution — recipes land in P2)
//! materializes a directory under the workspace at:
//!
//! ```text
//! <workspace>/.solomd/agent-runs/<run-id>/
//! ├── meta.json     # status / provider / model / tokens (C1.1)
//! ├── trace.jsonl   # one JSON line per step (C2)
//! └── run.md        # human-readable transcript (C1.2)
//! ```
//!
//! `run-id` format is `YYYYMMDD-HHMMSS-<6-char-hex>` (UTC), per contract C1.
//! We avoid the `chrono` dep — same Howard Hinnant civil-from-days math used
//! by `git_history::format_unix_utc`. Random suffix comes from the existing
//! `rand` crate (already in deps for crypto).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Agent run kind. Matches `meta.json.kind`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Panel,
    Recipe,
}

impl RunKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunKind::Panel => "panel",
            RunKind::Recipe => "recipe",
        }
    }
}

/// Live run handle. Hold one per active run. Cheap to clone (everything
/// non-Send is behind a `Mutex`).
#[derive(Debug)]
pub struct RunHandle {
    pub run_id: String,
    pub dir: PathBuf,
    pub workspace: PathBuf,
    pub kind: RunKind,
    pub provider: String,
    pub model: String,
    started_at: u64,
    seq: Mutex<u64>,
    /// Fully open trace.jsonl writer — append-mode.
    trace_file: Mutex<Option<fs::File>>,
    /// run.md writer — append-mode.
    run_md_file: Mutex<Option<fs::File>>,
}

/// Trace step structure used by `append_trace`. The `kind` enum mirrors C2.
/// All fields except `kind` are optional — caller fills in only what's
/// relevant for that step kind. We tack on `ts`/`run_id`/`seq` automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceStep {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages_n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd_estimate: Option<f64>,
}

/// Hard cap from C2: result strings written to trace.jsonl are clipped to
/// this many CHARACTERS (not bytes). Larger payloads keep the first N + the
/// `…(truncated)` sentinel and set `truncated: true`.
pub const TRACE_RESULT_CHAR_CAP: usize = 2048;

// ---------------------------------------------------------------------------
// run_id minting
// ---------------------------------------------------------------------------

fn unix_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a unix timestamp as `YYYYMMDD-HHMMSS` in UTC. Same Howard Hinnant
/// civil-from-days math as `git_history::format_unix_utc`.
fn format_run_ts(secs: u64) -> String {
    let secs = secs as i64;
    let days = secs.div_euclid(86_400);
    let mut sod = secs.rem_euclid(86_400);
    let h = sod / 3600;
    sod %= 3600;
    let m = sod / 60;
    let s = sod % 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        y, mo, d, h, m, s
    )
}

/// `YYYY-MM-DDTHH:MM:SSZ` ISO-8601 in UTC for `run.md` front matter.
pub fn format_run_iso(secs: u64) -> String {
    let secs = secs as i64;
    let days = secs.div_euclid(86_400);
    let mut sod = secs.rem_euclid(86_400);
    let h = sod / 3600;
    sod %= 3600;
    let m = sod / 60;
    let s = sod % 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, mo, d, h, m, s
    )
}

fn rand_hex6() -> String {
    let mut buf = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut buf);
    format!("{:02x}{:02x}{:02x}", buf[0], buf[1], buf[2])
}

/// Mint a new run_id per C1: `YYYYMMDD-HHMMSS-<6-hex>` UTC.
pub fn mint_run_id() -> String {
    format!("{}-{}", format_run_ts(unix_secs_now()), rand_hex6())
}

/// Public helper for callers that need a short tool-call id (C2 `tool_call_id`).
pub fn mint_tool_call_id() -> String {
    let mut buf = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut buf);
    format!("tc_{:02x}{:02x}{:02x}{:02x}", buf[0], buf[1], buf[2], buf[3])
}

// ---------------------------------------------------------------------------
// RunHandle impl
// ---------------------------------------------------------------------------

impl RunHandle {
    /// Mint a run_id, create the run dir, write meta.json with status="running"
    /// and seed run.md with its YAML header. Caller is responsible for
    /// keeping the handle alive until `finish` is called.
    pub fn start(
        workspace: &Path,
        kind: RunKind,
        provider: &str,
        model: &str,
        recipe: Option<Value>,
    ) -> Result<Self, String> {
        let run_id = mint_run_id();
        let started_at = unix_secs_now();
        let runs_root = workspace.join(".solomd").join("agent-runs");
        let dir = runs_root.join(&run_id);
        fs::create_dir_all(&dir).map_err(|e| format!("agent_run mkdir: {e}"))?;

        // meta.json
        let meta = json!({
            "run_id": run_id,
            "kind": kind.as_str(),
            "started_at": started_at,
            "ended_at": serde_json::Value::Null,
            "status": "running",
            "workspace": workspace.to_string_lossy(),
            "provider": provider,
            "model": model,
            "recipe": recipe.unwrap_or(Value::Null),
            "tokens": {"input": 0, "output": 0},
            "cost_usd_estimate": 0.0,
            "error": serde_json::Value::Null,
        });
        fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        )
        .map_err(|e| format!("meta.json write: {e}"))?;

        // run.md — front matter then heading.
        let header = match kind {
            RunKind::Panel => format!(
                "---\nrun_id: {run_id}\nkind: panel\nprovider: {provider}\nmodel: {model}\nstarted_at: {iso}\n---\n\n# Panel chat · {run_id}\n\n",
                iso = format_run_iso(started_at),
            ),
            RunKind::Recipe => format!(
                "---\nrun_id: {run_id}\nkind: recipe\nprovider: {provider}\nmodel: {model}\nstarted_at: {iso}\n---\n\n# Recipe run · {run_id}\n\n",
                iso = format_run_iso(started_at),
            ),
        };
        fs::write(dir.join("run.md"), header).map_err(|e| format!("run.md write: {e}"))?;

        // Open trace.jsonl + run.md handles in append mode for cheap repeated
        // writes. We hold them in Mutex<Option<File>> so finish() can drop.
        let trace_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("trace.jsonl"))
            .map_err(|e| format!("trace.jsonl open: {e}"))?;
        let run_md_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("run.md"))
            .map_err(|e| format!("run.md reopen: {e}"))?;

        let handle = RunHandle {
            run_id: run_id.clone(),
            dir,
            workspace: workspace.to_path_buf(),
            kind,
            provider: provider.to_string(),
            model: model.to_string(),
            started_at,
            seq: Mutex::new(0),
            trace_file: Mutex::new(Some(trace_file)),
            run_md_file: Mutex::new(Some(run_md_file)),
        };

        // Seed first line with `run_started`.
        let mut seed = TraceStep {
            kind: "run_started".to_string(),
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            ..Default::default()
        };
        if matches!(handle.kind, RunKind::Recipe) {
            // Mirror the recipe field if present in meta — caller already
            // supplied it via `recipe` param. We don't re-serialize here;
            // P2 will extend.
            seed.text = Some("recipe".to_string());
        }
        let _ = handle.append_trace(seed);

        Ok(handle)
    }

    fn next_seq(&self) -> u64 {
        let mut g = self.seq.lock().unwrap();
        *g += 1;
        *g
    }

    /// Append one trace.jsonl line. Truncates the `result` field to
    /// `TRACE_RESULT_CHAR_CAP` chars per C2 and stamps `ts`/`run_id`/`seq`.
    pub fn append_trace(&self, mut step: TraceStep) -> Result<(), String> {
        // Apply C2 truncation to `result` if present.
        if let Some(r) = step.result.take() {
            let chars: Vec<char> = r.chars().collect();
            if chars.len() > TRACE_RESULT_CHAR_CAP {
                step.result_bytes = Some(r.len() as u64);
                step.truncated = Some(true);
                let mut out: String = chars.into_iter().take(TRACE_RESULT_CHAR_CAP).collect();
                out.push_str("…(truncated)");
                step.result = Some(out);
            } else {
                if step.result_bytes.is_none() {
                    step.result_bytes = Some(r.len() as u64);
                }
                step.result = Some(r);
            }
        }

        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let seq = self.next_seq();
        // Encode as a flat JSON object — start from the typed step then patch
        // in the auto fields so they always appear.
        let mut v = serde_json::to_value(&step).unwrap_or_else(|_| Value::Object(Default::default()));
        if let Value::Object(map) = &mut v {
            map.insert("ts".to_string(), Value::from(ts_ms));
            map.insert("run_id".to_string(), Value::String(self.run_id.clone()));
            map.insert("seq".to_string(), Value::from(seq));
        }
        let line = serde_json::to_string(&v).map_err(|e| format!("trace serialize: {e}"))?;

        let mut guard = self.trace_file.lock().unwrap();
        if let Some(f) = guard.as_mut() {
            writeln!(f, "{line}").map_err(|e| format!("trace write: {e}"))?;
            let _ = f.flush();
        }
        Ok(())
    }

    /// Append a free-form section to `run.md`. Caller decides headings.
    pub fn append_run_md(&self, section: &str) -> Result<(), String> {
        let mut guard = self.run_md_file.lock().unwrap();
        if let Some(f) = guard.as_mut() {
            f.write_all(section.as_bytes()).map_err(|e| format!("run.md write: {e}"))?;
            if !section.ends_with('\n') {
                let _ = f.write_all(b"\n");
            }
            let _ = f.flush();
        }
        Ok(())
    }

    /// Close the run: drop file handles, write `run_ended` trace step, and
    /// rewrite `meta.json` with final status + token totals + (optional) error.
    pub fn finish(
        &self,
        status: &str,
        tokens_in: u64,
        tokens_out: u64,
        error: Option<String>,
    ) -> Result<(), String> {
        // Final trace line.
        let _ = self.append_trace(TraceStep {
            kind: "run_ended".to_string(),
            status: Some(status.to_string()),
            tokens_in_total: Some(tokens_in),
            tokens_out_total: Some(tokens_out),
            error: error.clone(),
            ..Default::default()
        });

        // Drop file handles so other readers see the final bytes flushed.
        {
            let mut g = self.trace_file.lock().unwrap();
            *g = None;
        }
        {
            let mut g = self.run_md_file.lock().unwrap();
            *g = None;
        }

        // Read existing meta.json, patch, rewrite. We read+write rather than
        // streaming so the file stays valid JSON even if the patch fails
        // halfway through.
        let meta_path = self.dir.join("meta.json");
        let raw = fs::read_to_string(&meta_path).unwrap_or_else(|_| "{}".to_string());
        let mut meta: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        if !matches!(meta, Value::Object(_)) {
            meta = json!({});
        }
        if let Value::Object(map) = &mut meta {
            map.insert("status".to_string(), Value::String(status.to_string()));
            map.insert("ended_at".to_string(), Value::from(unix_secs_now()));
            map.insert(
                "tokens".to_string(),
                json!({"input": tokens_in, "output": tokens_out}),
            );
            if let Some(e) = error {
                map.insert("error".to_string(), Value::String(e));
            }
            // Make sure the started_at carries over even if the file was
            // tampered with somehow.
            map.entry("started_at".to_string())
                .or_insert(Value::from(self.started_at));
        }
        fs::write(meta_path, serde_json::to_vec_pretty(&meta).unwrap_or_default())
            .map_err(|e| format!("meta.json finalize: {e}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Markdown helpers used by the panel runner
// ---------------------------------------------------------------------------

/// Render a "Tool: X { args }" block with a (truncated) result preview for
/// `run.md`. Result is clipped to ~2KB per C1.2 / C2.
pub fn fmt_tool_section(tool: &str, args: &Value, result_preview: &str) -> String {
    let args_pretty = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    let chars: Vec<char> = result_preview.chars().collect();
    let preview: String = if chars.len() > 2048 {
        let mut s: String = chars.into_iter().take(2048).collect();
        s.push_str("…(truncated)");
        s
    } else {
        result_preview.to_string()
    };
    format!(
        "### Tool: {tool} {args_pretty}\n```\n{preview}\n```\n\n",
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn run_id_format_matches_contract() {
        let id = mint_run_id();
        // YYYYMMDD-HHMMSS-XXXXXX = 8 + 1 + 6 + 1 + 6 = 22 chars
        assert_eq!(id.len(), 22, "run_id len: {id}");
        assert_eq!(id.as_bytes()[8], b'-');
        assert_eq!(id.as_bytes()[15], b'-');
    }

    #[test]
    fn start_creates_files_and_finish_updates_meta() {
        let tmp = std::env::temp_dir().join(format!("solomd-test-run-{}", mint_run_id()));
        fs::create_dir_all(&tmp).unwrap();
        let h = RunHandle::start(&tmp, RunKind::Panel, "anthropic", "claude-test", None).unwrap();
        assert!(h.dir.exists());
        assert!(h.dir.join("meta.json").exists());
        assert!(h.dir.join("run.md").exists());
        assert!(h.dir.join("trace.jsonl").exists());

        // Append a couple of trace lines.
        h.append_trace(TraceStep {
            kind: "prompt".to_string(),
            role: Some("user".to_string()),
            content: Some("hi".to_string()),
            ..Default::default()
        })
        .unwrap();
        h.append_run_md("## User\n\nhi\n").unwrap();

        h.finish("ok", 5, 7, None).unwrap();

        let meta_raw = fs::read_to_string(h.dir.join("meta.json")).unwrap();
        let meta: Value = serde_json::from_str(&meta_raw).unwrap();
        assert_eq!(meta["status"], "ok");
        assert_eq!(meta["tokens"]["input"], 5);
        assert_eq!(meta["tokens"]["output"], 7);

        // trace.jsonl should have run_started + prompt + run_ended.
        let trace_raw = fs::read_to_string(h.dir.join("trace.jsonl")).unwrap();
        let lines: Vec<&str> = trace_raw.lines().collect();
        assert!(lines.len() >= 3, "trace lines: {lines:?}");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"], "run_started");
        let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["kind"], "run_ended");
        assert_eq!(last["status"], "ok");

        // Cleanup.
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn append_trace_truncates_large_result() {
        let tmp = std::env::temp_dir().join(format!("solomd-test-trunc-{}", mint_run_id()));
        fs::create_dir_all(&tmp).unwrap();
        let h = RunHandle::start(&tmp, RunKind::Panel, "openai", "gpt-test", None).unwrap();

        let big = "x".repeat(5000);
        h.append_trace(TraceStep {
            kind: "tool_result".to_string(),
            tool_call_id: Some("tc_test".to_string()),
            result: Some(big.clone()),
            ..Default::default()
        })
        .unwrap();
        h.finish("ok", 0, 0, None).unwrap();

        let raw = fs::read_to_string(h.dir.join("trace.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        // find the tool_result line
        let v: Value = lines
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["kind"] == "tool_result")
            .expect("tool_result line");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["result_bytes"], 5000);
        let res = v["result"].as_str().unwrap();
        assert!(res.ends_with("…(truncated)"));
        assert!(res.chars().count() <= TRACE_RESULT_CHAR_CAP + 12);

        let _ = fs::remove_dir_all(&tmp);
    }
}
