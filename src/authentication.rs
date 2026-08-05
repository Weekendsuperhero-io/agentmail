//! Message-authentication evidence computed from exact RFC822 source bytes.

use std::time::Duration;

use chrono::Utc;
use mail_auth::{AuthenticatedMessage, DkimResult, MessageAuthenticator};

use crate::{CancelFn, DkimVerification, Result};

const DKIM_TIMEOUT: Duration = Duration::from_secs(15);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Verify every parseable DKIM signature and summarize the strongest result.
///
/// A pass wins because it proves at least one signer authenticated the archived
/// bytes. Other multi-signature outcomes retain the verifier's complete result
/// summary in `detail`; no `Authentication-Results` header is trusted here.
pub(crate) async fn verify_dkim(
    raw_message: &[u8],
    cancel: Option<&CancelFn>,
) -> Result<DkimVerification> {
    crate::imap_client::check_cancel(cancel)?;
    let checked_at = Utc::now();
    let Some(message) = AuthenticatedMessage::parse(raw_message) else {
        return Ok(DkimVerification {
            result: "permError".to_string(),
            domain: None,
            detail: Some("the complete RFC822 message could not be parsed".to_string()),
            checked_at,
        });
    };
    if message.dkim_headers.is_empty() {
        return Ok(DkimVerification {
            result: "none".to_string(),
            domain: None,
            detail: Some("no verifiable DKIM-Signature header was present".to_string()),
            checked_at,
        });
    }

    let authenticator = match MessageAuthenticator::new_system_conf() {
        Ok(authenticator) => authenticator,
        Err(error) => {
            return Ok(DkimVerification {
                result: "tempError".to_string(),
                domain: None,
                detail: Some(format!(
                    "could not initialize the system DNS resolver: {error}"
                )),
                checked_at,
            });
        }
    };

    let verification = await_with_cancel(
        tokio::time::timeout(DKIM_TIMEOUT, authenticator.verify_dkim(&message)),
        cancel,
    )
    .await?;
    let outputs = match verification {
        Ok(outputs) => outputs,
        Err(_) => {
            return Ok(DkimVerification {
                result: "tempError".to_string(),
                domain: None,
                detail: Some(format!(
                    "DKIM verification timed out after {} seconds",
                    DKIM_TIMEOUT.as_secs()
                )),
                checked_at,
            });
        }
    };

    if let Some(output) = outputs
        .iter()
        .find(|output| output.result() == &DkimResult::Pass)
    {
        let passing = outputs
            .iter()
            .filter(|output| output.result() == &DkimResult::Pass)
            .count();
        return Ok(DkimVerification {
            result: "pass".to_string(),
            domain: output.signature().map(|signature| signature.d.to_string()),
            detail: (outputs.len() > 1).then(|| {
                format!(
                    "{passing} of {} DKIM signatures passed local verification",
                    outputs.len()
                )
            }),
            checked_at,
        });
    }

    let selected = outputs.first();
    let result = selected.map_or("none", |output| dkim_result_name(output.result()));
    let domain = selected
        .and_then(|output| output.signature())
        .map(|signature| signature.d.to_string());
    let detail = if outputs.is_empty() {
        Some("no verifiable DKIM signature was present".to_string())
    } else {
        Some(
            outputs
                .iter()
                .map(|output| output.result().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    Ok(DkimVerification {
        result: result.to_string(),
        domain,
        detail,
        checked_at,
    })
}

fn dkim_result_name(result: &DkimResult) -> &'static str {
    match result {
        DkimResult::Pass => "pass",
        DkimResult::Neutral(_) => "neutral",
        DkimResult::Fail(_) => "fail",
        DkimResult::PermError(_) => "permError",
        DkimResult::TempError(_) => "tempError",
        DkimResult::None => "none",
    }
}

async fn await_with_cancel<F, T>(future: F, cancel: Option<&CancelFn>) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(future);
    loop {
        crate::imap_client::check_cancel(cancel)?;
        tokio::select! {
            output = &mut future => return Ok(output),
            () = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unsigned_message_reports_none_without_dns() {
        let result = verify_dkim(
            b"From: sender@example.com\r\nSubject: archive me\r\n\r\nbody\r\n",
            None,
        )
        .await
        .expect("verification result");
        assert_eq!(result.result, "none");
        assert!(result.domain.is_none());
    }

    #[tokio::test]
    async fn cancelled_verification_stops_before_dns() {
        let cancel: CancelFn = std::sync::Arc::new(|| true);
        let result = verify_dkim(
            b"DKIM-Signature: v=1; a=rsa-sha256; d=example.com; s=test; h=from; bh=x; b=x\r\nFrom: sender@example.com\r\n\r\nbody",
            Some(&cancel),
        )
        .await;
        assert!(matches!(result, Err(crate::AgentmailError::Cancelled)));
    }
}
