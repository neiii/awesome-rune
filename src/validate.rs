//! Validates projects.json and ignored.json against their JSON Schemas, formats included.

use crate::read_json;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::process::ExitCode;

const FILES: [(&str, &str); 2] = [("projects.json", "projects.schema.json"), ("ignored.json", "ignored.schema.json")];

pub fn run() -> Result<ExitCode> {
    let mut valid = true;
    for (file, schema) in FILES {
        let validator = jsonschema::options().should_validate_formats(true).build(&read_json::<Value>(schema)?).map_err(|e| anyhow!("{schema}: {e}"))?;
        let instance: Value = read_json(file)?;
        let mut errors = validator.iter_errors(&instance).peekable();
        if errors.peek().is_none() {
            println!("{file} is valid.");
            continue;
        }
        valid = false;
        for error in errors {
            eprintln!("{file}{}: {error}", error.instance_path());
        }
    }
    Ok(if valid { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}
