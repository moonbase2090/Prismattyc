// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};
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
        ensure!(
            output.is_valid(&serde_json::from_str::<Value>(json)?),
            "command result does not match its schema"
        );
        Ok(())
    }
}

fn compile(text: &str) -> Result<Validator> {
    ensure!(
        text.len() <= 64 * 1024,
        "command schema exceeds its byte budget"
    );
    let schema: Value = serde_json::from_str(text)?;
    Ok(jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_pattern_options(
            PatternOptions::regex()
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024),
        )
        .with_retriever(DenyRetriever)
        .build(&schema)
        .context("invalid or unsupported command schema")?)
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
        assert!(schemas.output("read", r#"{"ok":false}"#).is_err());
        assert!(schemas.input("undeclared", "{}").is_err());
        assert!(compile(r#"{"$ref":"file:///etc/passwd"}"#).is_err());
        assert!(compile(r#"{"$ref":"https://example.invalid/schema"}"#).is_err());
        assert!(compile(r#"{"type":"string","pattern":"(a)\\1"}"#).is_err());
    }
}
