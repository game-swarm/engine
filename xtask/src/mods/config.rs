use std::path::{Component, Path};
use std::{fmt, str::FromStr};

use toml_edit::{DocumentMut, Item, Value};

#[derive(Debug)]
pub(crate) struct ModsConfig {
    document: DocumentMut,
}

impl ModsConfig {
    pub(crate) fn parse(source: &str) -> Result<Self, ConfigError> {
        let document = DocumentMut::from_str(source)
            .map_err(|error| ConfigError::new(format!("invalid mods.toml: {error}")))?;
        let config = Self { document };
        config.entries()?;
        Ok(config)
    }

    pub(crate) fn entries(&self) -> Result<Vec<ModEntry>, ConfigError> {
        let mods = self
            .document
            .get("mods")
            .and_then(Item::as_table)
            .ok_or_else(|| ConfigError::new("mods.toml must contain a [mods] table"))?;

        mods.iter()
            .map(|(name, item)| parse_entry(name, item))
            .collect()
    }

    pub(crate) fn set_revision(
        &mut self,
        name: &ModName,
        revision: &CommitSha,
    ) -> Result<(), ConfigError> {
        let entry = self
            .document
            .get_mut("mods")
            .and_then(Item::as_table_mut)
            .and_then(|mods| mods.get_mut(name.as_str()))
            .and_then(Item::as_inline_table_mut)
            .ok_or_else(|| ConfigError::new(format!("unknown mod '{}'", name.as_str())))?;

        entry.insert("rev", Value::from(revision.as_str()));
        Ok(())
    }

    pub(crate) fn render(&self) -> String {
        self.document.to_string()
    }
}

fn parse_entry(name: &str, item: &Item) -> Result<ModEntry, ConfigError> {
    validate_mod_name(name)?;
    let table = item
        .as_inline_table()
        .ok_or_else(|| ConfigError::new(format!("mod '{name}' must be an inline table")))?;
    let git = table.get("git").and_then(Value::as_str);
    let path = table.get("path").and_then(Value::as_str);

    let source = match (git, path) {
        (Some(repository), None) => {
            if repository.is_empty() || repository.starts_with('-') {
                return Err(ConfigError::new(format!(
                    "git repository for mod '{name}' must be non-empty and must not start with '-'"
                )));
            }
            let revision = table
                .get("rev")
                .and_then(Value::as_str)
                .ok_or_else(|| ConfigError::new(format!("git mod '{name}' must declare rev")))?;
            if !is_sha40(revision) {
                return Err(ConfigError::new(format!(
                    "git mod '{name}' rev must be exactly 40 ASCII hex characters"
                )));
            }
            ModSource::Git {
                repository: repository.to_string(),
                revision: CommitSha::new(revision.to_string()),
            }
        }
        (None, Some(path)) => ModSource::Path {
            path: path.to_string(),
        },
        (Some(_), Some(_)) => {
            return Err(ConfigError::new(format!(
                "mod '{name}' cannot declare both git and path"
            )));
        }
        (None, None) => {
            return Err(ConfigError::new(format!(
                "mod '{name}' must declare git or path"
            )));
        }
    };

    Ok(ModEntry {
        name: ModName::new(name.to_string()),
        source,
    })
}

fn validate_mod_name(name: &str) -> Result<(), ConfigError> {
    let mut components = Path::new(name).components();
    let is_single_normal_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if is_single_normal_component {
        return Ok(());
    }
    Err(ConfigError::new(format!(
        "mod name '{name}' must be a single path component"
    )))
}

fn is_sha40(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModEntry {
    pub(crate) name: ModName,
    pub(crate) source: ModSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModSource {
    Git {
        repository: String,
        revision: CommitSha,
    },
    Path {
        path: String,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ModName(String);

impl ModName {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitSha(String);

impl CommitSha {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn rejects_empty_and_option_like_git_repositories() {
        for repository in ["", "--upload-pack=malicious"] {
            let source = format!(
                "[mods]\ncombat-core = {{ git = \"{repository}\", rev = \"{REVISION}\" }}\n"
            );

            let error = ModsConfig::parse(&source).unwrap_err();

            assert!(error.to_string().contains("git repository"));
        }
    }
}
