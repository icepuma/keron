use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use keron_domain::{
    AbsolutePath, CommandResource, LinkResource, ManifestSpec, PackageManagerName, PackageName,
    PackageResource, PackageState, Resource, TemplateResource,
};

use crate::error::ManifestEvalError;
use crate::fs_util::normalize_path;
use crate::secrets::resolve_secret;
use mlua::{Error as LuaError, Lua, MultiValue, Result as LuaResult, Table, Value};

type ManifestEvalOutput = (ManifestSpec, Vec<String>, BTreeSet<String>);
type ManifestBatchEvalOutput = (Vec<ManifestSpec>, Vec<String>, BTreeSet<String>);

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

fn parse_elevate(opts: Option<&Table>) -> LuaResult<bool> {
    parse_bool(opts, "elevate", false)
}

fn add_link(
    collector: &Rc<RefCell<ManifestSpec>>,
    manifest_dir: &Path,
    src: &str,
    dest: &str,
    opts: Option<&Table>,
) -> LuaResult<()> {
    let dest_path = PathBuf::from(dest);
    let normalized_dest = normalize_path(&dest_path);
    let dest = AbsolutePath::try_from(normalized_dest).map_err(|_| {
        LuaError::RuntimeError(format!("link destination must be absolute: {dest}"))
    })?;

    let link = LinkResource {
        src: resolve_manifest_relative(manifest_dir, src),
        dest,
        force: parse_bool(opts, "force", false)?,
        mkdirs: parse_bool(opts, "mkdirs", false)?,
        elevate: parse_elevate(opts)?,
    };

    collector.borrow_mut().resources.push(Resource::Link(link));
    Ok(())
}

fn add_install_packages(
    collector: &Rc<RefCell<ManifestSpec>>,
    manager: &str,
    names: &Table,
    opts: Option<&Table>,
) -> LuaResult<()> {
    let manager = PackageManagerName::try_from(manager.to_string()).map_err(|_| {
        LuaError::RuntimeError(
            "install_packages(manager, names, opts?) requires a non-empty manager string"
                .to_string(),
        )
    })?;

    if parse_string(opts, "provider")?.is_some() {
        return Err(LuaError::RuntimeError(
            "provider option is removed; use install_packages(\"<manager>\", {\"name\"}, { state = \"present\" })".to_string(),
        ));
    }

    let state = parse_package_state(opts)?;
    let mut package_count = 0usize;
    for value in names.sequence_values::<Value>() {
        let value = value?;
        let Value::String(text) = value else {
            return Err(LuaError::RuntimeError(
                "install_packages names list must contain only strings".to_string(),
            ));
        };
        package_count += 1;
        let package_name = PackageName::try_from(text.to_str()?.to_owned()).map_err(|_| {
            LuaError::RuntimeError(
                "install_packages names list must contain only non-empty strings".to_string(),
            )
        })?;
        collector
            .borrow_mut()
            .resources
            .push(Resource::Package(PackageResource {
                name: package_name,
                provider_hint: Some(manager.clone()),
                state,
                elevate: parse_elevate(opts)?,
            }));
    }

    if package_count == 0 {
        return Err(LuaError::RuntimeError(
            "install_packages(manager, names, opts?) requires at least one package name"
                .to_string(),
        ));
    }

    Ok(())
}

fn add_command(
    collector: &Rc<RefCell<ManifestSpec>>,
    binary: String,
    args: Option<Table>,
    opts: Option<&Table>,
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
        elevate: parse_elevate(opts)?,
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
    let normalized_dest = normalize_path(&dest_path);
    let dest = AbsolutePath::try_from(normalized_dest).map_err(|_| {
        LuaError::RuntimeError(format!("template destination must be absolute: {dest}"))
    })?;

    let template = TemplateResource {
        src: resolve_manifest_relative(manifest_dir, src),
        dest,
        vars: parse_string_map(opts, "vars")?,
        force: parse_bool(opts, "force", false)?,
        mkdirs: parse_bool(opts, "mkdirs", false)?,
        elevate: parse_elevate(opts)?,
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

    let global = lua.create_table()?;
    if let Some(home) = dirs::home_dir() {
        global.set("HOME", home.to_string_lossy().into_owned())?;
    } else {
        global.set("HOME", Value::Nil)?;
    }
    globals.set("global", global)?;

    Ok(lua)
}

fn register_env_function(
    lua: &Lua,
    globals: &Table,
    sensitive_values: &Rc<RefCell<BTreeSet<String>>>,
) -> LuaResult<()> {
    let sensitive = Rc::clone(sensitive_values);
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
    Ok(())
}

fn register_secret_function(
    lua: &Lua,
    globals: &Table,
    sensitive_values: &Rc<RefCell<BTreeSet<String>>>,
) -> LuaResult<()> {
    let sensitive = Rc::clone(sensitive_values);
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
        let resolved =
            resolve_secret(&uri).map_err(|error| LuaError::RuntimeError(error.to_string()))?;
        sensitive.borrow_mut().insert(resolved.clone());
        Ok(resolved)
    })?;
    globals.set("secret", function)?;
    Ok(())
}

fn register_os_functions(lua: &Lua, globals: &Table) -> LuaResult<()> {
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
    Ok(())
}

fn register_depends_on_function(
    lua: &Lua,
    globals: &Table,
    collector: &Rc<RefCell<ManifestSpec>>,
    manifest_dir: &Path,
) -> LuaResult<()> {
    let collector = Rc::clone(collector);
    let manifest_dir = manifest_dir.to_path_buf();
    let function = lua.create_function(move |_, dependency: String| {
        let dependency_path = resolve_manifest_relative(&manifest_dir, &dependency);
        collector
            .borrow_mut()
            .dependencies
            .push(dependency_path.into());
        Ok(())
    })?;
    globals.set("depends_on", function)?;
    Ok(())
}

fn register_link_function(
    lua: &Lua,
    globals: &Table,
    collector: &Rc<RefCell<ManifestSpec>>,
    manifest_dir: &Path,
) -> LuaResult<()> {
    let collector = Rc::clone(collector);
    let manifest_dir = manifest_dir.to_path_buf();
    let function = lua.create_function(
        move |_, (src, dest, opts): (String, String, Option<Table>)| {
            add_link(&collector, &manifest_dir, &src, &dest, opts.as_ref())
        },
    )?;
    globals.set("link", function)?;
    Ok(())
}

fn register_install_packages_function(
    lua: &Lua,
    globals: &Table,
    collector: &Rc<RefCell<ManifestSpec>>,
) -> LuaResult<()> {
    let collector = Rc::clone(collector);
    let function = lua.create_function(move |_, args: MultiValue| {
        match args.len() {
            2 | 3 => {}
            _ => {
                return Err(LuaError::RuntimeError(
                    "install_packages(manager, names, opts?) expects 2 or 3 arguments".to_string(),
                ));
            }
        }

        let Some(first) = args.front() else {
            return Err(LuaError::RuntimeError(
                "install_packages(manager, names, opts?) expects manager string as first argument"
                    .to_string(),
            ));
        };
        let Some(second) = args.get(1) else {
            return Err(LuaError::RuntimeError(
                "install_packages(manager, names, opts?) expects package name list as second argument"
                    .to_string(),
            ));
        };

        let manager = match first {
            Value::String(value) => value.to_str()?.to_owned(),
            Value::Table(_) => {
                return Err(LuaError::RuntimeError(
                    "install_packages(...) requires an explicit manager as first argument; use install_packages(\"<manager>\", {\"name\"}, opts)".to_string(),
                ));
            }
            _ => {
                return Err(LuaError::RuntimeError(
                    "install_packages(manager, names, opts?) expects manager string as first argument"
                        .to_string(),
                ));
            }
        };

        let Value::Table(names) = second else {
            return Err(LuaError::RuntimeError(
                "install_packages(manager, names, opts?) expects package name list as second argument"
                    .to_string(),
            ));
        };

        let opts = if let Some(third) = args.get(2) {
            match third {
                Value::Nil => None,
                Value::Table(table) => Some(table),
                _ => {
                    return Err(LuaError::RuntimeError(
                        "install_packages(manager, names, opts?) expects options table as third argument"
                            .to_string(),
                    ));
                }
            }
        } else {
            None
        };

        add_install_packages(&collector, &manager, names, opts)
    })?;
    globals.set("install_packages", function)?;
    Ok(())
}

fn register_cmd_function(
    lua: &Lua,
    globals: &Table,
    collector: &Rc<RefCell<ManifestSpec>>,
) -> LuaResult<()> {
    let collector = Rc::clone(collector);
    let function = lua.create_function(move |_, args: MultiValue| {
        if !(1..=3).contains(&args.len()) {
            return Err(LuaError::RuntimeError(
                "cmd(binary, args?, opts?) expects 1 to 3 arguments".to_string(),
            ));
        }

        let Some(first) = args.front() else {
            return Err(LuaError::RuntimeError(
                "cmd(binary, args?, opts?) expects binary string as first argument".to_string(),
            ));
        };
        let Value::String(binary) = first else {
            return Err(LuaError::RuntimeError(
                "cmd(binary, args?, opts?) expects binary string as first argument".to_string(),
            ));
        };
        let binary = binary.to_str()?.to_owned();

        let parsed_args = match args.get(1) {
            Some(Value::Nil) | None => None,
            Some(Value::Table(table)) => Some(table.clone()),
            Some(_) => {
                return Err(LuaError::RuntimeError(
                    "cmd(binary, args?, opts?) expects args table as second argument".to_string(),
                ));
            }
        };

        let opts = match args.get(2) {
            Some(Value::Nil) | None => None,
            Some(Value::Table(table)) => Some(table),
            Some(_) => {
                return Err(LuaError::RuntimeError(
                    "cmd(binary, args?, opts?) expects options table as third argument".to_string(),
                ));
            }
        };

        add_command(&collector, binary, parsed_args, opts)
    })?;
    globals.set("cmd", function)?;
    Ok(())
}

fn register_template_function(
    lua: &Lua,
    globals: &Table,
    collector: &Rc<RefCell<ManifestSpec>>,
    manifest_dir: &Path,
) -> LuaResult<()> {
    let collector = Rc::clone(collector);
    let manifest_dir = manifest_dir.to_path_buf();
    let function = lua.create_function(
        move |_, (src, dest, opts): (String, String, Option<Table>)| {
            add_template(&collector, &manifest_dir, &src, &dest, opts.as_ref())
        },
    )?;
    globals.set("template", function)?;
    Ok(())
}

fn evaluate_manifest_with_lua(
    manifest_path: &Path,
    manifest_dir: &Path,
    script: &str,
) -> LuaResult<ManifestEvalOutput> {
    let collector = Rc::new(RefCell::new(ManifestSpec::new(manifest_path.to_path_buf())));
    let sensitive_values: Rc<RefCell<BTreeSet<String>>> = Rc::new(RefCell::new(BTreeSet::new()));
    let lua = create_lua()?;
    let globals = lua.globals();

    register_env_function(&lua, &globals, &sensitive_values)?;
    register_secret_function(&lua, &globals, &sensitive_values)?;
    register_os_functions(&lua, &globals)?;
    register_depends_on_function(&lua, &globals, &collector, manifest_dir)?;
    register_link_function(&lua, &globals, &collector, manifest_dir)?;
    register_install_packages_function(&lua, &globals, &collector)?;
    register_cmd_function(&lua, &globals, &collector)?;
    register_template_function(&lua, &globals, &collector, manifest_dir)?;

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
pub fn evaluate_manifest(path: &Path) -> std::result::Result<ManifestSpec, ManifestEvalError> {
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
) -> std::result::Result<ManifestEvalOutput, ManifestEvalError> {
    let manifest_path =
        fs::canonicalize(path).map_err(|source| ManifestEvalError::CanonicalizePath {
            path: path.to_path_buf(),
            source,
        })?;
    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| ManifestEvalError::MissingManifestParent {
            path: manifest_path.clone(),
        })?;

    let script =
        fs::read_to_string(&manifest_path).map_err(|source| ManifestEvalError::ReadManifest {
            path: manifest_path.clone(),
            source,
        })?;

    evaluate_manifest_with_lua(&manifest_path, &manifest_dir, &script).map_err(|source| {
        ManifestEvalError::LuaRuntime {
            path: manifest_path,
            source,
        }
    })
}

/// Evaluate multiple manifests and return only manifest specifications.
///
/// # Errors
///
/// Returns an error when any manifest cannot be read, parsed, or evaluated.
pub fn evaluate_many(
    paths: &[PathBuf],
) -> std::result::Result<Vec<ManifestSpec>, ManifestEvalError> {
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
) -> std::result::Result<ManifestBatchEvalOutput, ManifestEvalError> {
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
