//! Workflows: which hearth tools to call, in which order, for a question
//! that keeps coming up, and how to answer it. Built-in ones ship in
//! `mcp-prompts.json`; anyone can add their own, or replace a built-in, in
//! `workflows.json` beside their configuration file. `hearth mcp` serves
//! them as MCP prompts and `hearth workflows` prints them.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::{Value, json};

/// The built-in workflows.
pub fn builtin() -> Vec<Value> {
    serde_json::from_str::<Vec<Value>>(include_str!("mcp-prompts.json")).unwrap_or_default()
}

/// Where the user's own workflows are read from: `workflows.json` in the
/// directory of the user's configuration file.
pub fn user_file() -> Option<PathBuf> {
    Some(
        hearth_core::config::user_path()?
            .parent()?
            .join("workflows.json"),
    )
}

/// Why `w` is not a usable workflow, if it is not: a name, a description, a
/// template, arguments with names, and only tools that exist in backticks.
fn problem(w: &Value, tools: &[String]) -> Option<String> {
    let name = w["name"].as_str().filter(|n| {
        !n.is_empty()
            && n.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    let Some(name) = name else {
        return Some("a workflow needs a name of lowercase letters, digits and '-'".into());
    };
    let Some(template) = w["template"].as_str() else {
        return Some(format!("{name}: no template"));
    };
    if !w["description"].is_string() {
        return Some(format!("{name}: no description"));
    }
    let args = match &w["arguments"] {
        Value::Null => Vec::new(),
        Value::Array(a) => a.clone(),
        _ => return Some(format!("{name}: arguments must be a list")),
    };
    let mut seen = Vec::new();
    for a in &args {
        let Some(arg) = a["name"].as_str().filter(|n| !n.is_empty()) else {
            return Some(format!("{name}: every argument needs a name"));
        };
        if seen.contains(&arg) {
            return Some(format!("{name}: argument {arg} is declared twice"));
        }
        seen.push(arg);
        if !(a["default"].is_null() || a["default"].is_string())
            || !(a["required"].is_null() || a["required"].is_boolean())
        {
            return Some(format!(
                "{name}: {arg}: default must be text and required true or false"
            ));
        }
    }
    // Backticks are for tool names only, so an agent reading the template
    // can trust that what is quoted that way is a tool it may call.
    if template.matches('`').count() % 2 != 0 {
        return Some(format!("{name}: unbalanced backtick"));
    }
    for named in template.split('`').skip(1).step_by(2) {
        if !tools.iter().any(|t| t == named) {
            return Some(format!(
                "{name}: `{named}` is not a hearth tool (backticks are for tool names only)"
            ));
        }
    }
    None
}

/// Every workflow, built-in first, the user's replacing any of the same
/// name; and what was wrong with the user's file, if anything.
pub fn all(tools: &[String]) -> (Vec<Value>, Vec<String>) {
    let mut out: Vec<Value> = builtin()
        .into_iter()
        .map(|mut w| {
            w["source"] = json!("built-in");
            w
        })
        .collect();
    let mut problems = Vec::new();
    let Some(path) = user_file() else {
        return (out, problems);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (out, problems),
        Err(e) => {
            problems.push(format!("{}: {e}", path.display()));
            return (out, problems);
        }
    };
    let mine: Vec<Value> = match serde_json::from_str(&text) {
        Ok(Value::Array(a)) => a,
        Ok(_) => {
            problems.push(format!(
                "{}: must be a JSON array of workflows",
                path.display()
            ));
            return (out, problems);
        }
        Err(e) => {
            problems.push(format!("{}: {e}", path.display()));
            return (out, problems);
        }
    };
    for mut w in mine {
        if let Some(p) = problem(&w, tools) {
            problems.push(format!("{}: {p}", path.display()));
            continue;
        }
        w["source"] = json!(path.display().to_string());
        let mine_before = out
            .iter()
            .any(|o| o["name"] == w["name"] && o["source"] != "built-in");
        if mine_before {
            problems.push(format!(
                "{}: {} is defined twice; the later one is used",
                path.display(),
                w["name"].as_str().unwrap_or_default()
            ));
        }
        match out.iter_mut().find(|o| o["name"] == w["name"]) {
            Some(slot) => *slot = w,
            None => out.push(w),
        }
    }
    (out, problems)
}

/// `w`'s template with `arguments` filled in, in one pass: a value is
/// never itself read for placeholders. A missing argument takes its
/// default; a missing required one, an undeclared one, or one that is not
/// text or a number is an error.
pub fn render(w: &Value, arguments: &Value) -> Result<String, String> {
    let name = w["name"].as_str().unwrap_or_default();
    let declared: Vec<&Value> = w["arguments"].as_array().into_iter().flatten().collect();
    if let Some(given) = arguments.as_object() {
        for key in given.keys() {
            if !declared.iter().any(|a| a["name"] == key.as_str()) {
                return Err(format!("{name} takes no argument {key}"));
            }
        }
    }
    let mut values = std::collections::BTreeMap::new();
    for arg in &declared {
        let key = arg["name"].as_str().unwrap_or_default();
        let given = match &arguments[key] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => return Err(format!("{name}: {key} must be text or a number")),
        };
        let value = match (given, arg["default"].as_str()) {
            (Some(v), _) => v,
            (None, Some(default)) => default.to_string(),
            (None, None) if arg["required"] == true => {
                return Err(format!("{name} needs the argument {key}"));
            }
            (None, None) => "not given".into(),
        };
        values.insert(key.to_string(), value);
    }
    let template = w["template"].as_str().unwrap_or_default();
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let found = after
            .find('}')
            .and_then(|close| Some((close, values.get(&after[..close])?)));
        if let Some((close, value)) = found {
            out.push_str(value);
            rest = &after[close + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(clap::Args)]
pub struct Args {
    /// Print this workflow, filled in; without a name, list them.
    name: Option<String>,
    /// An argument for the workflow, as KEY=VALUE (repeatable).
    #[arg(long = "arg", value_name = "KEY=VALUE")]
    args: Vec<String>,
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let (all, problems) = all(&crate::mcp::tool_names());
    let Some(name) = &args.name else {
        if json {
            return crate::to_json(&json!({ "workflows": all, "problems": problems }));
        }
        let mut out = String::new();
        for w in &all {
            let _ = writeln!(
                out,
                "{:<24} {}  [{}]",
                w["name"].as_str().unwrap_or_default(),
                w["description"].as_str().unwrap_or_default(),
                w["source"].as_str().unwrap_or_default()
            );
        }
        for p in &problems {
            let _ = writeln!(out, "skipped: {p}");
        }
        if let Some(path) = user_file() {
            let _ = writeln!(out, "\nyour own go in {} (a JSON array)", path.display());
        }
        return Ok(out);
    };
    let w = all
        .iter()
        .find(|w| w["name"] == name.as_str())
        .ok_or_else(|| format!("no workflow named {name:?}; hearth workflows lists them"))?;
    let mut given = serde_json::Map::new();
    for a in &args.args {
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| format!("--arg {a:?} must be KEY=VALUE"))?;
        given.insert(k.to_string(), json!(v));
    }
    let text = render(w, &Value::Object(given))?;
    if json {
        crate::to_json(&json!({ "name": name, "text": text }))
    } else {
        Ok(text + "\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every source file of the workspace, for checking names against.
    fn sources(dir: &std::path::Path, out: &mut String) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push_str(&std::fs::read_to_string(&p).unwrap());
            }
        }
    }

    /// A field or argument a built-in workflow names (snake_case, so it is
    /// an identifier, not prose) exists: as a tool's argument or in its
    /// description, or as a field hearth reports. A renamed field cannot
    /// leave a workflow pointing at nothing.
    #[test]
    fn workflows_name_only_fields_that_exist() {
        let tools = include_str!("mcp-tools.json");
        let mut code = String::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".."),
            &mut code,
        );
        for w in builtin() {
            let template = w["template"].as_str().unwrap();
            for word in template.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                let identifier = word.contains('_')
                    && word.starts_with(|c: char| c.is_ascii_lowercase())
                    && !word.ends_with('_');
                if identifier {
                    assert!(
                        tools.contains(word) || code.contains(word),
                        "{}: {word} is not a field or argument hearth has",
                        w["name"]
                    );
                }
            }
        }
    }

    #[test]
    fn a_workflow_naming_a_missing_tool_is_refused() {
        let tools = vec!["doctor".to_string()];
        let ok = json!({"name":"mine","description":"d","template":"call `doctor`"});
        assert_eq!(problem(&ok, &tools), None);
        let bad = json!({"name":"mine","description":"d","template":"call `rm`"});
        assert!(problem(&bad, &tools).unwrap().contains("rm"));
        assert!(problem(&json!({"name":"Bad Name"}), &tools).is_some());
    }

    #[test]
    fn values_are_not_expanded_again_and_unknown_arguments_refused() {
        let w = json!({"name":"w","template":"{x} then {y}","arguments":[
            {"name":"x"},{"name":"y","default":"secret"}]});
        assert_eq!(render(&w, &json!({"x":"{y}"})).unwrap(), "{y} then secret");
        assert!(render(&w, &json!({"z":"1"})).is_err());
        assert!(render(&w, &json!({"x":true})).is_err());
        let dup = json!({"name":"d","description":"d","template":"t","arguments":[{"name":"a"},{"name":"a"}]});
        assert!(problem(&dup, &[]).unwrap().contains("twice"));
        let tick = json!({"name":"d","description":"d","template":"use `doctor"});
        assert!(problem(&tick, &["doctor".into()]).is_some());
    }

    #[test]
    fn rendering_fills_defaults_and_requires_what_is_required() {
        let w = json!({"name":"w","template":"look at {x} and {y}","arguments":[
            {"name":"x","required":true},{"name":"y","default":"everything"}]});
        assert_eq!(
            render(&w, &json!({"x":"ports"})).unwrap(),
            "look at ports and everything"
        );
        assert!(render(&w, &json!({})).is_err());
    }
}
