use thiserror::Error;

#[derive(Error, Debug)]
pub enum AgentmailError {
    #[error("IMAP error: {0}")]
    Imap(#[from] async_imap::error::Error),

    #[error("TLS error: {0}")]
    Tls(#[from] native_tls::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Credential error: {0}")]
    Credential(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Account not found: {0}")]
    AccountNotFound(String),

    #[error("Mailbox not found: {0}")]
    MailboxNotFound(String),

    #[error("Message not found: UID {0}")]
    MessageNotFound(u32),

    #[error("invalid search query: {0}")]
    InvalidSearch(String),

    #[error("Not connected")]
    NotConnected,

    #[error("Connection pool exhausted")]
    PoolExhausted,

    #[error("{0}")]
    Other(String),
}

impl AgentmailError {
    /// Whether this error indicates the IMAP **connection** dropped (broken
    /// pipe / reset / EOF / lost), as opposed to a server-level command
    /// rejection (`No`/`Bad`), a parse error, or a config/credential problem.
    /// Used to decide whether retrying the operation once with a fresh
    /// connection could help — see `ConnectionPool::with_session_retry`.
    pub fn is_connection_error(&self) -> bool {
        match self {
            AgentmailError::Imap(e) => matches!(
                e,
                async_imap::error::Error::Io(_) | async_imap::error::Error::ConnectionLost
            ),
            AgentmailError::Io(_) | AgentmailError::Tls(_) | AgentmailError::NotConnected => true,
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, AgentmailError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_connection_vs_other_errors() {
        // Connection-level → retryable.
        assert!(
            AgentmailError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe"))
                .is_connection_error()
        );
        assert!(
            AgentmailError::Imap(async_imap::error::Error::ConnectionLost).is_connection_error()
        );
        assert!(
            AgentmailError::Imap(async_imap::error::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset"
            )))
            .is_connection_error()
        );
        assert!(AgentmailError::NotConnected.is_connection_error());

        // Server rejection / client errors → NOT retryable.
        assert!(
            !AgentmailError::Imap(async_imap::error::Error::No("denied".into()))
                .is_connection_error()
        );
        assert!(
            !AgentmailError::Imap(async_imap::error::Error::Bad("syntax".into()))
                .is_connection_error()
        );
        assert!(!AgentmailError::Parse("bad".into()).is_connection_error());
        assert!(!AgentmailError::AccountNotFound("a".into()).is_connection_error());
        assert!(!AgentmailError::Credential("nope".into()).is_connection_error());
    }
}
