use std::io;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("failed to create temporary clone directory")]
    TempDirCreate {
        #[source]
        source: io::Error,
    },
    #[error("failed to prepare clone from {repo_url}")]
    PrepareClone {
        repo_url: String,
        #[source]
        source: Box<gix::clone::Error>,
    },
    #[error("invalid git ref name: {reference}")]
    InvalidGitRefName { reference: String },
    #[error("failed to fetch remote repository {repo_url}")]
    FetchRemote {
        repo_url: String,
        #[source]
        source: Box<gix::clone::fetch::Error>,
    },
    #[error("failed to check out repository {repo_url}")]
    CheckoutRemote {
        repo_url: String,
        #[source]
        source: Box<gix::clone::checkout::main_worktree::Error>,
    },
    #[error("invalid source URL: {input}")]
    InvalidSourceUrl {
        input: String,
        #[source]
        source: url::ParseError,
    },
    #[error("invalid scp-style source: {input}")]
    InvalidScpStyleSource {
        input: String,
        #[source]
        source: url::ParseError,
    },
    #[error("source path cannot be empty")]
    EmptySourcePath,
    #[error("source must include a repository path")]
    MissingRepositoryPath,
    #[error("query parameters are not supported; keron always checks out ref \"main\"")]
    QueryParametersNotSupported,
    #[error("\"//\" subdirectory delimiter is not supported; use a standard path (e.g. {example})")]
    LegacySubdirDelimiter { example: &'static str },
    #[error("remote source must include a URL scheme")]
    MissingRemoteScheme,
    #[error("invalid scp-style source; expected user@host:path")]
    InvalidScpStyleFormat,
    #[error(
        "http:// repositories are not supported; use https:// or git:// for public repositories"
    )]
    HttpSchemeNotSupported,
    #[error("file:// repositories are not supported; only public network repositories are allowed")]
    FileSchemeNotSupported,
    #[error(
        "unsupported repository scheme \"{scheme}\"; expected https://, git://, or git@host:path"
    )]
    UnsupportedRepositoryScheme { scheme: String },
    #[error("repository URL must include a host")]
    MissingRepositoryHost,
    #[error("repository URLs with passwords are not supported")]
    PasswordNotSupported,
    #[error(
        "authenticated repository URLs are not supported; only public repositories are allowed"
    )]
    AuthenticatedUrlsNotSupported,
    #[error("manifest subdirectory cannot contain '..'")]
    ParentDirNotAllowed,
    #[error("manifest subdirectory must be relative")]
    AbsoluteSubdirNotAllowed,
    #[error("manifest directory \"{subdir}\" does not exist in cloned repository")]
    MissingManifestDirectory { subdir: String },
    #[error("failed to canonicalize checkout root {path}")]
    CanonicalizeCheckoutRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to canonicalize manifest directory {path}")]
    CanonicalizeManifestDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("manifest directory escapes cloned repository checkout")]
    ManifestDirectoryEscapesCheckout,
}
