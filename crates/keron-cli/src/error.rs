use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    ArgumentParse(#[from] clap::Error),
    #[error(transparent)]
    Source(#[from] keron_source::SourceError),
    #[error(transparent)]
    Pipeline(#[from] keron_engine::PipelineError),
    #[error(transparent)]
    Apply(#[from] keron_engine::ApplyError),
    #[error(transparent)]
    Report(#[from] keron_report::ReportError),
}
