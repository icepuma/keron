use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReportError {
    #[error("failed to serialize report as JSON")]
    JsonSerialize {
        #[source]
        source: serde_json::Error,
    },
}
