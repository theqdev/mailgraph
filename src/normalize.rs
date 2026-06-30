#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedEmail {
    pub address: String,
    pub domain: String,
}

pub fn normalize_email(input: &str) -> Option<NormalizedEmail> {
    let trimmed = input.trim().trim_matches(['<', '>', '"', '\'']);
    let (local, domain) = trimmed.rsplit_once('@')?;

    let local = local.trim().to_ascii_lowercase();
    let domain = normalize_domain(domain)?;

    if local.is_empty() || local.contains(char::is_whitespace) {
        return None;
    }

    Some(NormalizedEmail {
        address: format!("{local}@{domain}"),
        domain,
    })
}

pub fn normalize_domain(input: &str) -> Option<String> {
    let domain = input.trim().trim_end_matches('.').to_ascii_lowercase();

    if domain.is_empty()
        || domain.contains(char::is_whitespace)
        || !domain.contains('.')
        || domain.starts_with('.')
        || domain.ends_with('.')
    {
        return None;
    }

    Some(domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_email_and_domain() {
        let normalized = normalize_email(" <Alice.Example@Example.COM> ").unwrap();

        assert_eq!(normalized.address, "alice.example@example.com");
        assert_eq!(normalized.domain, "example.com");
    }

    #[test]
    fn rejects_invalid_email() {
        assert!(normalize_email("not-an-email").is_none());
        assert!(normalize_email("bad local@example.com").is_none());
    }
}
