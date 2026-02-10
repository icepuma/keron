use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use keron_domain::{
    CommandResource, LinkResource, ManifestSpec, PackageResource, PackageState, Resource,
    TemplateResource,
};

use crate::fs_util::normalize_path;
use crate::secrets::resolve_secret;
use mlua::{Error as LuaError, Lua, MultiValue, Result as LuaResult, Table, Value};

fn resolve_manifest_relative(base: &Path, raw: &str) -> PathBuf {
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        normalize_path(&candidate)
    } else {
        normalize_path(&base.join(candidate))
    }
}

fn parse_bool(opts: Option<&Table>, key: &str, default: bool) -> LuaResult<bool> {
    let Some(table) = opts else {
        return Ok(default);
    };

    match table.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Boolean(value) => Ok(value),
        _ => Err(LuaError::RuntimeError(format!("{key} must be a boolean"))),
    }
}

fn parse_string(opts: Option<&Table>, key: &str) -> LuaResult<Option<String>> {
    let Some(table) = opts else {
        return Ok(None);
    };

    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(value.to_str()?.to_owned())),
        _ => Err(LuaError::RuntimeError(format!("{key} must be a string"))),
    }
}

fn parse_string_map(opts: Option<&Table>, key: &str) -> LuaResult<BTreeMap<String, String>> {
    let Some(table) = opts else {
        return Ok(BTreeMap::new());
    };

    match table.get::<Value>(key)? {
        Value::Nil => Ok(BTreeMap::new()),
        Value::Table(vars) => {
            let mut out = BTreeMap::new();
            for pair in vars.pairs::<Value, Value>() {
                let (key, value) = pair?;
                let key = match key {
                    Value::String(text) => text.to_str()?.to_owned(),
                    _ => {
                        return Err(LuaError::RuntimeError(
                            "vars keys must be strings".to_string(),
                        ));
                    }
                };
                let value = match value {
                    Value::String(text) => text.to_str()?.to_owned(),
                    _ => {
                        return Err(LuaError::RuntimeError(
                            "vars values must be strings".to_string(),
                        ));
                    }
                };
                out.insert(key, value);
            }
            Ok(out)
        }
        _ => Err(LuaError::RuntimeError(format!("{key} must be a table"))),
    }
}

fn parse_package_state(opts: Option<&Table>) -> LuaResult<PackageState> {
    match parse_string(opts, "state")? {
        Some(value) if value == "present" => Ok(PackageState::Present),
        Some(value) if value == "absent" => Ok(PackageState::Absent),
        Some(value) => Err(LuaError::RuntimeError(format!(
            "package state must be 'present' or 'absent', got: {value}"
        ))),
        None => Ok(PackageState::Present),
    }
}

fn add_link(
    collector: &Rc<RefCell<ManifestSpec>>,
    manifest_dir: &Path,
    src: &str,
    dest: &str,
    opts: Option<&Table>,
) -> LuaResult<()> {
    let dest_path = PathBuf::from(dest);
    if !dest_path.is_absolute() {
        return Err(LuaError::RuntimeError(format!(
            "link destination must be absolute: {dest}"
        )));
    }

    let link = LinkResource {
        src: resolve_manifest_relative(manifest_dir, src),
        dest: normalize_path(&dest_path),
        force: parse_bool(opts, "force", false)?,
        mkdirs: parse_bool(opts, "mkdirs", false)?,
    };

    collector.borrow_mut().resources.push(Resource::Link(link));
    Ok(())
}

fn add_packages(
    collector: &Rc<RefCell<ManifestSpec>>,
    manager: &str,
    names: &Table,
    opts: Option<&Table>,
) -> LuaResult<()> {
    if manager.trim().is_empty() {
        return Err(LuaError::RuntimeError(
            "packages(manager, names, opts?) requires a non-empty manager string".to_string(),
        ));
    }

    if parse_string(opts, "provider")?.is_some() {
        return Err(LuaError::RuntimeError(
            "provider option is removed; use packages(\"<manager>\", {\"name\"}, { state = \"present\" })".to_string(),
        ));
    }

    let state = parse_package_state(opts)?;
    let mut package_count = 0usize;
    for value in names.sequence_values::<Value>() {
        let value = value?;
        let Value::String(text) = value else {
            return Err(LuaError::RuntimeError(
                "packages list must contain only strings".to_string(),
            ));
        };
        package_count += 1;
        let package_name = text.to_str()?.to_owned();
        collector
            .borrow_mut()
            .resources
            .push(Resource::Package(PackageResource {
                name: package_name,
                provider_hint: Some(manager.to_string()),
                state,
            }));
    }

    if package_count == 0 {
        return Err(LuaError::RuntimeError(
            "packages(manager, names, opts?) requires at least one package name".to_string(),
        ));
    }

    Ok(())
}

fn add_command(
    collector: &Rc<RefCell<ManifestSpec>>,
    binary: String,
    args: Option<Table>,
) -> LuaResult<()> {
    let parsed_args = match args {
        Some(table) => {
            let mut out = Vec::new();
            for value in table.sequence_values::<Value>() {
                let value = value?;
                let Value::String(text) = value else {
                    return Err(LuaError::RuntimeError(
                        "cmd args must be strings".to_string(),
                    ));
                };
                out.push(text.to_str()?.to_owned());
            }
            out
        }
        None => Vec::new(),
    };

    let command_resource = CommandResource {
        binary,
        args: parsed_args,
    };

    collector
        .borrow_mut()
        .resources
        .push(Resource::Command(command_resource));
    Ok(())
}

fn add_template(
    collector: &Rc<RefCell<ManifestSpec>>,
    manifest_dir: &Path,
    src: &str,
    dest: &str,
    opts: Option<&Table>,
) -> LuaResult<()> {
    let dest_path = PathBuf::from(dest);
    if !dest_path.is_absolute() {
        return Err(LuaError::RuntimeError(format!(
            "template destination must be absolute: {dest}"
        )));
    }

    let template = TemplateResource {
        src: resolve_manifest_relative(manifest_dir, src),
        dest: normalize_path(&dest_path),
        vars: parse_string_map(opts, "vars")?,
        force: parse_bool(opts, "force", false)?,
        mkdirs: parse_bool(opts, "mkdirs", false)?,
    };

    collector
        .borrow_mut()
        .resources
        .push(Resource::Template(template));
    Ok(())
}

fn create_lua() -> LuaResult<Lua> {
    let lua = Lua::new();
    let globals = lua.globals();

    // Keep the runtime predictable and avoid filesystem/process primitives from Lua stdlib.
    for key in [
        "io", "os", "package", "debug", "dofile", "loadfile", "require",
    ] {
        globals.set(key, Value::Nil)?;
    }

    Ok(lua)
}

// Single-pass parser/evaluator setup; splitting would reduce readability for shared Lua wiring.
#[allow(clippy::too_many_lines)]
fn evaluate_manifest_with_lua(
    manifest_path: &Path,
    manifest_dir: &Path,
    script: &str,
) -> LuaResult<(ManifestSpec, Vec<String>, BTreeSet<String>)> {
    let collector = Rc::new(RefCell::new(ManifestSpec::new(manifest_path.to_path_buf())));
    let sensitive_values: Rc<RefCell<BTreeSet<String>>> = Rc::new(RefCell::new(BTreeSet::new()));
    let lua = create_lua()?;
    let globals = lua.globals();

    {
        let sensitive = Rc::clone(&sensitive_values);
        let function = lua.create_function(move |_, args: MultiValue| {
            if args.len() != 1 {
                return Err(LuaError::RuntimeError(
                    "env(name) expects exactly one string argument".to_string(),
                ));
            }

            let Some(first) = args.front() else {
                return Err(LuaError::RuntimeError(
                    "env(name) expects exactly one string argument".to_string(),
                ));
            };

            let Value::String(name) = first else {
                return Err(LuaError::RuntimeError(
                    "env(name) expects exactly one string argument".to_string(),
                ));
            };

            let name = name.to_str()?.to_owned();
            env::var_os(&name).map_or_else(
                || {
                    Err(LuaError::RuntimeError(format!(
                        "env(\"{name}\") is not set in the current environment"
                    )))
                },
                |value| {
                    let resolved = value.to_string_lossy().into_owned();
                    sensitive.borrow_mut().insert(resolved.clone());
                    Ok(resolved)
                },
            )
        })?;
        globals.set("env", function)?;
    }

    {
        let sensitive = Rc::clone(&sensitive_values);
        let function = lua.create_function(move |_, args: MultiValue| {
            if args.len() != 1 {
                return Err(LuaError::RuntimeError(
                    "secret(uri) expects exactly one string argument".to_string(),
                ));
            }

            let Some(first) = args.front() else {
                return Err(LuaError::RuntimeError(
                    "secret(uri) expects exactly one string argument".to_string(),
                ));
            };

            let Value::String(uri) = first else {
                return Err(LuaError::RuntimeError(
                    "secret(uri) expects exactly one string argument".to_string(),
                ));
            };

            let uri = uri.to_str()?.to_owned();
            let resolved = resolve_secret(&uri).map_err(LuaError::RuntimeError)?;
            sensitive.borrow_mut().insert(resolved.clone());
            Ok(resolved)
        })?;
        globals.set("secret", function)?;
    }

    {
        let os_family = std::env::consts::OS;
        globals.set(
            "is_macos",
            lua.create_function(move |_, ()| Ok(os_family == "macos"))?,
        )?;
        globals.set(
            "is_linux",
            lua.create_function(move |_, ()| Ok(os_family == "linux"))?,
        )?;
        globals.set(
            "is_windows",
            lua.create_function(move |_, ()| Ok(os_family == "windows"))?,
        )?;
    }

    {
        let collector = Rc::clone(&collector);
        let manifest_dir = manifest_dir.to_path_buf();
        let function = lua.create_function(move |_, dependency: String| {
            let dependency_path = resolve_manifest_relative(&manifest_dir, &dependency);
            collector.borrow_mut().dependencies.push(dependency_path);
            Ok(())
        })?;
        globals.set("depends_on", function)?;
    }

    {
        let collector = Rc::clone(&collector);
        let manifest_dir = manifest_dir.to_path_buf();
        let function = lua.create_function(
            move |_, (src, dest, opts): (String, String, Option<Table>)| {
                add_link(&collector, &manifest_dir, &src, &dest, opts.as_ref())
            },
        )?;
        globals.set("link", function)?;
    }

    {
        let function = lua.create_function(move |_, _: MultiValue| {
            Err::<(), _>(LuaError::RuntimeError(
                "package(...) is removed; use packages(\"<manager>\", {\"name\"}, { state = \"present\" })".to_string(),
            ))
        })?;
        globals.set("package", function)?;
    }

    {
        let collector = Rc::clone(&collector);
        let function = lua.create_function(move |_, args: MultiValue| {
            match args.len() {
                2 | 3 => {}
                _ => {
                    return Err(LuaError::RuntimeError(
                        "packages(manager, names, opts?) expects 2 or 3 arguments".to_string(),
                    ));
                }
            }

            let Some(first) = args.front() else {
                return Err(LuaError::RuntimeError(
                    "packages(manager, names, opts?) expects manager string as first argument"
                        .to_string(),
                ));
            };
            let Some(second) = args.get(1) else {
                return Err(LuaError::RuntimeError(
                    "packages(manager, names, opts?) expects package name list as second argument"
                        .to_string(),
                ));
            };

            let manager = match first {
                Value::String(value) => value.to_str()?.to_owned(),
                Value::Table(_) => {
                    return Err(LuaError::RuntimeError(
                        "packages(...) now requires an explicit manager as first argument; use packages(\"<manager>\", {\"name\"}, opts)".to_string(),
                    ));
                }
                _ => {
                    return Err(LuaError::RuntimeError(
                        "packages(manager, names, opts?) expects manager string as first argument"
                            .to_string(),
                    ));
                }
            };

            let Value::Table(names) = second else {
                return Err(LuaError::RuntimeError(
                    "packages(manager, names, opts?) expects package name list as second argument"
                        .to_string(),
                ));
            };

            let opts = if let Some(third) = args.get(2) {
                match third {
                    Value::Nil => None,
                    Value::Table(table) => Some(table),
                    _ => {
                        return Err(LuaError::RuntimeError(
                            "packages(manager, names, opts?) expects options table as third argument"
                                .to_string(),
                        ));
                    }
                }
            } else {
                None
            };

            add_packages(&collector, &manager, names, opts)
        })?;
        globals.set("packages", function)?;
    }

    {
        let collector = Rc::clone(&collector);
        let function = lua.create_function(move |_, (binary, args): (String, Option<Table>)| {
            add_command(&collector, binary, args)
        })?;
        globals.set("cmd", function)?;
    }

    {
        let collector = Rc::clone(&collector);
        let manifest_dir = manifest_dir.to_path_buf();
        let function = lua.create_function(
            move |_, (src, dest, opts): (String, String, Option<Table>)| {
                add_template(&collector, &manifest_dir, &src, &dest, opts.as_ref())
            },
        )?;
        globals.set("template", function)?;
    }

    lua.load(script)
        .set_name(manifest_path.to_string_lossy())
        .exec()?;

    let sensitive = sensitive_values.borrow().clone();
    Ok((collector.borrow().clone(), Vec::new(), sensitive))
}

/// Evaluate a single manifest file into a manifest specification.
///
/// # Errors
///
/// Returns an error when the manifest cannot be read, parsed, or evaluated.
pub fn evaluate_manifest(path: &Path) -> Result<ManifestSpec> {
    let (manifest, _, _) = evaluate_manifest_with_warnings(path)?;
    Ok(manifest)
}

/// Evaluate a single manifest and collect non-fatal warnings and sensitive values.
///
/// # Errors
///
/// Returns an error when the manifest cannot be read, parsed, or evaluated.
pub fn evaluate_manifest_with_warnings(
    path: &Path,
) -> Result<(ManifestSpec, Vec<String>, BTreeSet<String>)> {
    let manifest_path = fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize manifest path: {}", path.display()))?;
    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("manifest path has no parent: {}", manifest_path.display()))?;

    let script = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read manifest: {}", manifest_path.display()))?;

    evaluate_manifest_with_lua(&manifest_path, &manifest_dir, &script)
        .map_err(|error| anyhow!("{}: {error}", manifest_path.display()))
}

/// Evaluate multiple manifests and return only manifest specifications.
///
/// # Errors
///
/// Returns an error when any manifest cannot be read, parsed, or evaluated.
pub fn evaluate_many(paths: &[PathBuf]) -> Result<Vec<ManifestSpec>> {
    let (manifests, _, _) = evaluate_many_with_warnings(paths)?;
    Ok(manifests)
}

/// Evaluate multiple manifests and aggregate warnings and sensitive values.
///
/// # Errors
///
/// Returns an error when any manifest cannot be read, parsed, or evaluated.
pub fn evaluate_many_with_warnings(
    paths: &[PathBuf],
) -> Result<(Vec<ManifestSpec>, Vec<String>, BTreeSet<String>)> {
    let mut manifests = Vec::with_capacity(paths.len());
    let mut warnings = Vec::new();
    let mut sensitive = BTreeSet::new();
    for path in paths {
        let (manifest, manifest_warnings, manifest_sensitive) =
            evaluate_manifest_with_warnings(path)?;
        manifests.push(manifest);
        warnings.extend(manifest_warnings);
        sensitive.extend(manifest_sensitive);
    }
    Ok((manifests, warnings, sensitive))
}

#[cfg(test)]
mod tests;
