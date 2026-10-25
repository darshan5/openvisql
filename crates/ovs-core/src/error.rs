use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    /// Could not establish or keep a connection to the server.
    #[error("connection error: {0}")]
    Connection(String),

    /// The server rejected a statement. `code` is the engine's native error number when known.
    #[error("{}{message}", code.map(|c| format!("Error {c}: ")).unwrap_or_default())]
    Server { code: Option<u32>, message: String },

    /// The running statement was cancelled by the user.
    #[error("query cancelled")]
    Cancelled,

    /// The operation is not available for this engine or server version.
    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn server(code: impl Into<Option<u32>>, message: impl Into<String>) -> Self {
        Self::Server {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_error_includes_code_when_present() {
        assert_eq!(
            Error::server(1146, "Table 'x.y' doesn't exist").to_string(),
            "Error 1146: Table 'x.y' doesn't exist"
        );
        assert_eq!(Error::server(None, "boom").to_string(), "boom");
    }
}
