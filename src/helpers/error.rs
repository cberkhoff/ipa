use crate::error::BoxError;
use crate::helpers::Identity;
use crate::protocol::{RecordId, UniqueStepId};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("An error occurred while sending data to {dest:?}")]
    SendError {
        dest: Identity,

        #[source]
        inner: BoxError,
    },
    #[error("An error occurred while receiving data from {source:?}")]
    ReceiveError {
        source: Identity,
        #[source]
        inner: BoxError,
    },
    #[error("An error occurred while serializing or deserializing data for {record_id:?} and step {step}")]
    SerializationError {
        record_id: RecordId,
        step: String,
        #[source]
        inner: serde_json::Error,
    },
}

impl Error {
    pub fn send_error<E: std::error::Error + Send + Sync + 'static>(
        dest: Identity,
        inner: E,
    ) -> Error {
        Self::SendError {
            dest,
            inner: Box::new(inner),
        }
    }

    pub fn receive_error<E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>>(
        source: Identity,
        inner: E,
    ) -> Error {
        Self::ReceiveError {
            source,
            inner: inner.into(),
        }
    }

    #[must_use]
    pub fn serialization_error(
        record_id: RecordId,
        step: &UniqueStepId,
        inner: serde_json::Error,
    ) -> Error {
        Self::SerializationError {
            record_id,
            step: String::from(step.as_ref()),
            inner,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;