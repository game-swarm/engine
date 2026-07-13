use std::{fs, path::Path};

use toml::{Value, map::Map};

pub fn try_run(args: impl IntoIterator<Item = String>) -> Result<bool, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    if args.first().map(|s| s.as_str()) != Some("mod") {
        return Ok(false);
    }
    run(&args[1..])?;
    Ok(true)
}

fn run(args: &[String]) -> Result<(), String> {
    let mut lock_path = "mods.lock".to_string();
    let mut parsed = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--lock" {
            index += 1;
            lock_path = args.get(index).ok_or("missing path after --lock")?.clone();
        } else {
            parsed.push(args[index].clone());
        }
        index += 1;
    }

    match parsed.as_slice() {
        [cmd, name, version] if cmd == "add" => {
            add_mod(&lock_path, name, version)?;
            println!("added plugin {name} {version}");
        }
        [cmd, name, version] if cmd == "upgrade" => {
            upgrade_mod(&lock_path, name, version)?;
            println!("upgraded plugin {name} {version}");
        }
        [cmd, name] if cmd == "disable" => {
            set_enabled(&lock_path, name, false)?;
            println!("disabled plugin {name}");
        }
        [cmd, name] if cmd == "enable" => {
            set_enabled(&lock_path, name, true)?;
            println!("enabled plugin {name}");
        }
        [cmd, name] if cmd == "remove" => {
            remove_mod(&lock_path, name)?;
            println!("removed plugin {name}");
        }
        [cmd] if cmd == "list" => {
            for plugin in list_mods(&lock_path)? {
                println!(
                    "{} {} enabled={}",
                    plugin.name, plugin.version, plugin.enabled
                );
            }
        }
        [cmd, name, key, value] if cmd == "config" => {
            configure_mod(&lock_path, name, key, parse_value(value))?;
            println!("updated plugin {name} config {key}");
        }
        _ => {
            return Err(
                "usage: mod [--lock mods.lock] add <name> <version>|upgrade <name> <version>|disable <name>|enable <name>|remove <name>|list|config <name> <key> <value>".into(),
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstalledMod {
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub config: Map<String, Value>,
}

pub fn add_mod(path: impl AsRef<Path>, name: &str, version: &str) -> Result<InstalledMod, String> {
    let mut doc = read(path.as_ref())?;
    let plugins = plugins_mut(&mut doc)?;
    if plugins.contains_key(name) {
        return Err(format!("plugin already exists: {name}"));
    }
    plugins.insert(
        name.to_string(),
        Value::Table(entry_table(version, true, Map::new())),
    );
    write(path.as_ref(), &doc)?;
    Ok(InstalledMod {
        name: name.to_string(),
        version: version.to_string(),
        enabled: true,
        config: Map::new(),
    })
}

pub fn upgrade_mod(path: impl AsRef<Path>, name: &str, version: &str) -> Result<(), String> {
    let mut doc = read(path.as_ref())?;
    let entry = plugin_entry_mut(&mut doc, name)?;
    entry.insert("version".to_string(), Value::String(version.to_string()));
    write(path.as_ref(), &doc)
}

pub fn set_enabled(path: impl AsRef<Path>, name: &str, enabled: bool) -> Result<(), String> {
    let mut doc = read(path.as_ref())?;
    let entry = plugin_entry_mut(&mut doc, name)?;
    entry.insert("enabled".to_string(), Value::Boolean(enabled));
    write(path.as_ref(), &doc)
}

pub fn remove_mod(path: impl AsRef<Path>, name: &str) -> Result<(), String> {
    let mut doc = read(path.as_ref())?;
    let plugins = plugins_mut(&mut doc)?;
    if plugins.remove(name).is_none() {
        return Err(format!("plugin not installed: {name}"));
    }
    write(path.as_ref(), &doc)
}

pub fn configure_mod(
    path: impl AsRef<Path>,
    name: &str,
    key: &str,
    value: Value,
) -> Result<(), String> {
    let mut doc = read(path.as_ref())?;
    let entry = plugin_entry_mut(&mut doc, name)?;
    let config = entry
        .entry("config".to_string())
        .or_insert_with(|| Value::Table(Map::new()))
        .as_table_mut()
        .ok_or("plugin config must be table")?;
    config.insert(key.to_string(), value);
    write(path.as_ref(), &doc)
}

pub fn list_mods(path: impl AsRef<Path>) -> Result<Vec<InstalledMod>, String> {
    let doc = read(path.as_ref())?;
    let Some(plugins) = doc.get("plugins").and_then(Value::as_table) else {
        return Ok(Vec::new());
    };
    let mut out = plugins
        .iter()
        .filter_map(|(name, value)| {
            let table = value.as_table()?;
            Some(InstalledMod {
                name: name.clone(),
                version: table.get("version")?.as_str()?.to_string(),
                enabled: table
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                config: table
                    .get("config")
                    .and_then(Value::as_table)
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    out.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(out)
}

fn entry_table(version: &str, enabled: bool, config: Map<String, Value>) -> Map<String, Value> {
    let mut entry = Map::new();
    entry.insert("version".to_string(), Value::String(version.to_string()));
    entry.insert("enabled".to_string(), Value::Boolean(enabled));
    if !config.is_empty() {
        entry.insert("config".to_string(), Value::Table(config));
    }
    entry
}

fn plugin_entry_mut<'a>(
    doc: &'a mut Value,
    name: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    plugins_mut(doc)?
        .get_mut(name)
        .and_then(Value::as_table_mut)
        .ok_or_else(|| format!("plugin not installed: {name}"))
}

fn plugins_mut(doc: &mut Value) -> Result<&mut Map<String, Value>, String> {
    doc.as_table_mut()
        .ok_or("root must be table")?
        .entry("plugins".to_string())
        .or_insert_with(|| Value::Table(Map::new()))
        .as_table_mut()
        .ok_or("plugins must be table".to_string())
}

fn read(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map_err(|error| format!("failed to parse {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Table(Map::new())),
        Err(error) => Err(format!("failed to read {}: {error}", path.display())),
    }
}

fn write(path: &Path, doc: &Value) -> Result<(), String> {
    fs::write(
        path,
        toml::to_string_pretty(doc).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn parse_value(value: &str) -> Value {
    if let Ok(parsed) = value.parse::<i64>() {
        Value::Integer(parsed)
    } else if let Ok(parsed) = value.parse::<bool>() {
        Value::Boolean(parsed)
    } else {
        Value::String(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_config_disable_enable_upgrade_remove() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("mods.lock");

        add_mod(&lock, "combat-core", "0.1.0").unwrap();
        configure_mod(
            &lock,
            "combat-core",
            "damage_multiplier",
            Value::Integer(120),
        )
        .unwrap();
        set_enabled(&lock, "combat-core", false).unwrap();
        upgrade_mod(&lock, "combat-core", "0.2.0").unwrap();

        let plugins = list_mods(&lock).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "combat-core");
        assert_eq!(plugins[0].version, "0.2.0");
        assert!(!plugins[0].enabled);
        assert_eq!(
            plugins[0].config.get("damage_multiplier"),
            Some(&Value::Integer(120))
        );

        set_enabled(&lock, "combat-core", true).unwrap();
        assert!(list_mods(&lock).unwrap()[0].enabled);
        remove_mod(&lock, "combat-core").unwrap();
        assert!(list_mods(&lock).unwrap().is_empty());
    }

    #[test]
    fn reads_existing_table_document_lock_file() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("mods.lock");
        fs::write(
            &lock,
            r#"
[plugins.combat-core]
version = "0.1.0"
enabled = true
"#,
        )
        .unwrap();

        let plugins = list_mods(&lock).unwrap();

        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "combat-core");
        assert_eq!(plugins[0].version, "0.1.0");
        assert!(plugins[0].enabled);
    }
}
