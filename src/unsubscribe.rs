//! RFC 2369 / RFC 8058 parsing, authentication, and outbound transport.
//!
//! The side-effecting path is deliberately fail-closed: one exact copy of
//! each required header, a passing DKIM signature that covers both headers,
//! one parsed HTTPS URI, a public and DNS-pinned destination, no proxy, no
//! redirects, no retries, and a direct 2xx response.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use mail_auth::{AuthenticatedMessage, DkimResult, MessageAuthenticator};
use reqwest::{Client, Url};

use crate::{AgentmailError, CancelFn, Result, UnsubscribeResult};

const ONE_CLICK_POST_VALUE: &str = "List-Unsubscribe=One-Click";
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
const DKIM_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Default)]
pub(crate) struct ListHeaders {
    pub list_unsubscribe: Option<String>,
    pub list_unsubscribe_post: Option<String>,
    pub list_id: Option<String>,
    list_unsubscribe_count: usize,
    list_unsubscribe_post_count: usize,
    list_id_count: usize,
}

impl ListHeaders {
    fn require_single_one_click_headers(&self) -> std::result::Result<(), String> {
        if self.list_unsubscribe_count != 1 {
            return Err(format!(
                "RFC 8058 requires exactly one List-Unsubscribe header; found {}.",
                self.list_unsubscribe_count
            ));
        }
        if self.list_unsubscribe_post_count != 1 {
            return Err(format!(
                "RFC 8058 requires exactly one List-Unsubscribe-Post header; found {}.",
                self.list_unsubscribe_post_count
            ));
        }
        if !self
            .list_unsubscribe_post
            .as_deref()
            .is_some_and(is_one_click_post_value)
        {
            return Err(
                "List-Unsubscribe-Post is not the exact RFC 8058 One-Click value.".to_string(),
            );
        }
        Ok(())
    }

    pub(crate) fn has_single_list_id(&self) -> bool {
        self.list_id_count == 1
    }
}

#[derive(Debug)]
pub(crate) struct OneClickAttempt {
    pub result: UnsubscribeResult,
    pub dkim_domain: Option<String>,
    pub list_id_authenticated: bool,
}

#[derive(Debug)]
struct VerifiedDkim {
    domain: String,
    list_id_authenticated: bool,
}

#[derive(Debug)]
struct PinnedEndpoint {
    url: Url,
    host: String,
    addresses: Vec<SocketAddr>,
}

/// Extract the three list fields from the RFC 5322 header block, unfolding
/// continuation lines. Counts are retained so action-time parsing can reject
/// ambiguous duplicate headers rather than trusting the first one.
pub(crate) fn parse_list_headers(raw_message: &[u8]) -> ListHeaders {
    let header_text = String::from_utf8_lossy(rfc5322_header_block(raw_message));
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;

    for raw_line in header_text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            if let Some(field) = current.take() {
                fields.push(field);
            }
            break;
        }

        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = current.as_mut() {
                let continuation = line.trim();
                if !continuation.is_empty() {
                    if !value.is_empty() {
                        value.push(' ');
                    }
                    value.push_str(continuation);
                }
            }
            continue;
        }

        if let Some(field) = current.take() {
            fields.push(field);
        }
        if let Some((name, value)) = line.split_once(':') {
            current = Some((name.to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    if let Some(field) = current {
        fields.push(field);
    }

    let mut result = ListHeaders::default();
    for (name, value) in fields {
        match name.as_str() {
            "list-unsubscribe" => {
                result.list_unsubscribe_count += 1;
                result.list_unsubscribe.get_or_insert(value);
            }
            "list-unsubscribe-post" => {
                result.list_unsubscribe_post_count += 1;
                result.list_unsubscribe_post.get_or_insert(value);
            }
            "list-id" => {
                result.list_id_count += 1;
                result.list_id.get_or_insert(value);
            }
            _ => {}
        }
    }
    result
}

fn rfc5322_header_block(raw_message: &[u8]) -> &[u8] {
    let crlf_end = raw_message
        .windows(4)
        .position(|window| window == b"\r\n\r\n");
    let lf_end = raw_message.windows(2).position(|window| window == b"\n\n");
    let end = match (crlf_end, lf_end) {
        (Some(crlf), Some(lf)) => crlf.min(lf),
        (Some(end), None) | (None, Some(end)) => end,
        (None, None) => raw_message.len(),
    };
    &raw_message[..end]
}

/// RFC 8058's `postarg` is exactly one known key/value pair. ABNF string
/// literals are case-insensitive, and the comparison ignores ALL interior
/// whitespace, not just the edges: header folding is legal FWS anywhere in a
/// structured field (unfolding inserts a space, e.g. `List-Unsubscribe=\r\n
/// One-Click`), and some ESPs pad around the `=`. Token content must still
/// match exactly, so trailing parameters or extra pairs are rejected.
pub(crate) fn is_one_click_post_value(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    compact.eq_ignore_ascii_case(ONE_CLICK_POST_VALUE)
}

/// Parse a displayable HTTPS URI for discovery. This performs all local RFC
/// syntax checks, but intentionally does not resolve DNS or claim DKIM has
/// passed; execution repeats parsing against a live-fetched full message.
pub(crate) fn advertised_https_url(header: &str) -> Option<String> {
    parse_one_click_url(header).ok().map(|url| url.to_string())
}

pub(crate) fn advertises_one_click(
    list_unsubscribe: Option<&str>,
    list_unsubscribe_post: Option<&str>,
) -> bool {
    list_unsubscribe.and_then(advertised_https_url).is_some()
        && list_unsubscribe_post.is_some_and(is_one_click_post_value)
}

/// Execute the authenticated RFC 8058 request. Policy decisions about whether
/// a failed unsubscribe may be followed by message deletion stay with the
/// caller; this function performs no IMAP mutation.
pub(crate) async fn attempt_one_click(
    raw_message: &[u8],
    headers: &ListHeaders,
    cancel: Option<&CancelFn>,
) -> Result<OneClickAttempt> {
    let fail =
        |reason: String, url: Option<String>, status: Option<u16>, dkim: Option<VerifiedDkim>| {
            let (dkim_domain, list_id_authenticated) = dkim.map_or((None, false), |verified| {
                (Some(verified.domain), verified.list_id_authenticated)
            });
            OneClickAttempt {
                result: UnsubscribeResult {
                    success: false,
                    method: None,
                    url,
                    http_status: status,
                    reason: Some(reason),
                },
                dkim_domain,
                list_id_authenticated,
            }
        };

    if let Err(reason) = headers.require_single_one_click_headers() {
        return Ok(fail(reason, None, None, None));
    }
    let Some(list_unsubscribe) = headers.list_unsubscribe.as_deref() else {
        return Ok(fail(
            "List-Unsubscribe header value was unavailable.".to_string(),
            None,
            None,
            None,
        ));
    };
    let url = match parse_one_click_url(list_unsubscribe) {
        Ok(url) => url,
        Err(reason) => return Ok(fail(reason, None, None, None)),
    };

    let mut dkim = match verify_rfc8058_dkim(raw_message, cancel).await? {
        Ok(identity) => identity,
        Err(reason) => return Ok(fail(reason, Some(url.to_string()), None, None)),
    };
    // DKIM's bottom-up duplicate-header selection cannot authenticate one
    // unambiguous cleanup identity when more than one List-Id is present.
    dkim.list_id_authenticated &= headers.has_single_list_id();

    let endpoint = match resolve_public_endpoint(url, cancel).await? {
        Ok(endpoint) => endpoint,
        Err(reason) => {
            return Ok(fail(reason, None, None, Some(dkim)));
        }
    };
    crate::imap_client::check_cancel(cancel)?;

    let client = match hardened_client(&endpoint) {
        Ok(client) => client,
        Err(error) => {
            return Ok(fail(
                format!("Failed to create hardened HTTP client: {error}"),
                Some(endpoint.url.to_string()),
                None,
                Some(dkim),
            ));
        }
    };

    let mut result = send_one_click_request(&client, &endpoint.url, cancel).await?;
    let VerifiedDkim {
        domain,
        list_id_authenticated,
    } = dkim;
    if result.success {
        result.method = Some("rfc8058-one-click".to_string());
    }
    Ok(OneClickAttempt {
        result,
        dkim_domain: Some(domain),
        list_id_authenticated,
    })
}

async fn verify_rfc8058_dkim(
    raw_message: &[u8],
    cancel: Option<&CancelFn>,
) -> Result<std::result::Result<VerifiedDkim, String>> {
    let Some(message) = AuthenticatedMessage::parse(raw_message) else {
        return Ok(Err(
            "The complete message could not be parsed for DKIM verification.".to_string(),
        ));
    };
    if message.dkim_headers.is_empty() {
        return Ok(Err(
            "No verifiable DKIM signature was present; RFC 8058 one-click was not attempted."
                .to_string(),
        ));
    }
    let authenticator = match MessageAuthenticator::new_system_conf() {
        Ok(authenticator) => authenticator,
        Err(error) => {
            return Ok(Err(format!(
                "Could not initialize the system DNS resolver for DKIM: {error}"
            )));
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
            return Ok(Err(format!(
                "DKIM verification timed out after {} seconds.",
                DKIM_TIMEOUT.as_secs()
            )));
        }
    };

    let passing_signature_without_coverage = outputs
        .iter()
        .any(|output| output.result() == &DkimResult::Pass && output.signature().is_some());
    if let Some(selected) = select_rfc8058_signature(outputs.iter().filter_map(|output| {
        if output.result() != &DkimResult::Pass {
            return None;
        }
        output
            .signature()
            .map(|signature| (signature.d.as_str(), signature.h.as_slice()))
    })) {
        return Ok(Ok(selected));
    }

    if passing_signature_without_coverage {
        Ok(Err(
            "A DKIM signature passed, but it did not cover both List-Unsubscribe and List-Unsubscribe-Post."
                .to_string(),
        ))
    } else if outputs.is_empty() {
        Ok(Err(
            "No verifiable DKIM signature was present; RFC 8058 one-click was not attempted."
                .to_string(),
        ))
    } else {
        let summary = outputs
            .iter()
            .map(|output| output.result().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Err(format!(
            "No DKIM signature passed verification ({summary}); RFC 8058 one-click was not attempted."
        )))
    }
}

fn select_rfc8058_signature<'a>(
    signatures: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> Option<VerifiedDkim> {
    let mut two_header_fallback = None;
    for (domain, headers) in signatures {
        if !signature_covers_rfc8058_headers(headers) {
            continue;
        }
        let candidate = VerifiedDkim {
            domain: domain.to_string(),
            list_id_authenticated: signature_covers_list_id(headers),
        };
        if candidate.list_id_authenticated {
            return Some(candidate);
        }
        two_header_fallback.get_or_insert(candidate);
    }
    two_header_fallback
}

fn signature_covers_rfc8058_headers(headers: &[String]) -> bool {
    let covers_unsubscribe = headers
        .iter()
        .any(|name| name.eq_ignore_ascii_case("List-Unsubscribe"));
    let covers_post = headers
        .iter()
        .any(|name| name.eq_ignore_ascii_case("List-Unsubscribe-Post"));
    covers_unsubscribe && covers_post
}

fn signature_covers_list_id(headers: &[String]) -> bool {
    headers
        .iter()
        .any(|name| name.eq_ignore_ascii_case("List-Id"))
}

fn parse_one_click_url(header: &str) -> std::result::Result<Url, String> {
    let uris = parse_rfc2369_uris(header)?;
    let mut https_url = None;

    for uri in uris {
        let parsed = Url::parse(&uri)
            .map_err(|_| "List-Unsubscribe contains a malformed URI alternative.".to_string())?;
        match parsed.scheme() {
            "https" => {
                validate_static_url(&parsed)?;
                if https_url.replace(parsed).is_some() {
                    return Err(
                        "RFC 8058 requires exactly one HTTPS URI in List-Unsubscribe.".to_string(),
                    );
                }
            }
            "http" => {
                return Err(
                    "List-Unsubscribe contains an HTTP URI; RFC 8058 permits one HTTPS URI and only non-HTTP/S alternatives."
                        .to_string(),
                );
            }
            _ => {}
        }
    }

    https_url
        .ok_or_else(|| "No valid HTTPS URI was found in the List-Unsubscribe header.".to_string())
}

/// RFC 2369 URI-list parser. Commas inside angle brackets belong to the URI;
/// comments and folding whitespace outside brackets are skipped, and
/// whitespace inserted inside brackets by a broken MTA is ignored as the RFC
/// requires receivers to do.
fn parse_rfc2369_uris(header: &str) -> std::result::Result<Vec<String>, String> {
    let bytes = header.as_bytes();
    let mut index = 0usize;
    skip_cfws(bytes, &mut index)?;
    if bytes.get(index) != Some(&b'<') {
        return Err("List-Unsubscribe does not begin with an angle-bracket URI.".to_string());
    }

    let mut uris = Vec::new();
    loop {
        if bytes.get(index) != Some(&b'<') {
            return Err("List-Unsubscribe contains a malformed URI alternative.".to_string());
        }
        index += 1;
        let mut uri = Vec::new();
        let mut closed = false;
        while let Some(&byte) = bytes.get(index) {
            index += 1;
            match byte {
                b'>' => {
                    closed = true;
                    break;
                }
                b'<' | 0..=8 | 11..=12 | 14..=31 | 127 => {
                    return Err("List-Unsubscribe contains an invalid URI character.".to_string());
                }
                b' ' | b'\t' | b'\r' | b'\n' => {}
                _ => uri.push(byte),
            }
        }
        if !closed {
            return Err("List-Unsubscribe contains an unterminated URI.".to_string());
        }
        if uri.is_empty() {
            return Err("List-Unsubscribe contains an empty URI.".to_string());
        }
        let uri = String::from_utf8(uri)
            .map_err(|_| "List-Unsubscribe URI is not valid UTF-8.".to_string())?;
        uris.push(uri);

        skip_cfws(bytes, &mut index)?;
        if index >= bytes.len() {
            return Ok(uris);
        }
        if bytes[index] != b',' {
            return Err(
                "List-Unsubscribe contains unexplained trailing text after a URI alternative."
                    .to_string(),
            );
        }
        index += 1;
        skip_cfws(bytes, &mut index)?;
        if index >= bytes.len() {
            return Err("List-Unsubscribe contains a dangling comma.".to_string());
        }
        if bytes.get(index) != Some(&b'<') {
            return Err("List-Unsubscribe contains a malformed URI alternative.".to_string());
        }
    }
}

fn skip_cfws(bytes: &[u8], index: &mut usize) -> std::result::Result<(), String> {
    loop {
        while bytes
            .get(*index)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            *index += 1;
        }
        if bytes.get(*index) != Some(&b'(') {
            return Ok(());
        }

        *index += 1;
        let mut depth = 1usize;
        while let Some(&byte) = bytes.get(*index) {
            *index += 1;
            match byte {
                b'\\' => {
                    if *index >= bytes.len() {
                        return Err("List-Unsubscribe contains an invalid comment escape.".into());
                    }
                    *index += 1;
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err("List-Unsubscribe contains an unterminated comment.".to_string());
        }
    }
}

fn validate_static_url(url: &Url) -> std::result::Result<(), String> {
    if url.scheme() != "https" {
        return Err("RFC 8058 one-click requires an HTTPS URI.".to_string());
    }
    if url.host_str().is_none() {
        return Err("The HTTPS unsubscribe URI has no host.".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The HTTPS unsubscribe URI must not contain credentials.".to_string());
    }
    if url.fragment().is_some() {
        return Err(
            "The HTTPS unsubscribe URI must not contain a fragment, which is never sent to the server."
                .to_string(),
        );
    }
    if url.port_or_known_default().is_none_or(|port| port == 0) {
        return Err("The HTTPS unsubscribe URI has no usable port.".to_string());
    }
    Ok(())
}

async fn resolve_public_endpoint(
    url: Url,
    cancel: Option<&CancelFn>,
) -> Result<std::result::Result<PinnedEndpoint, String>> {
    let Some(host) = url.host_str() else {
        return Ok(Err("The HTTPS unsubscribe URI has no host.".to_string()));
    };
    let host = host.trim_matches(['[', ']']).to_string();
    let Some(port) = url.port_or_known_default() else {
        return Ok(Err(
            "The HTTPS unsubscribe URI has no usable port.".to_string()
        ));
    };

    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let lookup = await_with_cancel(
            tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host.as_str(), port))),
            cancel,
        )
        .await?;
        match lookup {
            Ok(Ok(addresses)) => addresses.collect::<Vec<_>>(),
            Ok(Err(error)) => {
                return Ok(Err(format!(
                    "Could not resolve the unsubscribe host safely: {error}"
                )));
            }
            Err(_) => {
                return Ok(Err(format!(
                    "Resolving the unsubscribe host timed out after {} seconds.",
                    DNS_TIMEOUT.as_secs()
                )));
            }
        }
    };

    if addresses.is_empty() {
        return Ok(Err(
            "The unsubscribe host resolved to no destinations.".to_string()
        ));
    }
    if let Some(address) = addresses
        .iter()
        .find(|address| !is_public_destination(address.ip()))
    {
        return Ok(Err(format!(
            "The unsubscribe host resolved to a non-public destination ({}); the request was blocked.",
            address.ip()
        )));
    }

    let mut addresses = addresses;
    addresses.sort_unstable();
    addresses.dedup();
    Ok(Ok(PinnedEndpoint {
        url,
        host,
        addresses,
    }))
}

fn hardened_client(endpoint: &PinnedEndpoint) -> reqwest::Result<Client> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .retry(reqwest::retry::never())
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .resolve_to_addrs(&endpoint.host, &endpoint.addresses)
        .build()
}

async fn send_one_click_request(
    client: &Client,
    url: &Url,
    cancel: Option<&CancelFn>,
) -> Result<UnsubscribeResult> {
    crate::imap_client::check_cancel(cancel)?;
    let request = client
        .post(url.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(ONE_CLICK_POST_VALUE)
        .send();
    let response = await_with_cancel(request, cancel).await?;

    match response {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                Ok(UnsubscribeResult {
                    success: true,
                    method: None,
                    url: Some(url.to_string()),
                    http_status: Some(status.as_u16()),
                    reason: None,
                })
            } else {
                let reason = if status.is_redirection() {
                    format!(
                        "Unsubscribe endpoint returned forbidden HTTP redirect {}; redirects were not followed.",
                        status.as_u16()
                    )
                } else {
                    format!(
                        "Unsubscribe endpoint returned HTTP {}; only a direct 2xx response is success.",
                        status.as_u16()
                    )
                };
                Ok(UnsubscribeResult {
                    success: false,
                    method: None,
                    url: Some(url.to_string()),
                    http_status: Some(status.as_u16()),
                    reason: Some(reason),
                })
            }
        }
        Err(error) => Ok(UnsubscribeResult {
            success: false,
            method: None,
            url: Some(url.to_string()),
            http_status: None,
            reason: Some(format!("One-click HTTP request failed: {error}")),
        }),
    }
}

async fn await_with_cancel<F, T>(future: F, cancel: Option<&CancelFn>) -> Result<T>
where
    F: Future<Output = T>,
{
    crate::imap_client::check_cancel(cancel)?;
    let Some(cancel) = cancel else {
        return Ok(future.await);
    };

    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {
                if cancel() {
                    return Err(AgentmailError::Cancelled);
                }
            }
        }
    }
}

fn is_public_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let value = u32::from(ip);
    ![
        (0x0000_0000, 8),  // current network / unspecified
        (0x0a00_0000, 8),  // private
        (0x6440_0000, 10), // shared address space
        (0x7f00_0000, 8),  // loopback
        (0xa9fe_0000, 16), // link-local
        (0xac10_0000, 12), // private
        (0xc000_0000, 24), // IETF protocol assignments
        (0xc000_0200, 24), // documentation
        (0xc058_6300, 24), // deprecated 6to4 relay anycast
        (0xc0a8_0000, 16), // private
        (0xc612_0000, 15), // benchmark tests
        (0xc633_6400, 24), // documentation
        (0xcb00_7100, 24), // documentation
        (0xe000_0000, 4),  // multicast
        (0xf000_0000, 4),  // reserved / broadcast
    ]
    .iter()
    .any(|&(network, prefix)| ipv4_in_prefix(value, network, prefix))
}

fn ipv4_in_prefix(value: u32, network: u32, prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    value & mask == network & mask
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }

    let value = u128::from(ip);
    // Public unicast is currently allocated from 2000::/3. Reject special
    // sub-ranges that can tunnel, translate, benchmark, or document traffic.
    ipv6_in_prefix(value, 0x2000_u128 << 112, 3)
        && ![
            (0x2001_0000_u128 << 96, 32), // Teredo
            (0x2001_0002_u128 << 96, 48), // benchmark
            (0x2001_000d_u128 << 96, 48), // deprecated ORCHID
            (0x2001_0010_u128 << 96, 28), // ORCHID
            (0x2001_0020_u128 << 96, 28), // ORCHIDv2
            (0x2001_0db8_u128 << 96, 32), // documentation
            (0x2002_u128 << 112, 16),     // 6to4 tunneling
            (0x3fff_u128 << 112, 20),     // documentation
        ]
        .iter()
        .any(|&(network, prefix)| ipv6_in_prefix(value, network, prefix))
}

fn ipv6_in_prefix(value: u128, network: u128, prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    value & mask == network & mask
}

#[cfg(test)]
mod tests {
    use std::borrow::Borrow;
    use std::hash::Hash;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use mail_auth::common::crypto::Ed25519Key;
    use mail_auth::common::headers::HeaderWriter;
    use mail_auth::common::parse::TxtRecordParser;
    use mail_auth::common::verify::DomainKey;
    use mail_auth::dkim::DkimSigner;
    use mail_auth::{Parameters, ResolverCache, Txt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone)]
    struct StaticTxtCache(Txt);

    impl ResolverCache<Box<str>, Txt> for StaticTxtCache {
        fn get<Q>(&self, _name: &Q) -> Option<Txt>
        where
            Box<str>: Borrow<Q>,
            Q: Hash + Eq + ?Sized,
        {
            Some(self.0.clone())
        }

        fn remove<Q>(&self, _name: &Q) -> Option<Txt>
        where
            Box<str>: Borrow<Q>,
            Q: Hash + Eq + ?Sized,
        {
            None
        }

        fn insert(&self, _key: Box<str>, _value: Txt, _valid_until: std::time::Instant) {}
    }

    #[test]
    fn exact_postarg_is_case_insensitive_but_rejects_extras() {
        assert!(is_one_click_post_value(ONE_CLICK_POST_VALUE));
        assert!(is_one_click_post_value("  list-unsubscribe=one-click  "));
        assert!(!is_one_click_post_value(
            "List-Unsubscribe=One-Click&next=https://victim.example"
        ));
        assert!(!is_one_click_post_value("x=1; List-Unsubscribe=One-Click"));
    }

    #[test]
    fn postarg_tolerates_folding_and_equals_padding() {
        // Unfolded fold point after '=': legal FWS in a structured field.
        assert!(is_one_click_post_value("List-Unsubscribe= One-Click"));
        // ESP padding around the '='.
        assert!(is_one_click_post_value("List-Unsubscribe = One-Click"));
        // Whitespace tolerance must not admit different tokens.
        assert!(!is_one_click_post_value("List-Unsubscribe=OneClick"));
        assert!(!is_one_click_post_value("List-Unsubscribe=Two-Click"));
    }

    #[test]
    fn parses_rfc2369_comments_folding_whitespace_and_uri_commas() {
        let header =
            "(preferred) <mailto:list@example.test>, (web)\r\n\t<HTTPS://example.test/unsub?a=1,2>";
        let uris = parse_rfc2369_uris(header).expect("valid RFC 2369 URI list");
        assert_eq!(
            uris,
            [
                "mailto:list@example.test",
                "HTTPS://example.test/unsub?a=1,2"
            ]
        );

        let spaced = parse_rfc2369_uris("<https://exa mple.test/un sub>")
            .expect("internal MTA whitespace is ignored");
        assert_eq!(spaced, ["https://example.test/unsub"]);

        let trailing_comment = parse_rfc2369_uris("<https://example.test/unsub> (preferred)")
            .expect("trailing CFWS is permitted");
        assert_eq!(trailing_comment, ["https://example.test/unsub"]);
    }

    #[test]
    fn rfc2369_parser_rejects_trailing_text_and_malformed_alternatives() {
        for invalid in [
            "<https://example.test/u> trailing",
            "<https://example.test/u>,",
            "<https://example.test/u>, (comment)",
            "<https://example.test/u>, mailto:list@example.test",
            "<https://example.test/u>,, <mailto:list@example.test>",
            "<https://example.test/u> <mailto:list@example.test>",
        ] {
            assert!(
                parse_rfc2369_uris(invalid).is_err(),
                "invalid URI list was accepted: {invalid}"
            );
        }
    }

    #[test]
    fn one_click_url_requires_one_https_and_safe_static_parts() {
        assert!(parse_one_click_url("<https://example.test/u>, <mailto:x@y>").is_ok());
        assert!(parse_one_click_url("<http://example.test/u>").is_err());
        assert!(parse_one_click_url("<https://example.test/a>, <https://example.test/b>").is_err());
        assert!(parse_one_click_url("<https://user:pass@example.test/u>").is_err());
        assert!(parse_one_click_url("<https://example.test/u#token>").is_err());
        assert!(parse_one_click_url("<https://example.test/u>, <definitely-not-a-uri>").is_err());
    }

    #[test]
    fn action_headers_reject_duplicates() {
        let raw = concat!(
            "List-Unsubscribe: <https://example.test/a>\r\n",
            "List-Unsubscribe: <https://example.test/b>\r\n",
            "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
            "\r\nbody"
        );
        let headers = parse_list_headers(raw.as_bytes());
        assert!(headers.require_single_one_click_headers().is_err());
    }

    #[test]
    fn header_parser_excludes_the_binary_message_body() {
        let expected = concat!(
            "List-Unsubscribe: <https://example.test/a>\r\n",
            "List-Unsubscribe-Post: List-Unsubscribe=One-Click"
        )
        .as_bytes();
        let mut raw = expected.to_vec();
        raw.extend_from_slice(b"\r\n\r\n\xff\xfe\nList-Unsubscribe: <https://attacker.test/u>");

        assert_eq!(rfc5322_header_block(&raw), expected);
        let headers = parse_list_headers(&raw);
        assert_eq!(
            headers.list_unsubscribe.as_deref(),
            Some("<https://example.test/a>")
        );
        assert_eq!(headers.list_unsubscribe_count, 1);
    }

    #[test]
    fn dkim_coverage_requires_both_headers() {
        assert!(signature_covers_rfc8058_headers(&[
            "From".into(),
            "list-unsubscribe".into(),
            "LIST-UNSUBSCRIBE-POST".into(),
        ]));
        assert!(!signature_covers_rfc8058_headers(&[
            "From".into(),
            "List-Unsubscribe".into(),
        ]));
    }

    #[test]
    fn dkim_selection_prefers_a_signature_that_also_covers_list_id() {
        let two_headers = [
            "From".to_string(),
            "List-Unsubscribe".to_string(),
            "List-Unsubscribe-Post".to_string(),
        ];
        let all_headers = [
            "From".to_string(),
            "List-Unsubscribe".to_string(),
            "List-Unsubscribe-Post".to_string(),
            "List-Id".to_string(),
        ];

        let selected = select_rfc8058_signature([
            ("two.example", two_headers.as_slice()),
            ("all.example", all_headers.as_slice()),
        ])
        .expect("a qualifying signature should be selected");

        assert_eq!(selected.domain, "all.example");
        assert!(selected.list_id_authenticated);

        let fallback = select_rfc8058_signature([("two.example", two_headers.as_slice())])
            .expect("the two-header signature still authorizes one-click");
        assert_eq!(fallback.domain, "two.example");
        assert!(!fallback.list_id_authenticated);
    }

    #[tokio::test]
    async fn mail_auth_verifies_a_signed_rfc8058_message_and_header_coverage() {
        const ED25519_SEED: [u8; 32] = [
            157, 97, 177, 157, 239, 253, 90, 96, 186, 132, 74, 244, 146, 236, 44, 196, 68, 73, 197,
            105, 123, 50, 105, 25, 112, 59, 172, 3, 28, 174, 127, 96,
        ];
        const ED25519_PUBLIC: [u8; 32] = [
            215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 14, 225, 114,
            243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 26,
        ];
        const PUBLIC_RECORD: &[u8] =
            b"v=DKIM1; k=ed25519; p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
        let raw = concat!(
            "From: sender@example.test\r\n",
            "List-Unsubscribe: <https://unsubscribe.example.test/u>\r\n",
            "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
            "List-Id: Newsletter <news.example.test>\r\n",
            "Subject: test\r\n",
            "\r\n",
            "body\r\n"
        );
        let key = Ed25519Key::from_seed_and_public_key(&ED25519_SEED, &ED25519_PUBLIC).unwrap();
        let signature = DkimSigner::from_key(key)
            .domain("example.test")
            .selector("oneclick")
            .headers([
                "From",
                "List-Unsubscribe",
                "List-Unsubscribe-Post",
                "List-Id",
                "Subject",
            ])
            .sign(raw.as_bytes())
            .unwrap();
        let signed = format!("{}{raw}", signature.to_header());
        let message = AuthenticatedMessage::parse(signed.as_bytes()).unwrap();
        let cache = StaticTxtCache(Txt::DomainKey(Arc::new(
            DomainKey::parse(PUBLIC_RECORD).unwrap(),
        )));
        let authenticator = MessageAuthenticator::new(
            mail_auth::hickory_resolver::config::ResolverConfig::default(),
            mail_auth::hickory_resolver::config::ResolverOpts::default(),
        )
        .unwrap();
        let outputs = authenticator
            .verify_dkim(Parameters::new(&message).with_txt_cache(&cache))
            .await;

        assert!(
            outputs
                .iter()
                .any(|output| output.result() == &DkimResult::Pass)
        );
        let selected = select_rfc8058_signature(outputs.iter().filter_map(|output| {
            (output.result() == &DkimResult::Pass)
                .then(|| output.signature())
                .flatten()
                .map(|signature| (signature.d.as_str(), signature.h.as_slice()))
        }))
        .expect("passing mail-auth output should authorize RFC 8058");
        assert_eq!(selected.domain, "example.test");
        assert!(selected.list_id_authenticated);
    }

    #[tokio::test]
    async fn unsigned_message_cannot_pass_rfc8058_dkim() {
        let raw = concat!(
            "From: sender@example.test\r\n",
            "List-Unsubscribe: <https://example.test/u>\r\n",
            "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
            "\r\nbody\r\n"
        );
        let result = verify_rfc8058_dkim(raw.as_bytes(), None)
            .await
            .unwrap()
            .expect_err("unsigned message must not verify");
        assert!(result.contains("No verifiable DKIM signature"), "{result}");
    }

    #[tokio::test]
    async fn production_endpoint_resolution_rejects_private_ip_literals() {
        let url = parse_one_click_url("<https://127.0.0.1/unsubscribe>").unwrap();
        let error = resolve_public_endpoint(url, None)
            .await
            .unwrap()
            .expect_err("loopback destination must be blocked");
        assert!(error.contains("non-public destination"));
    }

    #[test]
    fn rejects_non_public_destinations() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "ff02::1",
        ] {
            assert!(
                !is_public_destination(ip.parse().expect("test IP")),
                "{ip} must be rejected"
            );
        }
        assert!(is_public_destination("8.8.8.8".parse().unwrap()));
        assert!(is_public_destination(
            "2606:4700:4700::1111".parse().unwrap()
        ));
    }

    async fn spawn_http_server(
        status_line: &'static str,
        extra_headers: &'static str,
        hold_response: Option<Arc<AtomicBool>>,
    ) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers_end = end + 4;
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .or_else(|| line.strip_prefix("Content-Length: "))
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= headers_end + length {
                        break;
                    }
                }
            }

            if let Some(hold) = hold_response {
                while hold.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            let response = format!(
                "HTTP/1.1 {status_line}\r\n{extra_headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = socket.write_all(response.as_bytes()).await;
            String::from_utf8_lossy(&request).into_owned()
        });
        (
            Url::parse(&format!("http://{address}/unsubscribe?opaque=a%2Cb")).unwrap(),
            handle,
        )
    }

    fn test_http_client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn http_integration_sends_exact_post_and_accepts_only_2xx() {
        let (url, request) = spawn_http_server("204 No Content", "", None).await;
        let result = send_one_click_request(&test_http_client(), &url, None)
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.http_status, Some(204));

        let request = request.await.unwrap();
        assert!(request.starts_with("POST /unsubscribe?opaque=a%2Cb HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded")
        );
        assert!(request.ends_with(ONE_CLICK_POST_VALUE));
        for forbidden in ["cookie", "authorization", "referer"] {
            assert!(
                !request.lines().skip(1).any(|line| {
                    line.split_once(':')
                        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(forbidden))
                }),
                "outbound request unexpectedly contained {forbidden}"
            );
        }

        let (url, _) = spawn_http_server(
            "302 Found",
            "Location: http://127.0.0.1:9/must-not-follow\r\n",
            None,
        )
        .await;
        let result = send_one_click_request(&test_http_client(), &url, None)
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.http_status, Some(302));
        assert!(result.reason.unwrap().contains("redirect"));

        let (url, _) = spawn_http_server("400 Bad Request", "", None).await;
        let result = send_one_click_request(&test_http_client(), &url, None)
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.http_status, Some(400));
    }

    #[tokio::test]
    async fn hardened_client_rejects_plain_http() {
        let endpoint = PinnedEndpoint {
            url: Url::parse("http://unsubscribe.example.test/unsubscribe").unwrap(),
            host: "unsubscribe.example.test".to_string(),
            addresses: vec!["127.0.0.1:9".parse().unwrap()],
        };
        let client = hardened_client(&endpoint).expect("hardened client should build");
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            send_one_click_request(&client, &endpoint.url, None),
        )
        .await
        .expect("plain HTTP should fail before a connection attempt")
        .unwrap();

        assert!(!result.success);
        assert!(result.http_status.is_none());
        let reason = result
            .reason
            .expect("transport failure should explain itself");
        assert!(reason.contains("builder error"), "{reason}");
        assert!(reason.contains("http://"), "{reason}");
    }

    #[tokio::test]
    async fn http_integration_honors_cancellation() {
        let hold = Arc::new(AtomicBool::new(true));
        let (url, server) = spawn_http_server("204 No Content", "", Some(Arc::clone(&hold))).await;
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel: CancelFn = {
            let cancelled = Arc::clone(&cancelled);
            Arc::new(move || cancelled.load(Ordering::Acquire))
        };
        let task = tokio::spawn(async move {
            send_one_click_request(&test_http_client(), &url, Some(&cancel)).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancelled.store(true, Ordering::Release);
        let error = task.await.unwrap().expect_err("request should cancel");
        assert!(matches!(error, AgentmailError::Cancelled));
        hold.store(false, Ordering::Release);
        let _ = server.await;
    }
}
