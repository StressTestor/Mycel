use std::{error::Error, fmt};

use crate::{cli::ValidationError, headless::HeadlessError, runtime::RuntimeAdapterError};

#[derive(Debug)]
pub enum CliError {
    Validation(ValidationError),
    Runtime(RuntimeAdapterError),
    Headless(HeadlessError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => error.fmt(formatter),
            Self::Runtime(error) => error.fmt(formatter),
            Self::Headless(error) => error.fmt(formatter),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Headless(error) => Some(error),
        }
    }
}

impl From<ValidationError> for CliError {
    fn from(value: ValidationError) -> Self {
        Self::Validation(value)
    }
}

impl From<RuntimeAdapterError> for CliError {
    fn from(value: RuntimeAdapterError) -> Self {
        Self::Runtime(value)
    }
}

impl From<HeadlessError> for CliError {
    fn from(value: HeadlessError) -> Self {
        Self::Headless(value)
    }
}
