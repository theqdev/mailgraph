use crate::normalize::normalize_email;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderKind {
    Person,
    NoReply,
    Bulk,
    Newsletter,
    Promotional,
    Automated,
    Invalid,
}

impl SenderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SenderKind::Person => "person",
            SenderKind::NoReply => "no_reply",
            SenderKind::Bulk => "bulk",
            SenderKind::Newsletter => "newsletter",
            SenderKind::Promotional => "promotional",
            SenderKind::Automated => "automated",
            SenderKind::Invalid => "invalid",
        }
    }
}

pub fn classify_sender(email: &str, headers: Option<&str>) -> SenderKind {
    let Some(normalized) = normalize_email(email) else {
        return SenderKind::Invalid;
    };

    let local = normalized
        .address
        .split_once('@')
        .map(|(local, _)| local)
        .unwrap_or_default();

    if normalized.domain.starts_with("noreply.")
        || normalized.domain.starts_with("no-reply.")
        || normalized.domain.contains(".noreply.")
        || normalized.domain.contains(".no-reply.")
    {
        return SenderKind::NoReply;
    }

    if matches!(
        local,
        "no-reply" | "noreply" | "do-not-reply" | "donotreply" | "no.reply"
    ) {
        return SenderKind::NoReply;
    }

    if local.contains("noreply")
        || local.contains("no-reply")
        || local.contains("no_reply")
        || local.contains("do-not-reply")
        || local.contains("do_not_reply")
        || local.contains("donotreply")
    {
        return SenderKind::NoReply;
    }

    if matches!(local, "newsletter" | "digest" | "updates") {
        return SenderKind::Newsletter;
    }

    if matches!(
        local,
        "promo" | "promotions" | "marketing" | "offers" | "deals"
    ) {
        return SenderKind::Promotional;
    }

    if matches!(
        local,
        "notifications" | "notification" | "alerts" | "mailer" | "bot"
    ) {
        return SenderKind::Automated;
    }

    if let Some(headers) = headers {
        let lower = headers.to_ascii_lowercase();
        if lower.contains("precedence: bulk")
            || lower.contains("list-unsubscribe:")
            || lower.contains("x-mailer:")
        {
            return SenderKind::Bulk;
        }
    }

    SenderKind::Person
}

pub fn is_filtered_sender_kind(kind: &str) -> bool {
    matches!(
        kind,
        "no_reply" | "bulk" | "newsletter" | "promotional" | "automated" | "invalid"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_machine_senders() {
        assert_eq!(
            classify_sender("noreply@example.com", None),
            SenderKind::NoReply
        );
        assert_eq!(
            classify_sender("project@noreply.github.com", None),
            SenderKind::NoReply
        );
        assert_eq!(
            classify_sender("digest-noreply@quora.com", None),
            SenderKind::NoReply
        );
        assert_eq!(
            classify_sender("do_not_reply@example.com", None),
            SenderKind::NoReply
        );
        assert_eq!(
            classify_sender("noreply=example.com@mail.example.com", None),
            SenderKind::NoReply
        );
        assert_eq!(
            classify_sender("newsletter@example.com", None),
            SenderKind::Newsletter
        );
        assert_eq!(
            classify_sender("alice@example.com", Some("List-Unsubscribe: <mailto:x>")),
            SenderKind::Bulk
        );
    }
}
