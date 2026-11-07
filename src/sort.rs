//! Sorts projects.json using the category order defined by its JSON Schema.

use crate::root;
use serde_json::Value;
use std::process::ExitCode;

fn enum_order(schema: &Value, property: &str) -> Vec<String> {
    schema["$defs"]["project"]["properties"][property]["enum"]
        .as_array()
        .map(|values| values.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn position(order: &[String], value: Option<&str>) -> usize {
    value.and_then(|v| order.iter().position(|o| o == v)).unwrap_or(order.len())
}

pub fn sorted_content() -> Result<String, String> {
    let read = |name: &str| -> Result<Value, String> {
        let text = std::fs::read_to_string(root().join(name)).map_err(|e| format!("{name}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))
    };
    let mut data = read("projects.json")?;
    let schema = read("projects.schema.json")?;
    let (categories, subcategories) = (enum_order(&schema, "category"), enum_order(&schema, "subcategory"));
    let projects = data["projects"].as_array_mut().ok_or("projects.json: `projects` is not an array")?;
    projects.sort_by_cached_key(|project| {
        let name = project["name"].as_str().unwrap_or("").to_string();
        (
            position(&categories, project["category"].as_str()),
            position(&subcategories, project["subcategory"].as_str()),
            name.to_lowercase(),
            name,
        )
    });
    Ok(serde_json::to_string_pretty(&data).map_err(|e| e.to_string())? + "\n")
}

pub fn run(check: bool) -> Result<ExitCode, String> {
    let path = root().join("projects.json");
    let current = std::fs::read_to_string(&path).map_err(|e| format!("projects.json: {e}"))?;
    let expected = sorted_content()?;
    if current == expected {
        println!("projects.json is sorted.");
        return Ok(ExitCode::SUCCESS);
    }
    if check {
        eprintln!("projects.json is not sorted. Run:\n\n    cargo run --quiet -- sort");
        return Ok(ExitCode::FAILURE);
    }
    std::fs::write(&path, expected).map_err(|e| format!("projects.json: {e}"))?;
    println!("Sorted projects.json.");
    Ok(ExitCode::SUCCESS)
}
