//! Validates projects.json and ignored.json against their JSON Schemas, formats included.

use crate::root;
use serde_json::Value;
use std::process::ExitCode;

const FILES: [(&str, &str); 2] = [("projects.json", "projects.schema.json"), ("ignored.json", "ignored.schema.json")];

fn read(name: &str) -> Result<Value, String> {
    let text = std::fs::read_to_string(root().join(name)).map_err(|e| format!("{name}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))
}

pub fn run() -> Result<ExitCode, String> {
    let mut valid = true;
    for (file, schema) in FILES {
        let validator = jsonschema::options().should_validate_formats(true).build(&read(schema)?).map_err(|e| format!("{schema}: {e}"))?;
        let instance = read(file)?;
        let errors: Vec<String> = validator.iter_errors(&instance).map(|e| format!("{file}{}: {e}", e.instance_path())).collect();
        if errors.is_empty() {
            println!("{file} is valid.");
        } else {
            valid = false;
            for error in errors {
                eprintln!("{error}");
            }
        }
    }
    Ok(if valid { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}
