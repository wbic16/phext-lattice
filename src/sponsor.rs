//! Donation link for Phext Edit.
//!
//! The site does not carry ads. The one offer is a Stripe Payment Link
//! (hosted Checkout) opened from "Support the site". The URL is
//! `PHEXT_SPONSOR_URL`. An empty or unusable value hides the control so a
//! machine without Stripe does not show a dead link. This module does not
//! call Stripe and does not add a dependency.

/// Environment variable deploy.sh writes into the phext-edit service environment.
pub const ENV_NAME: &str = "PHEXT_SPONSOR_URL";

/// Placeholder in the public HTML. Replaced when a page is served.
pub const SLOT: &str = "<!--phext-sponsor-->";

const TITLE: &str = "Donate to support the site. This is a donation, not an advertisement, and not an Exocortex Founding purchase.";

/// A configured donation URL, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsorOffer {
    url: Option<String>,
}

impl SponsorOffer {
    pub fn hidden() -> Self {
        Self { url: None }
    }

    /// Read [`ENV_NAME`]. A missing variable and a blank value both hide the link.
    /// A non-empty value that is not a usable https URL also hides it, and warns
    /// once per call so a bad deploy is visible without breaking startup.
    pub fn from_env() -> Self {
        match std::env::var(ENV_NAME) {
            Ok(raw) => {
                let offer = Self::parse(&raw);
                if offer.url.is_none() && !raw.trim().is_empty() {
                    eprintln!(
                        "sponsor: {ENV_NAME} is set but is not a usable https URL; hiding the donation link"
                    );
                }
                offer
            }
            Err(_) => Self::hidden(),
        }
    }

    /// Accept a Stripe Payment Link (or any https URL safe to place in an href).
    /// Rejects anything that could break out of the attribute or send the visitor
    /// somewhere other than an https page.
    pub fn parse(raw: &str) -> Self {
        match accept_https_url(raw.trim()) {
            Some(url) => Self { url: Some(url) },
            None => Self::hidden(),
        }
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// The visible control, or an empty string when the link is hidden.
    pub fn markup(&self) -> String {
        let Some(url) = self.url() else {
            return String::new();
        };
        format!(
            "<span class=\"sponsor-offer\"> · <a class=\"sponsor-link\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\" title=\"{}\">Support the site</a></span>",
            escape_attr(url),
            escape_attr(TITLE),
        )
    }
}

/// Replace every [`SLOT`] in `html`. Pages without the slot are unchanged
/// apart from the usual allocate-on-replace cost when the caller already
/// checked; this function always returns an owned string.
pub fn apply_slot(html: &str, offer: &SponsorOffer) -> String {
    html.replace(SLOT, &offer.markup())
}

fn accept_https_url(url: &str) -> Option<String> {
    if url.is_empty() || url.len() > 2048 || !url.is_ascii() {
        return None;
    }
    let rest = url.strip_prefix("https://")?;
    if rest.is_empty() || rest.starts_with('/') {
        return None;
    }
    // Quotes, angle brackets, and backslashes would break the attribute or
    // the env file. '@' is userinfo. Whitespace and controls never belong.
    if url.chars().any(|c| {
        c.is_ascii_whitespace()
            || c.is_control()
            || matches!(c, '<' | '>' | '"' | '\'' | '`' | '\\' | '@' | '{' | '}')
    }) {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if !valid_host(host) {
        return None;
    }
    Some(url.to_string())
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.contains("..") || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    if name.is_empty() || !name.contains('.') || name.starts_with('-') || name.ends_with('-') {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return false;
    }
    if name.split('.').any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
    {
        return false;
    }
    if let Some(port) = port {
        if port.is_empty() || port.len() > 5 || !port.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sponsor_url_comes_from_config_and_hides_when_empty() {
        assert_eq!(ENV_NAME, "PHEXT_SPONSOR_URL");
        assert!(SponsorOffer::parse("").url().is_none());
        assert!(SponsorOffer::parse("   ").url().is_none());
        assert!(SponsorOffer::parse("").markup().is_empty());

        let offer = SponsorOffer::parse("https://buy.stripe.com/test_sponsor");
        assert_eq!(offer.url(), Some("https://buy.stripe.com/test_sponsor"));
        let html = offer.markup();
        assert!(html.contains("href=\"https://buy.stripe.com/test_sponsor\""));
        assert!(html.contains(">Support the site</a>"));
        assert!(html.contains("Donate to support the site"));
        assert!(html.contains("not an advertisement"));
        assert!(html.contains("Exocortex Founding"));
        assert!(html.contains("target=\"_blank\""));
        assert!(html.contains("rel=\"noopener noreferrer\""));
        assert!(!html.contains("149"));
        assert!(!html.to_ascii_lowercase().contains("payment_method_types"));
    }

    #[test]
    fn query_ampersands_are_escaped_in_the_attribute() {
        let offer = SponsorOffer::parse("https://buy.stripe.com/abc?foo=1&bar=2");
        let html = offer.markup();
        assert!(html.contains("href=\"https://buy.stripe.com/abc?foo=1&amp;bar=2\""));
        assert!(!html.contains("foo=1&bar=2"));
    }

    #[test]
    fn unsafe_or_non_https_urls_are_hidden() {
        let rejected = [
            "http://buy.stripe.com/x",
            "javascript:alert(1)",
            "https://evil.test/\"onclick=1",
            "https://user:pass@buy.stripe.com/x",
            "https://buy.stripe.com/a b",
            "https://buy.stripe.com/<script>",
            "https:///missing-host",
            "https://localhost/path",
            "not a url",
            "",
        ];
        for bad in rejected {
            assert!(
                SponsorOffer::parse(bad).markup().is_empty(),
                "should hide {bad}"
            );
        }
    }

    #[test]
    fn markup_is_a_donation_link_and_not_ad_markup() {
        let html = SponsorOffer::parse("https://buy.stripe.com/abc").markup();
        let lower = html.to_ascii_lowercase();
        for needle in [
            "adsbygoogle",
            "carbonads",
            "ethicalads",
            "ethical-ads",
            "doubleclick",
            "googlesyndication",
            "googletag",
            "ad-slot",
            "data-ad-",
            "buysellads",
            "sponsored placement",
            "tracking pixel",
        ] {
            assert!(!lower.contains(needle), "{needle} in {html}");
        }
    }

    #[test]
    fn slot_replacement_uses_the_configured_url() {
        let page = "<p>No ads, ever.<!--phext-sponsor--></p>";
        let hidden = apply_slot(page, &SponsorOffer::parse(""));
        assert_eq!(hidden, "<p>No ads, ever.</p>");
        assert!(!hidden.contains("sponsor-link"));
        assert!(!hidden.contains("buy.stripe.com"));

        let shown = apply_slot(page, &SponsorOffer::parse("https://buy.stripe.com/test_abc"));
        assert!(shown.contains("href=\"https://buy.stripe.com/test_abc\""));
        assert!(shown.contains(">Support the site</a>"));
        assert!(shown.contains("No ads, ever."));
        assert!(!shown.contains(SLOT));
        assert!(!shown.to_ascii_lowercase().contains("adsbygoogle"));
    }

    #[test]
    fn from_env_matches_parse_of_the_same_variable() {
        match std::env::var(ENV_NAME) {
            Err(_) => assert!(SponsorOffer::from_env().url().is_none()),
            Ok(raw) if raw.trim().is_empty() => assert!(SponsorOffer::from_env().url().is_none()),
            Ok(raw) => assert_eq!(SponsorOffer::from_env().url(), SponsorOffer::parse(&raw).url()),
        }
    }
}
