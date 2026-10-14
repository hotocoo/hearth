//! `hearth config`: every setting in effect, where its value came from, and
//! what it means. hearth keeps no number in its code; this is where all of
//! them can be seen.

use std::fmt::Write as _;

#[derive(clap::Args)]
pub struct Args {
    /// Only settings whose key contains this, e.g. `supervise` or `deadline`.
    filter: Option<String>,
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let settings: Vec<_> = hearth_core::config::explain()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|s| {
            args.filter
                .as_ref()
                .is_none_or(|f| s.key.contains(f.as_str()))
        })
        .collect();
    if json {
        return crate::to_json(&serde_json::json!({
            "user_file": hearth_core::config::user_path(),
            "settings": settings,
        }));
    }
    let mut out = String::new();
    match hearth_core::config::user_path() {
        Some(path) if path.exists() => {
            let _ = writeln!(
                out,
                "user file: {} (merged over the built-in values)\n",
                path.display()
            );
        }
        Some(path) => {
            let _ = writeln!(
                out,
                "no user file at {}: every value is built in\n",
                path.display()
            );
        }
        None => {}
    }
    for s in &settings {
        let value = s
            .value
            .as_ref()
            .map_or_else(|| "(unset)".to_string(), ToString::to_string);
        let _ = writeln!(out, "{} = {value}    [{}]", s.key, s.source);
        if !s.doc.is_empty() {
            let _ = writeln!(out, "    {}", s.doc);
        }
    }
    Ok(out)
}
