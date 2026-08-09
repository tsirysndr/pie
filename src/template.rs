//! Minimal `{{ name }}` substitution.
//!
//! Deliberately not a template language: recipes are build scripts, and a build
//! script that needs conditionals should say so in shell, where it can be read.

use anyhow::{bail, Result};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.0.insert(key.into(), value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|s| s.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    /// Expands every `{{ key }}` in `input`. An unknown key is an error rather
    /// than an empty string: a silently blank path is far worse than a failure.
    pub fn expand(&self, input: &str) -> Result<String> {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;

        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                bail!("unterminated '{{{{' in template: {input}");
            };
            let key = after[..end].trim();
            match self.0.get(key) {
                Some(value) => out.push_str(value),
                None => bail!(
                    "unknown template variable '{{{{{key}}}}}' (known: {})",
                    self.0.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            }
            rest = &after[end + 2..];
        }

        out.push_str(rest);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_and_trims() {
        let mut vars = Vars::new();
        vars.set("version", "v22.11.0").set("arch", "x64");
        assert_eq!(
            vars.expand("node-{{version}}-linux-{{ arch }}").unwrap(),
            "node-v22.11.0-linux-x64"
        );
    }

    #[test]
    fn unknown_variable_is_an_error() {
        assert!(Vars::new().expand("{{nope}}").is_err());
    }

    #[test]
    fn leaves_plain_text_alone() {
        assert_eq!(Vars::new().expand("make -j4").unwrap(), "make -j4");
    }
}
