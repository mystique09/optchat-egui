pub mod attachments;
pub mod credentials;
pub mod integrations;
pub mod local_tools;
pub mod memory;
pub mod model;
pub mod oauth;
pub mod runtime;
pub mod skills;

pub const NODE: usize = 512;
pub const VIEW: usize = 128_000;
pub const JOBS: usize = 8;
pub const TRIES: usize = 5;
pub const CAP: usize = 30_000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, Error>;
