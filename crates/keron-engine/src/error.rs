use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestEvalError {
    #[error("failed to canonicalize manifest path: {path}")]
    CanonicalizePath {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("manifest path has no parent: {path}")]
    MissingManifestParent { path: PathBuf },
    #[error("failed to read manifest: {path}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {source}")]
    LuaRuntime {
        path: PathBuf,
        #[source]
        source: mlua::Error,
    },
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("no package provider available (hint: {hint:?})")]
    NoProviderAvailable { hint: Option<String> },
    #[error("failed to execute {program} {args}")]
    CommandSpawn {
        program: &'static str,
        args: String,
        #[source]
        source: io::Error,
    },
    #[error("command failed: {program} {args} (exit: {status})")]
    CommandFailed {
        program: &'static str,
        args: String,
        status: ExitStatus,
    },
}

#[derive(Debug, Error)]
pub enum PlanningError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("manifest root does not exist: {root}")]
    RootDoesNotExist { root: PathBuf },
    #[error("manifest root must be a directory: {root}")]
    RootIsNotDirectory { root: PathBuf },
    #[error("failed while walking manifest directory")]
    Walk {
        #[source]
        source: walkdir::Error,
    },
    #[error("failed to canonicalize manifest path: {path}")]
    CanonicalizePath {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("dependency graph has missing nodes:\n  - {details}")]
    MissingNodes { details: String },
    #[error("{message}")]
    Invariant { message: String },
    #[error("dependency cycle detected among: {cycle}")]
    CycleDetected { cycle: String },
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error("no manifests found under {folder} (expected files ending with .lua)")]
    NoManifests { folder: PathBuf },
    #[error(transparent)]
    ManifestEval(#[from] ManifestEvalError),
    #[error(transparent)]
    Planning(#[from] PlanningError),
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("binary \"{binary}\" not found on PATH")]
    CommandBinaryNotFound { binary: String },
    #[error("failed to execute command: {binary}")]
    CommandSpawn {
        binary: String,
        #[source]
        source: io::Error,
    },
    #[error("command exited with non-zero status: {status}")]
    CommandFailed { status: ExitStatus },
    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to render template {path}")]
    TemplateRender {
        path: PathBuf,
        #[source]
        source: tera::Error,
    },
    #[error("{message}")]
    Invariant { message: String },
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("invalid URI: expected scheme://path, got \"{uri}\"")]
    InvalidUri { uri: String },
    #[error("unsupported scheme \"{scheme}\" in secret URI \"{uri}\"")]
    UnsupportedScheme { scheme: String, uri: String },
    #[error("{scheme}:// URI requires {expected}, got \"{uri}\"")]
    InvalidSchemePath {
        scheme: &'static str,
        expected: &'static str,
        uri: String,
    },
    #[error("secret(\"{uri}\") requires the \"{binary}\" CLI to be installed and on PATH")]
    CliMissing { uri: String, binary: &'static str },
    #[error("failed to execute \"{provider_binary}\" ({binary_path}) for secret(\"{uri}\")")]
    CommandSpawn {
        provider_binary: String,
        binary_path: PathBuf,
        uri: String,
        #[source]
        source: io::Error,
    },
    #[error("\"{provider_binary}\" exited with {status}: {stderr}")]
    CommandFailed {
        provider_binary: String,
        status: ExitStatus,
        stderr: String,
    },
}
