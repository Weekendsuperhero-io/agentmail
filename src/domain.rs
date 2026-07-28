//! Canonical sender-domain parsing and Public Suffix List metadata.
//!
//! Exact canonical domains are the stable identity used by ranking and
//! mutation tools. Registrable-domain and subdomain fields are presentation
//! metadata derived from the current PSL so a list update never requires a
//! cache rewrite.

use std::net::IpAddr;

use idna::uts46::AsciiDenyList;

/// An exact canonical domain plus optional PSL-derived hierarchy metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DomainIdentity {
    pub(crate) domain: String,
    pub(crate) registrable_domain: Option<String>,
    pub(crate) subdomain: Option<String>,
}

/// Convert a user- or header-supplied domain to canonical ASCII DNS form.
///
/// UTS #46 handles Unicode equivalence and IDNs. The explicit DNS checks are
/// intentionally stricter than URL host parsing: sender organization must not
/// silently accept address literals, empty labels, or non-hostname labels.
pub(crate) fn canonicalize_domain(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() || input.starts_with('[') || input.ends_with(']') {
        return None;
    }

    let ascii = idna::domain_to_ascii_cow(input.as_bytes(), AsciiDenyList::URL).ok()?;
    // A single trailing root dot is equivalent DNS spelling. More than one
    // creates an empty label and is therefore malformed.
    let domain = ascii.strip_suffix('.').unwrap_or(ascii.as_ref());
    if domain.is_empty() || domain.ends_with('.') || domain.len() > 253 {
        return None;
    }
    if domain.parse::<IpAddr>().is_ok() {
        return None;
    }
    if domain.split('.').any(|label| {
        label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return None;
    }

    Some(domain.to_string())
}

/// Extract and canonicalize the domain portion of an email address.
pub(crate) fn domain_from_address(address: &str) -> Option<String> {
    let address = address.trim();
    let (local, domain) = address.rsplit_once('@')?;
    if local.is_empty() || domain.is_empty() {
        return None;
    }
    canonicalize_domain(domain)
}

/// Canonicalize a domain and attach hierarchy metadata from the current PSL.
pub(crate) fn domain_identity(input: &str) -> Option<DomainIdentity> {
    let domain = canonicalize_domain(input)?;
    let registrable_domain = psl::suffix(domain.as_bytes())
        .filter(|suffix| suffix.is_known())
        .and_then(|_| psl::domain_str(&domain))
        .map(str::to_owned);
    let subdomain = registrable_domain.as_deref().and_then(|registrable| {
        domain
            .strip_suffix(registrable)
            .and_then(|prefix| prefix.strip_suffix('.'))
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_owned)
    });

    Some(DomainIdentity {
        domain,
        registrable_domain,
        subdomain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_case_root_dot_and_unicode() {
        assert_eq!(
            canonicalize_domain("  MAIL.Example.COM.  "),
            Some("mail.example.com".to_string())
        );
        assert_eq!(
            canonicalize_domain("BÜCHER.DE"),
            Some("xn--bcher-kva.de".to_string())
        );
    }

    #[test]
    fn rejects_address_literals_and_malformed_dns_names() {
        for invalid in [
            "",
            ".",
            "example.com..",
            "two..dots.example",
            "-bad.example",
            "bad-.example",
            "not a domain.example",
            "example.com/path",
            "[127.0.0.1]",
            "[IPv6:2001:db8::1]",
            "127.0.0.1",
        ] {
            assert_eq!(canonicalize_domain(invalid), None, "accepted {invalid:?}");
        }

        let oversized_label = format!("{}.example", "a".repeat(64));
        assert_eq!(canonicalize_domain(&oversized_label), None);
        let oversized_domain = std::iter::repeat_n("a".repeat(63), 4)
            .collect::<Vec<_>>()
            .join(".");
        assert_eq!(canonicalize_domain(&oversized_domain), None);
    }

    #[test]
    fn extracts_domain_after_the_last_address_separator() {
        assert_eq!(
            domain_from_address("\"foo@bar\"@MAIL.Example.COM"),
            Some("mail.example.com".to_string())
        );
        for invalid in ["", "missing-at.example", "@example.com", "sender@"] {
            assert_eq!(domain_from_address(invalid), None, "accepted {invalid:?}");
        }
    }

    #[test]
    fn derives_registrable_domain_and_full_subdomain_from_psl() {
        assert_eq!(
            domain_identity("News.EU.Example.CO.UK"),
            Some(DomainIdentity {
                domain: "news.eu.example.co.uk".to_string(),
                registrable_domain: Some("example.co.uk".to_string()),
                subdomain: Some("news.eu".to_string()),
            })
        );
        assert_eq!(
            domain_identity("example.com"),
            Some(DomainIdentity {
                domain: "example.com".to_string(),
                registrable_domain: Some("example.com".to_string()),
                subdomain: None,
            })
        );
    }

    #[test]
    fn honors_private_suffixes_and_keeps_unknown_internal_domains() {
        assert_eq!(
            domain_identity("mail.tenant.github.io"),
            Some(DomainIdentity {
                domain: "mail.tenant.github.io".to_string(),
                registrable_domain: Some("tenant.github.io".to_string()),
                subdomain: Some("mail".to_string()),
            })
        );
        assert_eq!(
            domain_identity("intranet"),
            Some(DomainIdentity {
                domain: "intranet".to_string(),
                registrable_domain: None,
                subdomain: None,
            })
        );
    }
}
