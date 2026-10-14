//! Which model runtime, if any, answers on a port.
//!
//! The probe order and the response shapes come from the runtimes' own APIs
//! (ported from ah's discovery): a runtime is identified by what it answers,
//! never by which port it happens to use. llama.cpp readiness is read from
//! `/health`, not `/v1/models`, which answers 200 while a model is still
//! loading.

use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{self, ProbeError};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Identity {
    Ollama {
        version: String,
        models: Vec<String>,
        /// Models resident in memory right now, with the bytes Ollama reports.
        loaded: Vec<Loaded>,
    },
    LlamaCpp {
        model: Option<String>,
        /// Total context the server allocated (`n_ctx`).
        n_ctx: Option<u64>,
        slots: Option<u64>,
        /// `/health` status: `ok`, or why it is not ready.
        health: String,
        /// Router mode serves several models from one port.
        models: Vec<String>,
    },
    ComfyUi {
        version: Option<String>,
        devices: Vec<String>,
    },
    StableDiffusionApi {
        models: Vec<String>,
    },
    LmStudio {
        /// `(model, state)`; state is `loaded` or `not-loaded`.
        models: Vec<(String, String)>,
    },
    OpenAiCompatible {
        models: Vec<String>,
    },
    /// Answers HTTP, but not like any runtime hearth knows.
    OtherHttp {
        status: u16,
    },
    /// Answers, but not with HTTP (a database, a cache, a VPN agent).
    NotHttp,
    /// Accepts connections and never answers: what a wedged server looks like.
    Unresponsive {
        waited_ms: u64,
    },
    Refused,
    Error {
        detail: String,
    },
}

impl Identity {
    /// A model runtime answers here, rather than some other service or
    /// nothing.
    #[must_use]
    pub fn is_runtime(&self) -> bool {
        matches!(
            self,
            Self::Ollama { .. }
                | Self::LlamaCpp { .. }
                | Self::ComfyUi { .. }
                | Self::StableDiffusionApi { .. }
                | Self::LmStudio { .. }
                | Self::OpenAiCompatible { .. }
        )
    }

    /// Every model name this runtime reports: served, listed or loaded.
    #[must_use]
    pub fn models(&self) -> Vec<&str> {
        match self {
            Self::Ollama { models, loaded, .. } => models
                .iter()
                .map(String::as_str)
                .chain(loaded.iter().map(|l| l.name.as_str()))
                .collect(),
            Self::LlamaCpp { model, models, .. } => {
                model.iter().chain(models).map(String::as_str).collect()
            }
            Self::StableDiffusionApi { models } | Self::OpenAiCompatible { models } => {
                models.iter().map(String::as_str).collect()
            }
            Self::LmStudio { models } => models.iter().map(|(m, _)| m.as_str()).collect(),
            Self::ComfyUi { .. }
            | Self::OtherHttp { .. }
            | Self::NotHttp
            | Self::Unresponsive { .. }
            | Self::Refused
            | Self::Error { .. } => Vec::new(),
        }
    }

    /// Why a runtime that answers is not ready to serve yet: llama.cpp
    /// reports loading through `/health`.
    #[must_use]
    pub fn not_ready(&self) -> Option<&str> {
        match self {
            Self::LlamaCpp { health, .. } if health != "ok" => Some(health),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loaded {
    pub name: String,
    pub size_bytes: Option<u64>,
    pub size_vram_bytes: Option<u64>,
    /// The context this model was loaded with, where Ollama reports it.
    #[serde(default)]
    pub context_length: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub addr: SocketAddr,
    #[serde(flatten)]
    pub identity: Identity,
    /// Time to the first answer, which later probes learn deadlines from.
    /// Microseconds: a runtime on loopback answers in well under one.
    pub latency_us: Option<u64>,
}

struct Prober {
    addr: SocketAddr,
    /// Bounds the requests after the first, which ask runtimes for more
    /// (model lists, slot state) and may take longer to answer.
    deadline: Duration,
    max_bytes: u64,
}

enum Probe {
    Json(Value),
    Status(u16),
}

impl Prober {
    fn get(&self, path: &str) -> Result<(Probe, Duration), ProbeError> {
        self.get_within(path, self.deadline)
    }

    fn get_within(&self, path: &str, deadline: Duration) -> Result<(Probe, Duration), ProbeError> {
        let r = http::get(self.addr, path, deadline, self.max_bytes)?;
        let probe = if (200..300).contains(&r.status) {
            serde_json::from_slice(&r.body).map_or(Probe::Status(r.status), Probe::Json)
        } else {
            Probe::Status(r.status)
        };
        Ok((probe, r.elapsed))
    }

    fn json(&self, path: &str) -> Option<Value> {
        match self.get(path) {
            Ok((Probe::Json(v), _)) => Some(v),
            _ => None,
        }
    }
}

fn strings(v: &Value, list: &str, field: &str) -> Vec<String> {
    v[list]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m[field].as_str().map(str::to_string))
        .collect()
}

/// Identifies what answers at `addr`. `deadline` bounds each request.
#[must_use]
pub fn fingerprint(addr: SocketAddr, deadline: Duration, max_bytes: u64) -> Fingerprint {
    fingerprint_within(addr, deadline, deadline, max_bytes)
}

/// Like [`fingerprint`], with a separate deadline for the first request. That
/// request is the one answer times are measured and learned on, so a learned
/// deadline applies to it alone; the requests after it keep `deadline`.
#[must_use]
pub fn fingerprint_within(
    addr: SocketAddr,
    first_deadline: Duration,
    deadline: Duration,
    max_bytes: u64,
) -> Fingerprint {
    let p = Prober {
        addr,
        deadline,
        max_bytes,
    };
    // The first request also decides whether this is HTTP at all; nothing
    // more is sent to a port that answered something else or nothing.
    let first = p.get_within("/api/version", first_deadline);
    let latency_us = first
        .as_ref()
        .ok()
        .map(|(_, d)| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
    let identity = match first {
        Err(ProbeError::Refused) => Identity::Refused,
        Err(ProbeError::NotHttp) => Identity::NotHttp,
        Err(ProbeError::NoAnswer { after }) => Identity::Unresponsive {
            waited_ms: u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
        },
        Err(ProbeError::TooLarge) => Identity::Error {
            detail: "response larger than discovery.max_probe_response_bytes".into(),
        },
        Err(ProbeError::Io(detail)) => Identity::Error { detail },
        Ok((Probe::Json(v), _)) if v["version"].is_string() => ollama(&p, &v),
        Ok((first, _)) => identify_http(&p, &first),
    };
    Fingerprint {
        addr,
        identity,
        latency_us,
    }
}

fn ollama(p: &Prober, version: &Value) -> Identity {
    let tags = p.json("/api/tags").unwrap_or(Value::Null);
    let ps = p.json("/api/ps").unwrap_or(Value::Null);
    Identity::Ollama {
        version: version["version"].as_str().unwrap_or_default().to_string(),
        models: strings(&tags, "models", "name"),
        loaded: ps["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                Some(Loaded {
                    name: m["name"].as_str()?.to_string(),
                    size_bytes: m["size"].as_u64(),
                    size_vram_bytes: m["size_vram"].as_u64(),
                    context_length: m["context_length"].as_u64(),
                })
            })
            .collect(),
    }
}

fn identify_http(p: &Prober, first: &Probe) -> Identity {
    if let Some(props) = p.json("/props")
        && props.get("default_generation_settings").is_some()
    {
        let health = match p.get("/health") {
            Ok((Probe::Json(v), _)) => v["status"].as_str().unwrap_or("ok").to_string(),
            Ok((Probe::Status(s), _)) => format!("http {s}"),
            Err(e) => e.to_string(),
        };
        let models = p
            .json("/v1/models")
            .map(|v| strings(&v, "data", "id"))
            .unwrap_or_default();
        return Identity::LlamaCpp {
            model: props["model_path"]
                .as_str()
                .or_else(|| props["model_alias"].as_str())
                .map(str::to_string),
            n_ctx: props["default_generation_settings"]["n_ctx"].as_u64(),
            slots: props["total_slots"].as_u64(),
            health,
            models,
        };
    }
    if let Some(stats) = p.json("/system_stats")
        && stats.get("system").is_some()
    {
        return Identity::ComfyUi {
            version: stats["system"]["comfyui_version"]
                .as_str()
                .map(str::to_string),
            devices: strings(&stats, "devices", "name"),
        };
    }
    if let Some(Value::Array(models)) = p.json("/sdapi/v1/sd-models") {
        return Identity::StableDiffusionApi {
            models: models
                .iter()
                .filter_map(|m| m["title"].as_str().map(str::to_string))
                .collect(),
        };
    }
    if let Some(v) = p.json("/api/v0/models")
        && let Some(data) = v["data"].as_array()
        && data.iter().all(|m| m.get("state").is_some())
    {
        return Identity::LmStudio {
            models: data
                .iter()
                .filter_map(|m| {
                    Some((
                        m["id"].as_str()?.to_string(),
                        m["state"].as_str()?.to_string(),
                    ))
                })
                .collect(),
        };
    }
    if let Some(v) = p.json("/v1/models")
        && v["data"].is_array()
    {
        return Identity::OpenAiCompatible {
            models: strings(&v, "data", "id"),
        };
    }
    Identity::OtherHttp {
        status: match first {
            Probe::Status(s) => *s,
            Probe::Json(_) => 200,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Serves fixed JSON per path, the way the real runtime would answer.
    fn fake(routes: Vec<(&'static str, &'static str)>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let reply = routes.iter().find(|(p, _)| *p == path).map_or_else(
                    || "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
                    |(_, body)| {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    },
                );
                let _ = s.write_all(reply.as_bytes());
            }
        });
        addr
    }

    fn fp(addr: SocketAddr) -> Identity {
        fingerprint(addr, Duration::from_secs(2), 1 << 20).identity
    }

    #[test]
    fn recognises_ollama_with_loaded_models() {
        let addr = fake(vec![
            ("/api/version", r#"{"version":"0.30.10"}"#),
            (
                "/api/tags",
                r#"{"models":[{"name":"glm-ocr:bf16"},{"name":"qwen3:4b"}]}"#,
            ),
            (
                "/api/ps",
                r#"{"models":[{"name":"glm-ocr:bf16","size":2000,"size_vram":1500}]}"#,
            ),
        ]);
        match fp(addr) {
            Identity::Ollama {
                version,
                models,
                loaded,
            } => {
                assert_eq!(version, "0.30.10");
                assert_eq!(models.len(), 2);
                assert_eq!(loaded[0].size_vram_bytes, Some(1500));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn recognises_llama_server_by_props_not_by_models() {
        let addr = fake(vec![
            (
                "/props",
                r#"{"default_generation_settings":{"n_ctx":262144},"total_slots":1,"model_path":"/m/q.gguf"}"#,
            ),
            ("/health", r#"{"status":"ok"}"#),
            ("/v1/models", r#"{"data":[{"id":"q"}]}"#),
        ]);
        match fp(addr) {
            Identity::LlamaCpp {
                n_ctx,
                slots,
                health,
                model,
                ..
            } => {
                assert_eq!((n_ctx, slots), (Some(262_144), Some(1)));
                assert_eq!(health, "ok");
                assert_eq!(model.as_deref(), Some("/m/q.gguf"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plain_openai_server_and_other_http() {
        let addr = fake(vec![(
            "/v1/models",
            r#"{"data":[{"id":"mlx-community/x"}]}"#,
        )]);
        assert!(matches!(fp(addr), Identity::OpenAiCompatible { .. }));
        let addr = fake(vec![]);
        assert!(matches!(fp(addr), Identity::OtherHttp { status: 404 }));
    }
}
