use thiserror::Error;

#[derive(Error, Debug)]
pub enum MailkitError {
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

    #[error("Not connected")]
    NotConnected,

    #[error("Connection pool exhausted")]
    PoolExhausted,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, MailkitError>;
