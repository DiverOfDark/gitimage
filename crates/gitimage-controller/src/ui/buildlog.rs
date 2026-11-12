//! Turns BuildKit `--progress=rawjson` lines into UI events: the steps (vertexes) with their
//! state, and the output of each step. Lines that are not JSON (the final `error: failed to
//! solve`, daemon messages) pass through as plain text.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::progress::humanize;

#[derive(Deserialize)]
struct SolveStatus {
    #[serde(default)]
    vertexes: Vec<RawVertex>,
    #[serde(default)]
    logs: Vec<RawLog>,
}

#[derive(Deserialize)]
struct RawVertex {
    digest: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    started: Option<String>,
    #[serde(default)]
    completed: Option<String>,
    #[serde(default)]
    cached: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct RawLog {
    vertex: String,
    #[serde(default)]
    data: String,
}

/// One build step as the UI shows it.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Vertex digest; log chunks refer to it.
    pub id: String,
    /// `step 4/12 RUN npm ci`
    pub title: String,
    /// BuildKit's own bookkeeping (`[internal] load build definition`).
    pub internal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<String>,
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Output of one step; `text` may end in the middle of a line.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    pub step: String,
    pub text: String,
}

/// What one log line of the BuildKit container means.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Build { steps: Vec<Step>, logs: Vec<Chunk> },
    Text(String),
    Nothing,
}

pub fn parse_line(line: &str) -> Parsed {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Parsed::Nothing;
    }
    let Ok(status) = serde_json::from_str::<SolveStatus>(trimmed) else {
        return Parsed::Text(trimmed.to_string());
    };
    let steps: Vec<Step> = status
        .vertexes
        .into_iter()
        .filter(|v| !v.name.is_empty())
        .map(|v| Step {
            internal: v.name.trim_start().starts_with("[internal]"),
            title: humanize(&v.name),
            id: v.digest,
            started: v.started,
            completed: v.completed,
            cached: v.cached,
            error: v.error.filter(|e| !e.is_empty()),
        })
        .collect();
    let logs: Vec<Chunk> = status
        .logs
        .into_iter()
        .filter_map(|l| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(l.data.as_bytes())
                .unwrap_or_else(|_| l.data.clone().into_bytes());
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (!text.is_empty()).then_some(Chunk {
                step: l.vertex,
                text,
            })
        })
        .collect();
    if steps.is_empty() && logs.is_empty() {
        Parsed::Nothing
    } else {
        Parsed::Build { steps, logs }
    }
}

/// `gitimage clone` logs JSON lines (fields flattened, or nested under `fields`); show
/// `LEVEL message key=value`.
pub fn prettify_clone_line(line: &str) -> String {
    let trimmed = line.trim();
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(trimmed)
    else {
        return trimmed.to_string();
    };
    let fields = obj
        .get("fields")
        .and_then(|v| v.as_object())
        .unwrap_or(&obj);
    let Some(message) = fields.get("message").and_then(|v| v.as_str()) else {
        return trimmed.to_string();
    };
    let level = obj.get("level").and_then(|v| v.as_str()).unwrap_or("INFO");
    let mut out = format!("{level} {message}");
    for (k, v) in fields {
        if matches!(
            k.as_str(),
            "message" | "timestamp" | "level" | "target" | "fields"
        ) {
            continue;
        }
        let v = v
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string());
        out.push_str(&format!(" {k}={v}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steps_and_output() {
        let line = r#"{"vertexes":[{"digest":"sha256:a","name":"[2/3] RUN npm ci","started":"2026-01-01T00:00:00Z"}],"logs":[{"vertex":"sha256:a","stream":1,"data":"YWRkZWQgMTIgcGFja2FnZXMK","timestamp":"2026-01-01T00:00:01Z"}]}"#;
        let Parsed::Build { steps, logs } = parse_line(line) else {
            panic!("not a build line");
        };
        assert_eq!(steps[0].title, "step 2/3 RUN npm ci");
        assert!(!steps[0].internal && steps[0].completed.is_none());
        assert_eq!(logs[0].step, "sha256:a");
        assert_eq!(logs[0].text, "added 12 packages\n");
    }

    #[test]
    fn marks_internal_cached_and_failed_steps() {
        let line = r#"{"vertexes":[{"digest":"i","name":"[internal] load build definition from Dockerfile","started":"t","completed":"t"},{"digest":"c","name":"[1/2] FROM alpine","cached":true,"started":"t","completed":"t"},{"digest":"f","name":"[2/2] RUN false","started":"t","completed":"t","error":"exit code: 1"}]}"#;
        let Parsed::Build { steps, .. } = parse_line(line) else {
            panic!("not a build line");
        };
        assert!(steps[0].internal);
        assert!(steps[1].cached);
        assert_eq!(steps[2].error.as_deref(), Some("exit code: 1"));
    }

    #[test]
    fn passes_plain_text_through() {
        assert_eq!(
            parse_line("error: failed to solve: exit code: 1\n"),
            Parsed::Text("error: failed to solve: exit code: 1".into())
        );
        assert_eq!(parse_line("  "), Parsed::Nothing);
        assert_eq!(parse_line(r#"{"statuses":[{"id":"x"}]}"#), Parsed::Nothing);
    }

    #[test]
    fn prettifies_clone_logs() {
        let line = r#"{"timestamp":"t","level":"INFO","fields":{"message":"checked out","sha":"abc"},"target":"gitimage_git"}"#;
        assert_eq!(prettify_clone_line(line), "INFO checked out sha=abc");
        let flat = r#"{"timestamp":"t","level":"INFO","message":"cloned a6f88d9","sha":"7cd144d","context":".","target":"gitimage_git::cli"}"#;
        assert_eq!(
            prettify_clone_line(flat),
            "INFO cloned a6f88d9 context=. sha=7cd144d"
        );
        assert_eq!(prettify_clone_line("plain"), "plain");
        assert_eq!(prettify_clone_line(r#"{"a":1}"#), r#"{"a":1}"#);
    }
}
