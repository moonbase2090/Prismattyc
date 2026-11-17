// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context, Result};
use jsonschema::{Draft, PatternOptions, Retrieve, Uri, Validator};
use serde_json::Value;

use super::wire::CommandSchema;

struct DenyRetriever;
impl Retrieve for DenyRetriever {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema references are not permitted".into())
    }
}

pub(super) struct Schemas(BTreeMap<String, (Validator, Validator)>);

impl Schemas {
    /// Schema compilation and validation run only in the killable native worker.
    pub(super) fn compile(schemas: BTreeMap<String, CommandSchema>) -> Result<Self> {
        ensure!(schemas.len() <= 64, "too many command schemas");
        let mut compiled = BTreeMap::new();
        for (id, schema) in schemas {
            compiled.insert(id, (compile(&schema.input)?, compile(&schema.output)?));
        }
        Ok(Self(compiled))
    }

    pub(super) fn input(&self, id: &str, json: &str) -> Result<()> {
        let (input, _) = self.0.get(id).context("unknown command contribution")?;
        ensure!(
            input.is_valid(&serde_json::from_str::<Value>(json)?),
            "command arguments do not match their schema"
        );
        Ok(())
    }

    pub(super) fn output(&self, id: &str, json: &str) -> Result<()> {
        let (_, output) = self.0.get(id).context("unknown command contribution")?;
        let value: Value = serde_json::from_str(json)?;
        if output.is_valid(&value) {
            return Ok(());
        }
        let detail = output
            .iter_errors(&value)
            .next()
            .map(|error| schema_mismatch(&error))
            .unwrap_or_else(|| "schema".to_string());
        bail!("command result does not match its schema: {detail}")
    }
}

fn schema_mismatch(error: &jsonschema::ValidationError<'_>) -> String {
    let mut best_len = 0usize;
    let mut best = String::new();
    fn walk(error: &jsonschema::ValidationError<'_>, best_len: &mut usize, best: &mut String) {
        let path = error.instance_path().as_str();
        let described = describe(error);
        if path.len() > *best_len || (path.len() == *best_len && described.len() > best.len()) {
            *best_len = path.len();
            *best = if path.is_empty() {
                described
            } else {
                format!("{path} {described}")
            };
        }
        let nested = match error.kind() {
            jsonschema::error::ValidationErrorKind::OneOfNotValid { context }
            | jsonschema::error::ValidationErrorKind::OneOfMultipleValid { context }
            | jsonschema::error::ValidationErrorKind::AnyOf { context } => context,
            _ => return,
        };
        for child in nested.iter().flatten() {
            walk(child, best_len, best);
        }
    }
    walk(error, &mut best_len, &mut best);
    best
}

fn describe(error: &jsonschema::ValidationError<'_>) -> String {
    let keyword = error.kind().keyword();
    match error.kind() {
        jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
            format!("{keyword} ({})", unexpected.join(", "))
        }
        jsonschema::error::ValidationErrorKind::Required { property } => {
            format!("{keyword} ({property})")
        }
        _ => keyword.to_string(),
    }
}

fn compile(text: &str) -> Result<Validator> {
    ensure!(
        text.len() <= 64 * 1024,
        "command schema exceeds its byte budget"
    );
    let schema: Value = serde_json::from_str(text)?;
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_pattern_options(
            PatternOptions::regex()
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024),
        )
        .with_retriever(DenyRetriever)
        .build(&schema)
        .context("invalid or unsupported command schema")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_validate_observable_arguments_and_reject_file_or_network_refs() {
        let schemas = Schemas::compile(BTreeMap::from([("read".into(), CommandSchema {
            input: r#"{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":256}},"required":["limit"],"additionalProperties":false}"#.into(),
            output: r#"{"type":"object","properties":{"ok":{"const":true}},"required":["ok"],"additionalProperties":false}"#.into(),
        })])).unwrap();
        schemas.input("read", r#"{"limit":3}"#).unwrap();
        assert!(schemas.input("read", r#"{"limit":257}"#).is_err());
        assert!(schemas
            .input("read", r#"{"limit":3,"shell":"run"}"#)
            .is_err());
        let mismatch = schemas.output("read", r#"{"ok":false}"#).unwrap_err();
        let mismatch = format!("{mismatch}");
        assert!(
            mismatch.contains("does not match its schema") && mismatch.contains("/ok"),
            "{mismatch}"
        );
        assert!(schemas.input("undeclared", "{}").is_err());
        assert!(compile(r#"{"$ref":"file:///etc/passwd"}"#).is_err());
        assert!(compile(r#"{"$ref":"https://example.invalid/schema"}"#).is_err());
        assert!(compile(r#"{"type":"string","pattern":"(a)\\1"}"#).is_err());
    }
}
