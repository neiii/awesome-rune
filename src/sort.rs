//! Sorts projects.json using the category order defined by its JSON Schema.

use crate::{read, read_json, root};
use anyhow::{Context, Result};
use serde_json::Value;
use std::process::ExitCode;

/// Where `value` falls in the schema's enum for `property`; unknown values sort last.
fn rank(schema: &Value, property: &str, value: &Value) -> usize {
    let order = schema["$defs"]["project"]["properties"][property]["enum"].as_array();
    order.and_then(|o| o.iter().position(|v| v == value)).unwrap_or(usize::MAX)
}

pub fn sorted_content() -> Result<String> {
    // Values, not structs, so fields this tool doesn't know about survive the rewrite.
    let mut data: Value = read_json("projects.json")?;
    let schema: Value = read_json("projects.schema.json")?;
    let projects = data["projects"].as_array_mut().context("projects.json: `projects` is not an array")?;
    projects.sort_by_cached_key(|p| {
        let name = p["name"].as_str().unwrap_or_default().to_string();
        (rank(&schema, "category", &p["category"]), rank(&schema, "subcategory", &p["subcategory"]), name.to_lowercase(), name)
    });
    Ok(serde_json::to_string_pretty(&data)? + "\n")
}

pub fn run(check: bool) -> Result<ExitCode> {
    let expected = sorted_content()?;
    if read("projects.json")? == expected {
        println!("projects.json is sorted.");
        return Ok(ExitCode::SUCCESS);
    }
    if check {
        eprintln!("projects.json is not sorted. Run:\n\n    cargo run --quiet -- sort");
        return Ok(ExitCode::FAILURE);
    }
    std::fs::write(root().join("projects.json"), expected).context("writing projects.json")?;
    println!("Sorted projects.json.");
    Ok(ExitCode::SUCCESS)
}
