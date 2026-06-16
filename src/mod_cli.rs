use std::{fs, path::Path, process::Command};
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
        [a, u] if a == "add" => {
            let m = add_mod(&world, u, None)?;
            println!("added mod {} {}", m.name, m.version)
        }
        [a, u, f, t] if a == "add" && f == "--tag" => {
            let m = add_mod(&world, u, Some(t))?;
            println!("added mod {} {}", m.name, m.version)
        }
        [a, n] if a == "update" => {
            let m = update_mod(&world, n)?;
            println!("updated mod {} {}", m.name, m.version)
        }
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
        _ => {
            return Err(
                "usage: mod [--world world.toml] add <url> [--tag <tag>]|update <name>|install|remove|config|list".into(),
            )
        }
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledMod {
    pub name: String,
    pub version: String,
    pub config: Map<String, Value>,
}
#[derive(Debug, Clone)]
struct ModSpec {
    name: String,
    source: Option<String>,
    version: String,
    config: Map<String, Value>,
}
pub fn install_mod(path: impl AsRef<Path>, name: &str) -> Result<(), String> {
    if name != EMPIRE_UPKEEP_NAME {
        return Err(format!("unknown mod: {name}"));
    }
    let mut c = Map::new();
    c.insert("drone_cost".into(), Value::Integer(5));
    c.insert("room_superlinear".into(), Value::Integer(2));
    c.insert("onshortfall".into(), Value::String("damage".into()));
    install_mod_spec(
        path.as_ref(),
        ModSpec {
            name: name.into(),
            source: None,
            version: EMPIRE_UPKEEP_VERSION.into(),
            config: c,
        },
    )
}
pub fn add_mod(
    path: impl AsRef<Path>,
    url: &str,
    tag: Option<&str>,
) -> Result<InstalledMod, String> {
    let path = path.as_ref();
    let name = infer_repo_name(url)?;
    let dir = mod_checkout_dir(path, &name);
    if dir.exists() {
        return Err(format!("mod checkout already exists: {}", dir.display()));
    }
    let parent = dir
        .parent()
        .ok_or_else(|| format!("invalid mod checkout path: {}", dir.display()))?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    git(
        None,
        &["clone", url, dir.to_str().ok_or("invalid checkout path")?],
    )?;
    let selected_tag = match tag {
        Some(tag) => tag.to_string(),
        None => latest_tag(&dir)?,
    };
    git(Some(&dir), &["checkout", &selected_tag])?;
    let rev = git_stdout(Some(&dir), &["rev-parse", "HEAD"])?;
    let version = tag_to_version(&selected_tag);
    let spec = ModSpec {
        name,
        source: Some(url.into()),
        version,
        config: Map::new(),
    };
    install_mod_spec(path, spec.clone())?;
    write_lock(path, &spec, &rev)?;
    Ok(installed_from_spec(spec))
}
pub fn update_mod(path: impl AsRef<Path>, name: &str) -> Result<InstalledMod, String> {
    let path = path.as_ref();
    let mut d = read(path)?;
    let mods = mods_mut(&mut d)?;
    let i = find(mods, name).ok_or_else(|| format!("mod not installed: {name}"))?;
    let t = mods[i].as_table_mut().ok_or("invalid mod")?;
    let source = t
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("mod has no git source: {name}"))?
        .to_string();
    let dir = mod_checkout_dir(path, name);
    if !dir.exists() {
        return Err(format!("mod checkout not found: {}", dir.display()));
    }
    git(Some(&dir), &["fetch", "--tags", "--force"])?;
    let selected_tag = latest_tag(&dir)?;
    git(Some(&dir), &["checkout", &selected_tag])?;
    let rev = git_stdout(Some(&dir), &["rev-parse", "HEAD"])?;
    let version = tag_to_version(&selected_tag);
    t.insert("version".into(), Value::String(version.clone()));
    write(path, &d)?;
    let spec = ModSpec {
        name: name.into(),
        source: Some(source),
        version,
        config: Map::new(),
    };
    write_lock(path, &spec, &rev)?;
    Ok(installed_from_spec(spec))
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
fn install_mod_spec(path: &Path, spec: ModSpec) -> Result<(), String> {
    let mut d = read(path)?;
    let mods = mods_mut(&mut d)?;
    if find(mods, &spec.name).is_some() {
        return Err(format!("mod already installed: {}", spec.name));
    }
    mods.push(Value::Table(table_from_spec(&spec, true)));
    write(path, &d)
}
fn installed_from_spec(spec: ModSpec) -> InstalledMod {
    InstalledMod {
        name: spec.name,
        version: spec.version,
        config: spec.config,
    }
}
fn table_from_spec(spec: &ModSpec, include_config: bool) -> Map<String, Value> {
    let mut e = Map::new();
    e.insert("name".into(), Value::String(spec.name.clone()));
    if let Some(source) = &spec.source {
        e.insert("source".into(), Value::String(source.clone()));
    }
    e.insert("version".into(), Value::String(spec.version.clone()));
    if include_config {
        e.insert("config".into(), Value::Table(spec.config.clone()));
    }
    e
}
fn write_lock(path: &Path, spec: &ModSpec, rev: &str) -> Result<(), String> {
    let lock_path = lock_path(path);
    let mut d = read(&lock_path)?;
    let mods = mods_mut(&mut d)?;
    let mut e = table_from_spec(spec, false);
    e.insert("rev".into(), Value::String(rev.into()));
    match find(mods, &spec.name) {
        Some(i) => mods[i] = Value::Table(e),
        None => mods.push(Value::Table(e)),
    }
    write(&lock_path, &d)
}
fn lock_path(path: &Path) -> std::path::PathBuf {
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("mods.lock")
}
fn mod_checkout_dir(path: &Path, name: &str) -> std::path::PathBuf {
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("mods")
        .join(name)
}
fn infer_repo_name(url: &str) -> Result<String, String> {
    let trimmed = url.trim_end_matches('/');
    let segment = trimmed
        .rsplit(['/', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("could not infer mod name from url: {url}"))?;
    let name = segment.strip_suffix(".git").unwrap_or(segment);
    if name.is_empty() {
        return Err(format!("could not infer mod name from url: {url}"));
    }
    Ok(name.into())
}
fn latest_tag(dir: &Path) -> Result<String, String> {
    git_stdout(Some(dir), &["tag", "--sort=-v:refname"])?
        .lines()
        .next()
        .map(str::to_string)
        .ok_or_else(|| "mod repository has no tags".into())
}
fn tag_to_version(tag: &str) -> String {
    tag.strip_prefix('v').unwrap_or(tag).into()
}
fn git(dir: Option<&Path>, args: &[&str]) -> Result<(), String> {
    let output = command(dir, args).output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
fn git_stdout(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let output = command(dir, args).output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
fn command(dir: Option<&Path>, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.args(args);
    if let Some(dir) = dir {
        c.current_dir(dir);
    }
    c
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
    use std::path::PathBuf;

    #[test]
    fn install_config_remove() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("world.toml");
        install_mod(&p, EMPIRE_UPKEEP_NAME).unwrap();
        configure_mod(&p, EMPIRE_UPKEEP_NAME, "drone_cost", Value::Integer(7)).unwrap();
        assert_eq!(
            list_mods(&p).unwrap()[0].config.get("drone_cost"),
            Some(&Value::Integer(7))
        );
        remove_mod(&p, EMPIRE_UPKEEP_NAME).unwrap();
        assert!(list_mods(&p).unwrap().is_empty());
    }

    #[test]
    fn add_clones_checkout_tag_and_writes_lock() {
        let td = tempfile::tempdir().unwrap();
        let remote = create_repo(td.path().join("empire-upkeep.git"), &[("v1.0.0", "one")]);
        let world = td.path().join("world.toml");

        let m = add_mod(&world, remote.to_str().unwrap(), Some("v1.0.0")).unwrap();

        assert_eq!(m.name, "empire-upkeep");
        assert_eq!(m.version, "1.0.0");
        let mods = list_mods(&world).unwrap();
        assert_eq!(mods[0].name, "empire-upkeep");
        assert_eq!(mods[0].version, "1.0.0");
        let lock = read(&td.path().join("mods.lock")).unwrap();
        let locked = lock.get("mods").and_then(Value::as_array).unwrap()[0]
            .as_table()
            .unwrap();
        assert_eq!(
            locked.get("name").and_then(Value::as_str),
            Some("empire-upkeep")
        );
        assert!(locked.get("rev").and_then(Value::as_str).unwrap().len() >= 40);
    }

    #[test]
    fn add_without_tag_uses_latest_tag() {
        let td = tempfile::tempdir().unwrap();
        let remote = create_repo(
            td.path().join("resource-decay.git"),
            &[("v1.0.0", "one"), ("v1.2.0", "two")],
        );
        let world = td.path().join("world.toml");

        let m = add_mod(&world, remote.to_str().unwrap(), None).unwrap();

        assert_eq!(m.name, "resource-decay");
        assert_eq!(m.version, "1.2.0");
    }

    #[test]
    fn update_checks_out_latest_tag_and_updates_lock() {
        let td = tempfile::tempdir().unwrap();
        let remote = create_repo(td.path().join("fog-of-war.git"), &[("v1.0.0", "one")]);
        let world = td.path().join("world.toml");
        add_mod(&world, remote.to_str().unwrap(), Some("v1.0.0")).unwrap();
        let old_rev = locked_rev(td.path());
        add_commit_and_tag(&remote, "v1.1.0", "two");

        let m = update_mod(&world, "fog-of-war").unwrap();

        assert_eq!(m.version, "1.1.0");
        assert_ne!(locked_rev(td.path()), old_rev);
        assert_eq!(list_mods(&world).unwrap()[0].version, "1.1.0");
    }

    fn create_repo(path: PathBuf, tags: &[(&str, &str)]) -> PathBuf {
        fs::create_dir_all(&path).unwrap();
        git(Some(&path), &["init", "-b", "main"]).unwrap();
        git(Some(&path), &["config", "user.email", "test@example.com"]).unwrap();
        git(Some(&path), &["config", "user.name", "Test User"]).unwrap();
        for (tag, contents) in tags {
            fs::write(path.join("mod.toml"), contents).unwrap();
            git(Some(&path), &["add", "mod.toml"]).unwrap();
            git(Some(&path), &["commit", "-m", tag]).unwrap();
            git(Some(&path), &["tag", tag]).unwrap();
        }
        path
    }

    fn add_commit_and_tag(path: &Path, tag: &str, contents: &str) {
        fs::write(path.join("mod.toml"), contents).unwrap();
        git(Some(path), &["add", "mod.toml"]).unwrap();
        git(Some(path), &["commit", "-m", tag]).unwrap();
        git(Some(path), &["tag", tag]).unwrap();
    }

    fn locked_rev(path: &Path) -> String {
        read(&path.join("mods.lock")).unwrap()["mods"][0]["rev"]
            .as_str()
            .unwrap()
            .into()
    }
}
