//! Parses BuildKit `--progress=rawjson` lines into a one-line build step for
//! `status.conditions[Building].message` and pod Events.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize)]
struct SolveStatus {
    #[serde(default)]
    vertexes: Vec<Vertex>,
}

#[derive(Deserialize)]
struct Vertex {
    digest: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    started: Option<String>,
    #[serde(default)]
    completed: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct VertexState {
    name: String,
    started: bool,
    completed: bool,
    order: usize,
}

/// Accumulates vertex state across log lines.
#[derive(Default)]
pub struct Progress {
    vertices: HashMap<String, VertexState>,
    next_order: usize,
    error: Option<String>,
}

impl Progress {
    /// Feeds one log line; non-JSON lines are ignored. Returns true if the step changed.
    pub fn feed(&mut self, line: &str) -> bool {
        let before = self.current();
        let Ok(status) = serde_json::from_str::<SolveStatus>(line.trim()) else {
            return false;
        };
        for v in status.vertexes {
            let order = self.next_order;
            let entry = self
                .vertices
                .entry(v.digest)
                .or_insert_with(|| VertexState {
                    name: String::new(),
                    started: false,
                    completed: false,
                    order,
                });
            if entry.order == order {
                self.next_order += 1;
            }
            if !v.name.is_empty() {
                entry.name = v.name;
            }
            entry.started |= v.started.is_some();
            entry.completed |= v.completed.is_some();
            if let Some(e) = v.error.filter(|e| !e.is_empty()) {
                self.error = Some(format!("{}: {e}", humanize(&entry.name)));
            }
        }
        self.current() != before
    }

    /// The newest started, unfinished step, humanized; e.g. `step 4/12 RUN npm ci`.
    pub fn current(&self) -> Option<String> {
        self.vertices
            .values()
            .filter(|v| v.started && !v.completed && !v.name.is_empty())
            .max_by_key(|v| v.order)
            .map(|v| humanize(&v.name))
    }

    /// First error a vertex reported, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// `[stage-0 4/12] RUN npm ci` → `step 4/12 RUN npm ci`;
/// `[internal] load metadata for docker.io/library/node:22` → `load metadata for ...`.
pub fn humanize(name: &str) -> String {
    let name = name.trim();
    if let Some(rest) = name.strip_prefix('[')
        && let Some((bracket, tail)) = rest.split_once(']')
    {
        let tail = tail.trim();
        if bracket == "internal" {
            return tail.to_string();
        }
        if let Some(step) = bracket.split_whitespace().last()
            && step.contains('/')
            && step
                .split('/')
                .all(|n| n.chars().all(|c| c.is_ascii_digit()))
        {
            return truncate(&format!("step {step} {tail}"), 120);
        }
        return truncate(tail, 120);
    }
    truncate(name, 120)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes_names() {
        assert_eq!(
            humanize("[stage-0 4/12] RUN npm ci"),
            "step 4/12 RUN npm ci"
        );
        assert_eq!(humanize("[2/5] COPY . ."), "step 2/5 COPY . .");
        assert_eq!(
            humanize("[internal] load build definition from Dockerfile"),
            "load build definition from Dockerfile"
        );
        assert_eq!(humanize("exporting to image"), "exporting to image");
        assert_eq!(
            humanize("[builder 1/3] FROM docker.io/library/rust"),
            "step 1/3 FROM docker.io/library/rust"
        );
    }

    #[test]
    fn tracks_newest_running_vertex() {
        let mut p = Progress::default();
        assert!(
            p.feed(r#"{"vertexes":[{"digest":"a","name":"[1/3] FROM alpine","started":"t"}]}"#)
        );
        assert_eq!(p.current().as_deref(), Some("step 1/3 FROM alpine"));
        p.feed(r#"{"vertexes":[{"digest":"a","name":"[1/3] FROM alpine","started":"t","completed":"t"},{"digest":"b","name":"[2/3] RUN npm ci","started":"t"}]}"#);
        assert_eq!(p.current().as_deref(), Some("step 2/3 RUN npm ci"));
        assert!(!p.feed("not json"));
        p.feed(r#"{"vertexes":[{"digest":"b","name":"[2/3] RUN npm ci","started":"t","completed":"t","error":"exit code: 1"}]}"#);
        assert_eq!(p.current(), None);
        assert_eq!(p.error(), Some("step 2/3 RUN npm ci: exit code: 1"));
    }
}
