#![deny(unsafe_code)]
#![deny(clippy::all)]

pub mod capture;
pub mod config;
pub mod delivery;
pub mod enrollment;
pub mod hw_traits;
pub mod identity;
pub mod observability;
pub mod perchpub;
pub mod queue;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("enrollment error: {0}")]
    Enrollment(String),
    #[error("perchpub HTTP error: {0}")]
    Perchpub(String),
    #[error("queue error: {0}")]
    Queue(String),
    #[error("delivery error: {0}")]
    Delivery(String),
}

pub type Result<T> = std::result::Result<T, Error>;
