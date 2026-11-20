//! Parses BuildKit `--progress=rawjson` lines into a one-line build step for
//! `status.conditions[Building].message` and pod Events.

use std::collections::HashMap;

use base64::Engine as _;
use serde::Deserialize;

#[derive(Deserialize)]
struct SolveStatus {
    #[serde(default)]
    vertexes: Vec<Vertex>,
    #[serde(default)]
    logs: Vec<VertexLog>,
}

#[derive(Deserialize)]
struct VertexLog {
    vertex: String,
    #[serde(default)]
    data: String,
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
    /// When this vertex was first seen started, relative to the others.
    start_seq: Option<usize>,
    /// Last non-empty line the step printed.
    last_line: Option<String>,
}

/// Accumulates vertex state across log lines.
#[derive(Default)]
pub struct Progress {
    vertices: HashMap<String, VertexState>,
    next_order: usize,
    next_start: usize,
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
                    start_seq: None,
                    last_line: None,
                });
            if entry.order == order {
                self.next_order += 1;
            }
            if !v.name.is_empty() {
                entry.name = v.name;
            }
            entry.started |= v.started.is_some();
            if entry.started && entry.start_seq.is_none() {
                entry.start_seq = Some(self.next_start);
                self.next_start += 1;
            }
            entry.completed |= v.completed.is_some();
            // A missing cache to import (every first build) is reported as a vertex error,
            // but BuildKit goes on without it.
            if let Some(e) = v.error.filter(|e| !e.is_empty())
                && !entry.name.starts_with("importing cache manifest")
            {
                self.error = Some(format!("{}: {e}", humanize(&entry.name)));
            }
        }
        for l in status.logs {
            let Some(entry) = self.vertices.get_mut(&l.vertex) else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(l.data.as_bytes())
            else {
                continue;
            };
            if let Some(line) = String::from_utf8_lossy(&bytes)
                .split(['\n', '\r'])
                .map(str::trim)
                .rfind(|l| !l.is_empty())
            {
                entry.last_line = Some(truncate(line, 300));
            }
        }
        self.current() != before
    }

    /// The build's progress as one line, e.g. `step 20/42 android-builder (+1 parallel): RUN …`.
    ///
    /// BuildKit numbers steps per stage (`[backend 3/9]`), and stages run in parallel, so one
    /// stage's counter says little about the build. BuildKit announces every step of every
    /// stage when the build starts, so the count here is over all of them: steps finished
    /// (cached included) + 1, of all numbered steps. The step named is the one running
    /// longest, which is what the build is waiting on and does not flip between parallel
    /// stages on every line. Unnumbered work (`load metadata`, `exporting to image`) is shown
    /// without a count when no numbered step runs.
    pub fn current(&self) -> Option<String> {
        let numbered: Vec<(&VertexState, (&str, &str))> = self
            .vertices
            .values()
            .filter_map(|v| numbered_step(&v.name).map(|s| (v, s)))
            .collect();
        let total = numbered.len();
        let done = numbered.iter().filter(|(v, _)| v.completed).count();
        let mut running: Vec<_> = numbered
            .iter()
            .filter(|(v, _)| v.started && !v.completed)
            .collect();
        running.sort_by_key(|(v, _)| v.start_seq);
        if let Some((_, (stage, cmd))) = running.first() {
            let parallel = match running.len() - 1 {
                0 => String::new(),
                k => format!(" (+{k} parallel)"),
            };
            let label = if stage.is_empty() {
                parallel.trim_start().to_string()
            } else {
                format!("{stage}{parallel}")
            };
            let head = if label.is_empty() {
                String::new()
            } else {
                format!("{label}: ")
            };
            let n = (done + 1).min(total);
            return Some(truncate(&format!("step {n}/{total} {head}{cmd}"), 120));
        }
        self.vertices
            .values()
            .filter(|v| v.started && !v.completed && !v.name.is_empty())
            .max_by_key(|v| v.start_seq)
            .map(|v| humanize(&v.name))
    }

    /// Last error a vertex reported, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Why the build failed, as far as its output says: the error a step reported, or, when
    /// BuildKit stopped without one, the last line printed by the longest-running unfinished
    /// step.
    pub fn failure(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(e.clone());
        }
        self.vertices
            .values()
            .filter(|v| v.started && !v.completed && !v.name.is_empty())
            .filter_map(|v| v.last_line.as_ref().map(|l| (v, l)))
            .min_by_key(|(v, _)| v.start_seq)
            .map(|(v, l)| format!("{}: {l}", humanize(&v.name)))
    }
}

/// `[builder 4/12] RUN npm ci` → `("builder", "RUN npm ci")`; `[2/5] COPY . .` → `("", "COPY . .")`.
/// `None` for vertices that are not a numbered Dockerfile step.
fn numbered_step(name: &str) -> Option<(&str, &str)> {
    let rest = name.trim().strip_prefix('[')?;
    let (bracket, tail) = rest.split_once(']')?;
    let (stage, step) = match bracket.rsplit_once(' ') {
        Some((stage, step)) => (stage.trim(), step),
        None => ("", bracket),
    };
    let (n, of) = step.split_once('/')?;
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    (digits(n) && digits(of)).then_some((stage, tail.trim()))
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
        assert_eq!(p.current().as_deref(), Some("step 1/1 FROM alpine"));
        p.feed(r#"{"vertexes":[{"digest":"a","name":"[1/3] FROM alpine","started":"t","completed":"t"},{"digest":"b","name":"[2/3] RUN npm ci","started":"t"}]}"#);
        assert_eq!(p.current().as_deref(), Some("step 2/2 RUN npm ci"));
        assert!(!p.feed("not json"));
        p.feed(r#"{"vertexes":[{"digest":"b","name":"[2/3] RUN npm ci","started":"t","completed":"t","error":"exit code: 1"}]}"#);
        assert_eq!(p.current(), None);
        assert_eq!(p.error(), Some("step 2/3 RUN npm ci: exit code: 1"));
    }

    /// BuildKit announces every vertex up front, without `started`.
    fn announce(p: &mut Progress, steps: &[(&str, &str)]) {
        let vs: Vec<String> = steps
            .iter()
            .map(|(d, n)| format!(r#"{{"digest":"{d}","name":"{n}"}}"#))
            .collect();
        p.feed(&format!(r#"{{"vertexes":[{}]}}"#, vs.join(",")));
    }

    fn set(p: &mut Progress, d: &str, started: bool, completed: bool) {
        let mut v = format!(r#"{{"digest":"{d}""#);
        if started {
            v.push_str(r#","started":"t""#);
        }
        if completed {
            v.push_str(r#","completed":"t""#);
        }
        p.feed(&format!(r#"{{"vertexes":[{v}}}]}}"#));
    }

    #[test]
    fn counts_steps_across_parallel_stages() {
        let mut p = Progress::default();
        announce(
            &mut p,
            &[
                ("m", "[internal] load metadata for docker.io/library/rust:1"),
                ("f1", "[frontend 1/3] FROM node"),
                ("f2", "[frontend 2/3] RUN npm ci"),
                ("f3", "[frontend 3/3] RUN npm run build"),
                ("b1", "[backend 1/4] FROM rust"),
                ("b2", "[backend 2/4] COPY . ."),
                ("b3", "[backend 3/4] RUN cargo build --release"),
                ("b4", "[backend 4/4] RUN strip app"),
                ("s1", "[stage-2 1/2] FROM debian"),
                ("s2", "[stage-2 2/2] COPY --from=backend /app /app"),
            ],
        );
        assert_eq!(p.current(), None, "nothing started yet");
        set(&mut p, "m", true, false);
        assert_eq!(
            p.current().as_deref(),
            Some("load metadata for docker.io/library/rust:1")
        );
        set(&mut p, "m", true, true);
        for d in ["f1", "b1", "s1", "f2", "b2"] {
            set(&mut p, d, true, true);
        }
        // backend's cargo build starts first, then frontend's build runs beside it.
        set(&mut p, "b3", true, false);
        set(&mut p, "f3", true, false);
        assert_eq!(
            p.current().as_deref(),
            Some("step 6/9 backend (+1 parallel): RUN cargo build --release")
        );
        // The frontend finishing moves the count, not the step shown.
        set(&mut p, "f3", true, true);
        assert_eq!(
            p.current().as_deref(),
            Some("step 7/9 backend: RUN cargo build --release")
        );
        set(&mut p, "b3", true, true);
        set(&mut p, "b4", true, true);
        set(&mut p, "s2", true, false);
        assert_eq!(
            p.current().as_deref(),
            Some("step 9/9 stage-2: COPY --from=backend /app /app")
        );
    }

    fn log(p: &mut Progress, d: &str, text: &str) {
        let data = base64::engine::general_purpose::STANDARD.encode(text);
        p.feed(&format!(
            r#"{{"logs":[{{"vertex":"{d}","stream":1,"data":"{data}"}}]}}"#
        ));
    }

    #[test]
    fn missing_cache_is_not_the_failure() {
        let mut p = Progress::default();
        p.feed(r#"{"vertexes":[{"digest":"c","name":"importing cache manifest from r/p:buildcache","started":"t","completed":"t","error":"failed to configure registry cache importer: not found"}]}"#);
        assert_eq!(p.error(), None);
        announce(
            &mut p,
            &[
                ("a", "[chef 1/2] FROM rust"),
                ("b", "[chef 2/2] RUN apt-get install -y libssl-dev"),
            ],
        );
        set(&mut p, "a", true, true);
        set(&mut p, "b", true, false);
        log(&mut p, "b", "Unpacking libssl3t64 ...\r\n");
        log(
            &mut p,
            "b",
            "dpkg: error processing archive x.deb (--unpack):\r\n unable to make backup link: No such device\r\n",
        );
        // BuildKit died without reporting the step: its last output line explains it.
        assert_eq!(
            p.failure().as_deref(),
            Some(
                "step 2/2 RUN apt-get install -y libssl-dev: unable to make backup link: No such device"
            )
        );
        p.feed(r#"{"vertexes":[{"digest":"b","name":"[chef 2/2] RUN apt-get install -y libssl-dev","started":"t","completed":"t","error":"exit code: 100"}]}"#);
        assert_eq!(
            p.failure().as_deref(),
            Some("step 2/2 RUN apt-get install -y libssl-dev: exit code: 100")
        );
    }

    #[test]
    fn single_stage_has_no_stage_label() {
        let mut p = Progress::default();
        announce(
            &mut p,
            &[("a", "[1/2] FROM alpine"), ("b", "[2/2] RUN make")],
        );
        set(&mut p, "a", true, true);
        set(&mut p, "b", true, false);
        assert_eq!(p.current().as_deref(), Some("step 2/2 RUN make"));
    }

    #[test]
    fn numbered_steps_are_parsed() {
        assert_eq!(
            numbered_step("[builder 4/12] RUN x"),
            Some(("builder", "RUN x"))
        );
        assert_eq!(numbered_step("[2/5] COPY . ."), Some(("", "COPY . .")));
        assert_eq!(numbered_step("[internal] load .dockerignore"), None);
        assert_eq!(numbered_step("exporting to image"), None);
        assert_eq!(numbered_step("[a b/c] x"), None);
    }
}
