use std::{fs, path::Path};
use toml::{Value, map::Map};
pub const EMPIRE_UPKEEP_NAME: &str = "empire-upkeep";
pub const EMPIRE_UPKEEP_VERSION: &str = "1.2.0";
pub fn try_run(args: impl IntoIterator<Item = String>) -> Result<bool, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    if args.first().map(|s| s.as_str()) != Some("mod") {
        return Ok(false);
    };
    run(&args[1..])?;
    Ok(true)
}
fn run(args: &[String]) -> Result<(), String> {
    let mut world = "world.toml".to_string();
    let mut p = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--world" {
            i += 1;
            world = args.get(i).ok_or("missing path after --world")?.clone()
        } else {
            p.push(args[i].clone())
        }
        i += 1;
    }
    match p.as_slice() {
        [a, n] if a == "install" => {
            install_mod(&world, n)?;
            println!("installed mod {n}")
        }
        [a, n] if a == "remove" => {
            remove_mod(&world, n)?;
            println!("removed mod {n}")
        }
        [a, n, k, v] if a == "config" => {
            configure_mod(&world, n, k, parse_value(v))?;
            println!("updated mod {n} config {k}")
        }
        [a] if a == "list" => {
            for m in list_mods(&world)? {
                println!("{} {}", m.name, m.version)
            }
        }
        _ => return Err("usage: mod [--world world.toml] install|remove|config|list".into()),
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledMod {
    pub name: String,
    pub version: String,
    pub config: Map<String, Value>,
}
pub fn install_mod(path: impl AsRef<Path>, name: &str) -> Result<(), String> {
    if name != EMPIRE_UPKEEP_NAME {
        return Err(format!("unknown mod: {name}"));
    }
    let mut d = read(path.as_ref())?;
    let mods = mods_mut(&mut d)?;
    if find(mods, name).is_some() {
        return Err(format!("mod already installed: {name}"));
    }
    let mut c = Map::new();
    c.insert("drone_cost".into(), Value::Integer(5));
    c.insert("room_superlinear".into(), Value::Integer(2));
    c.insert("onshortfall".into(), Value::String("damage".into()));
    let mut e = Map::new();
    e.insert("name".into(), Value::String(name.into()));
    e.insert(
        "version".into(),
        Value::String(EMPIRE_UPKEEP_VERSION.into()),
    );
    e.insert("config".into(), Value::Table(c));
    mods.push(Value::Table(e));
    write(path.as_ref(), &d)
}
pub fn remove_mod(path: impl AsRef<Path>, name: &str) -> Result<(), String> {
    let mut d = read(path.as_ref())?;
    let mods = mods_mut(&mut d)?;
    let i = find(mods, name).ok_or_else(|| format!("mod not installed: {name}"))?;
    mods.remove(i);
    write(path.as_ref(), &d)
}
pub fn configure_mod(
    path: impl AsRef<Path>,
    name: &str,
    key: &str,
    value: Value,
) -> Result<(), String> {
    let mut d = read(path.as_ref())?;
    let mods = mods_mut(&mut d)?;
    let i = find(mods, name).ok_or_else(|| format!("mod not installed: {name}"))?;
    let t = mods[i].as_table_mut().ok_or("invalid mod")?;
    let c = t
        .entry("config")
        .or_insert_with(|| Value::Table(Map::new()))
        .as_table_mut()
        .ok_or("invalid config")?;
    c.insert(key.into(), value);
    write(path.as_ref(), &d)
}
pub fn list_mods(path: impl AsRef<Path>) -> Result<Vec<InstalledMod>, String> {
    let d = read(path.as_ref())?;
    Ok(d.get("mods")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| {
            let t = v.as_table()?;
            Some(InstalledMod {
                name: t.get("name")?.as_str()?.into(),
                version: t.get("version")?.as_str()?.into(),
                config: t
                    .get("config")
                    .and_then(Value::as_table)
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect())
}
fn read(p: &Path) -> Result<Value, String> {
    match fs::read_to_string(p) {
        Ok(s) => s
            .parse()
            .map_err(|e| format!("failed to parse {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Table(Map::new())),
        Err(e) => Err(e.to_string()),
    }
}
fn write(p: &Path, d: &Value) -> Result<(), String> {
    fs::write(p, toml::to_string_pretty(d).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
fn mods_mut(d: &mut Value) -> Result<&mut Vec<Value>, String> {
    d.as_table_mut()
        .ok_or("root must be table")?
        .entry("mods")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("mods must be array".into())
}
fn find(mods: &[Value], name: &str) -> Option<usize> {
    mods.iter().position(|m| {
        m.as_table()
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            == Some(name)
    })
}
fn parse_value(s: &str) -> Value {
    if let Ok(v) = s.parse::<i64>() {
        Value::Integer(v)
    } else if let Ok(v) = s.parse::<bool>() {
        Value::Boolean(v)
    } else {
        Value::String(s.into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn install_config_remove() {
        let p = std::env::temp_dir().join("swarm-mod-cli-test.toml");
        let _ = fs::remove_file(&p);
        install_mod(&p, EMPIRE_UPKEEP_NAME).unwrap();
        configure_mod(&p, EMPIRE_UPKEEP_NAME, "drone_cost", Value::Integer(7)).unwrap();
        assert_eq!(
            list_mods(&p).unwrap()[0].config.get("drone_cost"),
            Some(&Value::Integer(7))
        );
        remove_mod(&p, EMPIRE_UPKEEP_NAME).unwrap();
        assert!(list_mods(&p).unwrap().is_empty());
        let _ = fs::remove_file(&p);
    }
}
