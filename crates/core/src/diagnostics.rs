//! Privacy-safe diagnostics for handled errors. Only code-defined steps and error categories leave the app.

use std::fmt;

#[derive(Debug)]
struct StepError {
    name: &'static str,
    source: anyhow::Error,
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

impl std::error::Error for StepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Adds a code-defined operation name to the error chain.
pub trait ResultStep<T> {
    fn step(self, name: &'static str) -> anyhow::Result<T>;
}

impl<T, E: Into<anyhow::Error>> ResultStep<T> for Result<T, E> {
    fn step(self, name: &'static str) -> anyhow::Result<T> {
        self.map_err(|error| {
            StepError {
                name,
                source: error.into(),
            }
            .into()
        })
    }
}

/// Formats the original error for display on the device.
pub fn local_error_message(error: &anyhow::Error) -> String {
    error
        .chain()
        .filter(|cause| !cause.is::<StepError>())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

#[derive(Clone, Debug)]
pub struct ErrorDiagnostic {
    pub operation: &'static str,
    pub chain: Vec<String>,
}

impl ErrorDiagnostic {
    pub fn from_error(error: &anyhow::Error) -> Self {
        let mut operation = "unknown";
        let chain = error
            .chain()
            .filter_map(|cause| {
                if let Some(step) = cause.downcast_ref::<StepError>() {
                    operation = step.name;
                    Some(step.to_string())
                } else if let Some(sqlite) = cause.downcast_ref::<rusqlite::Error>() {
                    let code = match sqlite {
                        rusqlite::Error::SqlInputError { error, .. } => Some(error),
                        _ => sqlite.sqlite_error(),
                    };
                    Some(match code {
                        Some(code) => format!("sqlite.{:?} ({})", code.code, code.extended_code),
                        None => "sqlite.invalid_query_or_row".into(),
                    })
                } else if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                    Some(format!("io.{:?}", io.kind()))
                } else if let Some(json) = cause.downcast_ref::<serde_json::Error>() {
                    Some(format!("json.{:?}", json.classify()))
                } else if cause.is::<std::num::TryFromIntError>() {
                    Some("integer_out_of_range".into())
                } else if cause.is::<std::num::ParseIntError>() {
                    Some("invalid_integer".into())
                } else {
                    None
                }
            })
            .collect();
        Self { operation, chain }
    }
}
