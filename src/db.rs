use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::{RawAddress, RawMessage};
use crate::classify::{classify_sender, is_filtered_sender_kind};
use crate::normalize::normalize_email;

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn begin_scan(&self, archive_path: &Path) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO scans (archive_path, status, started_at) VALUES (?1, 'running', ?2)",
            params![archive_path.display().to_string(), Utc::now().to_rfc3339()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_scan(&self, scan_id: i64, status: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE scans SET status = ?1, finished_at = ?2, error = ?3 WHERE id = ?4",
            params![status, Utc::now().to_rfc3339(), error, scan_id],
        )?;
        Ok(())
    }

    pub fn update_scan_progress(
        &self,
        scan_id: i64,
        messages_seen: i64,
        messages_indexed: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE scans SET messages_seen = ?1, messages_indexed = ?2 WHERE id = ?3",
            params![messages_seen, messages_indexed, scan_id],
        )?;
        Ok(())
    }

    pub fn insert_message(&mut self, scan_id: i64, message: &RawMessage) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let sender = message.sender.as_ref().or(message.from.as_ref());
        let sender_email = sender.and_then(|addr| normalize_email(&addr.email));
        let sender_contact_id = sender
            .and_then(|addr| upsert_contact(&tx, addr).transpose())
            .transpose()?;

        if let Some(normalized) = &sender_email {
            upsert_domain(&tx, &normalized.domain)?;
        }

        let sender_kind = sender
            .map(|addr| classify_sender(&addr.email, message.headers.as_deref()).as_str())
            .unwrap_or("unknown");

        let source_id = message
            .source_id
            .as_deref()
            .or(message.internet_message_id.as_deref());
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO messages (
                scan_id, source_id, internet_message_id, conversation_id, in_reply_to,
                reference_ids, subject, sent_at, received_at, folder_path, sender_contact_id,
                sender_email, sender_domain, sender_kind, headers, size_bytes, has_attachments
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
            )",
            params![
                scan_id,
                source_id,
                message.internet_message_id,
                message.conversation_id,
                message.in_reply_to,
                message.references,
                message.subject,
                message.sent_at,
                message.received_at,
                message.folder_path,
                sender_contact_id,
                sender_email.as_ref().map(|email| email.address.as_str()),
                sender_email.as_ref().map(|email| email.domain.as_str()),
                sender_kind,
                message.headers,
                message.size_bytes,
                message.has_attachments,
            ],
        )?;

        if inserted == 0 {
            tx.commit()?;
            return Ok(false);
        }

        let message_id = tx.last_insert_rowid();
        insert_recipients(&tx, message_id, "to", &message.to)?;
        insert_recipients(&tx, message_id, "cc", &message.cc)?;
        insert_recipients(&tx, message_id, "bcc", &message.bcc)?;

        tx.commit()?;
        Ok(true)
    }

    pub fn stats(&self) -> Result<Stats> {
        Ok(Stats {
            scans: count(&self.conn, "scans")?,
            messages: count(&self.conn, "messages")?,
            contacts: count(&self.conn, "contacts")?,
            valid_contacts: self.conn.query_row(
                "SELECT COUNT(*) FROM contacts
                 WHERE sender_kind NOT IN ('no_reply', 'bulk', 'newsletter', 'promotional', 'automated', 'invalid')",
                [],
                |row| row.get(0),
            )?,
            filtered_contacts: self.conn.query_row(
                "SELECT COUNT(*) FROM contacts
                 WHERE sender_kind IN ('no_reply', 'bulk', 'newsletter', 'promotional', 'automated', 'invalid')",
                [],
                |row| row.get(0),
            )?,
            domains: count(&self.conn, "domains")?,
            attachments_flagged: self.conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE has_attachments = 1",
                [],
                |row| row.get(0),
            )?,
            sender_contacts: self.conn.query_row(
                "SELECT COUNT(DISTINCT ca.contact_id)
                 FROM contact_addresses ca
                 JOIN messages m ON m.sender_email = ca.email",
                [],
                |row| row.get(0),
            )?,
            recipient_contacts: self.conn.query_row(
                "SELECT COUNT(DISTINCT ca.contact_id)
                 FROM contact_addresses ca
                 JOIN message_recipients mr ON mr.email = ca.email",
                [],
                |row| row.get(0),
            )?,
            to_contacts: role_contact_count(&self.conn, "to")?,
            cc_contacts: role_contact_count(&self.conn, "cc")?,
            bcc_contacts: role_contact_count(&self.conn, "bcc")?,
            kind_counts: sender_kind_counts(&self.conn)?,
        })
    }

    pub fn contacts(&self, query: &ContactQuery) -> Result<Vec<ContactSummary>> {
        let mut stmt = self.conn.prepare(
            "WITH interactions AS (
                SELECT
                    ca.contact_id,
                    1 AS sent_count,
                    0 AS to_count,
                    0 AS cc_count,
                    0 AS bcc_count,
                    COALESCE(m.conversation_id, m.subject, m.internet_message_id, m.source_id) AS conversation_key,
                    COALESCE(m.sent_at, m.received_at) AS seen_at
                FROM contact_addresses ca
                JOIN messages m ON m.sender_email = ca.email

                UNION ALL

                SELECT
                    ca.contact_id,
                    0 AS sent_count,
                    CASE WHEN mr.kind = 'to' THEN 1 ELSE 0 END AS to_count,
                    CASE WHEN mr.kind = 'cc' THEN 1 ELSE 0 END AS cc_count,
                    CASE WHEN mr.kind = 'bcc' THEN 1 ELSE 0 END AS bcc_count,
                    COALESCE(m.conversation_id, m.subject, m.internet_message_id, m.source_id) AS conversation_key,
                    COALESCE(m.sent_at, m.received_at) AS seen_at
                FROM contact_addresses ca
                JOIN message_recipients mr ON mr.email = ca.email
                JOIN messages m ON m.id = mr.message_id
            ),
            contact_activity AS (
                SELECT
                    contact_id,
                    SUM(sent_count) AS sent_count,
                    SUM(to_count) AS to_count,
                    SUM(cc_count) AS cc_count,
                    SUM(bcc_count) AS bcc_count,
                    COUNT(DISTINCT conversation_key) AS conversation_count,
                    MIN(seen_at) AS first_seen_at,
                    MAX(seen_at) AS last_seen_at
                FROM interactions
                GROUP BY contact_id
            )
            SELECT
                c.display_name,
                ca.email,
                ca.domain,
                c.sender_kind,
                c.message_count,
                COALESCE(a.sent_count, 0),
                COALESCE(a.to_count, 0),
                COALESCE(a.cc_count, 0),
                COALESCE(a.bcc_count, 0),
                COALESCE(a.conversation_count, 0),
                (
                    COALESCE(a.sent_count, 0) * 4
                    + COALESCE(a.to_count, 0) * 3
                    + COALESCE(a.cc_count, 0) * 2
                    + COALESCE(a.bcc_count, 0)
                    + COALESCE(a.conversation_count, 0) * 6
                    + CASE WHEN COALESCE(a.sent_count, 0) > 0 AND COALESCE(a.to_count, 0) > 0 THEN 12 ELSE 0 END
                    - CASE WHEN c.sender_kind IN ('no_reply', 'bulk', 'newsletter', 'promotional', 'automated') THEN 1000 ELSE 0 END
                ) AS rank_score
             FROM contacts c
             JOIN contact_addresses ca ON ca.contact_id = c.id
             LEFT JOIN contact_activity a ON a.contact_id = c.id
             WHERE (?2 = 1 OR c.sender_kind NOT IN ('no_reply', 'bulk', 'newsletter', 'promotional', 'automated'))
               AND (?3 IS NULL OR c.sender_kind = ?3)
               AND (?4 IS NULL OR ca.domain = ?4)
               AND (
                    ?5 IS NULL
                    OR (?5 = 'sender' AND COALESCE(a.sent_count, 0) > 0)
                    OR (?5 = 'to' AND COALESCE(a.to_count, 0) > 0)
                    OR (?5 = 'cc' AND COALESCE(a.cc_count, 0) > 0)
                    OR (?5 = 'bcc' AND COALESCE(a.bcc_count, 0) > 0)
                    OR (?5 = 'recipient' AND (
                        COALESCE(a.to_count, 0)
                        + COALESCE(a.cc_count, 0)
                        + COALESCE(a.bcc_count, 0)
                    ) > 0)
               )
             ORDER BY rank_score DESC, a.conversation_count DESC, c.message_count DESC, ca.email ASC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map(
            params![
                query.limit,
                query.include_filtered,
                query.kind.as_deref(),
                query.domain.as_deref(),
                query.role.as_deref()
            ],
            |row| {
                Ok(ContactSummary {
                    display_name: row.get(0)?,
                    email: row.get(1)?,
                    domain: row.get(2)?,
                    sender_kind: row.get(3)?,
                    message_count: row.get(4)?,
                    sent_count: row.get(5)?,
                    direct_count: row.get(6)?,
                    cc_count: row.get(7)?,
                    bcc_count: row.get(8)?,
                    conversation_count: row.get(9)?,
                    rank_score: row.get(10)?,
                })
            },
        )?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn contact_count(&self, query: &ContactQuery) -> Result<i64> {
        self.conn
            .query_row(
                "WITH interactions AS (
                    SELECT
                        ca.contact_id,
                        1 AS sent_count,
                        0 AS to_count,
                        0 AS cc_count,
                        0 AS bcc_count
                    FROM contact_addresses ca
                    JOIN messages m ON m.sender_email = ca.email

                    UNION ALL

                    SELECT
                        ca.contact_id,
                        0 AS sent_count,
                        CASE WHEN mr.kind = 'to' THEN 1 ELSE 0 END AS to_count,
                        CASE WHEN mr.kind = 'cc' THEN 1 ELSE 0 END AS cc_count,
                        CASE WHEN mr.kind = 'bcc' THEN 1 ELSE 0 END AS bcc_count
                    FROM contact_addresses ca
                    JOIN message_recipients mr ON mr.email = ca.email
                ),
                contact_activity AS (
                    SELECT
                        contact_id,
                        SUM(sent_count) AS sent_count,
                        SUM(to_count) AS to_count,
                        SUM(cc_count) AS cc_count,
                        SUM(bcc_count) AS bcc_count
                    FROM interactions
                    GROUP BY contact_id
                )
                SELECT COUNT(*)
                FROM contacts c
                JOIN contact_addresses ca ON ca.contact_id = c.id
                LEFT JOIN contact_activity a ON a.contact_id = c.id
                WHERE (?1 = 1 OR c.sender_kind NOT IN ('no_reply', 'bulk', 'newsletter', 'promotional', 'automated'))
                  AND (?2 IS NULL OR c.sender_kind = ?2)
                  AND (?3 IS NULL OR ca.domain = ?3)
                  AND (
                       ?4 IS NULL
                       OR (?4 = 'sender' AND COALESCE(a.sent_count, 0) > 0)
                       OR (?4 = 'to' AND COALESCE(a.to_count, 0) > 0)
                       OR (?4 = 'cc' AND COALESCE(a.cc_count, 0) > 0)
                       OR (?4 = 'bcc' AND COALESCE(a.bcc_count, 0) > 0)
                       OR (?4 = 'recipient' AND (
                           COALESCE(a.to_count, 0)
                           + COALESCE(a.cc_count, 0)
                           + COALESCE(a.bcc_count, 0)
                       ) > 0)
                  )",
                params![
                    query.include_filtered,
                    query.kind.as_deref(),
                    query.domain.as_deref(),
                    query.role.as_deref()
                ],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS scans (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                archive_path TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                messages_seen INTEGER NOT NULL DEFAULT 0,
                messages_indexed INTEGER NOT NULL DEFAULT 0,
                malformed_messages INTEGER NOT NULL DEFAULT 0,
                error TEXT
            );

            CREATE TABLE IF NOT EXISTS domains (
                domain TEXT PRIMARY KEY,
                message_count INTEGER NOT NULL DEFAULT 0,
                contact_count INTEGER NOT NULL DEFAULT 0,
                first_seen_at TEXT,
                last_seen_at TEXT
            );

            CREATE TABLE IF NOT EXISTS contacts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                display_name TEXT,
                sender_kind TEXT NOT NULL DEFAULT 'person',
                first_seen_at TEXT,
                last_seen_at TEXT,
                message_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS contact_addresses (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                contact_id INTEGER NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
                email TEXT NOT NULL UNIQUE,
                domain TEXT NOT NULL REFERENCES domains(domain),
                display_name TEXT,
                first_seen_at TEXT,
                last_seen_at TEXT
            );

            CREATE TABLE IF NOT EXISTS conversations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_key TEXT NOT NULL UNIQUE,
                subject TEXT,
                first_seen_at TEXT,
                last_seen_at TEXT,
                message_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scan_id INTEGER NOT NULL REFERENCES scans(id) ON DELETE CASCADE,
                source_id TEXT,
                internet_message_id TEXT,
                conversation_id TEXT,
                in_reply_to TEXT,
                reference_ids TEXT,
                subject TEXT,
                sent_at TEXT,
                received_at TEXT,
                folder_path TEXT,
                sender_contact_id INTEGER REFERENCES contacts(id),
                sender_email TEXT,
                sender_domain TEXT,
                sender_kind TEXT NOT NULL,
                headers TEXT,
                size_bytes INTEGER,
                has_attachments INTEGER NOT NULL DEFAULT 0,
                malformed INTEGER NOT NULL DEFAULT 0,
                UNIQUE(source_id),
                UNIQUE(internet_message_id)
            );

            CREATE TABLE IF NOT EXISTS message_recipients (
                message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                contact_id INTEGER REFERENCES contacts(id),
                kind TEXT NOT NULL,
                email TEXT NOT NULL,
                display_name TEXT,
                domain TEXT,
                PRIMARY KEY (message_id, kind, email)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_sender_email ON messages(sender_email);
            CREATE INDEX IF NOT EXISTS idx_messages_sender_domain ON messages(sender_domain);
            CREATE INDEX IF NOT EXISTS idx_messages_sent_at ON messages(sent_at);
            CREATE INDEX IF NOT EXISTS idx_recipients_email ON message_recipients(email);
            CREATE INDEX IF NOT EXISTS idx_contact_addresses_domain ON contact_addresses(domain);

            PRAGMA user_version = 1;
            ",
        )?;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Stats {
    pub scans: i64,
    pub messages: i64,
    pub contacts: i64,
    pub valid_contacts: i64,
    pub filtered_contacts: i64,
    pub domains: i64,
    pub attachments_flagged: i64,
    pub sender_contacts: i64,
    pub recipient_contacts: i64,
    pub to_contacts: i64,
    pub cc_contacts: i64,
    pub bcc_contacts: i64,
    pub kind_counts: Vec<(String, i64)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ContactSummary {
    pub display_name: Option<String>,
    pub email: String,
    pub domain: String,
    pub sender_kind: String,
    pub message_count: i64,
    pub sent_count: i64,
    pub direct_count: i64,
    pub cc_count: i64,
    pub bcc_count: i64,
    pub conversation_count: i64,
    pub rank_score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactQuery {
    pub limit: i64,
    pub include_filtered: bool,
    pub kind: Option<String>,
    pub domain: Option<String>,
    pub role: Option<String>,
}

impl ContactQuery {
    pub fn new(limit: i64) -> Self {
        Self {
            limit,
            include_filtered: false,
            kind: None,
            domain: None,
            role: None,
        }
    }
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |row| row.get(0))
        .map_err(Into::into)
}

fn role_contact_count(conn: &Connection, kind: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(DISTINCT ca.contact_id)
         FROM contact_addresses ca
         JOIN message_recipients mr ON mr.email = ca.email
         WHERE mr.kind = ?1",
        params![kind],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn sender_kind_counts(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT sender_kind, COUNT(*)
         FROM contacts
         GROUP BY sender_kind
         ORDER BY COUNT(*) DESC, sender_kind ASC",
    )?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn upsert_domain(conn: &Connection, domain: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO domains (domain, first_seen_at, last_seen_at, contact_count)
         VALUES (?1, ?2, ?2, 1)
         ON CONFLICT(domain) DO UPDATE SET
            last_seen_at = excluded.last_seen_at",
        params![domain, now],
    )?;
    Ok(())
}

fn upsert_contact(conn: &Connection, address: &RawAddress) -> Result<Option<i64>> {
    let Some(normalized) = normalize_email(&address.email) else {
        return Ok(None);
    };

    upsert_domain(conn, &normalized.domain)?;

    let existing: Option<i64> = conn
        .query_row(
            "SELECT contact_id FROM contact_addresses WHERE email = ?1",
            params![normalized.address],
            |row| row.get(0),
        )
        .optional()?;

    let now = Utc::now().to_rfc3339();
    let sender_kind = classify_sender(&normalized.address, None).as_str();

    let contact_id = if let Some(id) = existing {
        conn.execute(
            "UPDATE contacts
             SET last_seen_at = ?1,
                 message_count = message_count + 1,
                 sender_kind = CASE
                    WHEN sender_kind = 'person' AND ?2 != 'person' THEN ?2
                    ELSE sender_kind
                 END
             WHERE id = ?3",
            params![now, sender_kind, id],
        )?;
        id
    } else {
        conn.execute(
            "INSERT INTO contacts (display_name, sender_kind, first_seen_at, last_seen_at, message_count)
             VALUES (?1, ?2, ?3, ?3, 1)",
            params![address.name, sender_kind, now],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO contact_addresses (contact_id, email, domain, display_name, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, normalized.address, normalized.domain, address.name, now],
        )?;
        id
    };

    Ok(Some(contact_id))
}

pub fn sender_kind_is_filtered(kind: &str) -> bool {
    is_filtered_sender_kind(kind)
}

fn insert_recipients(
    conn: &Connection,
    message_id: i64,
    kind: &str,
    addresses: &[RawAddress],
) -> Result<()> {
    for address in addresses {
        let Some(normalized) = normalize_email(&address.email) else {
            continue;
        };
        let contact_id = upsert_contact(conn, address)?;
        conn.execute(
            "INSERT OR IGNORE INTO message_recipients
             (message_id, contact_id, kind, email, display_name, domain)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                message_id,
                contact_id,
                kind,
                normalized.address,
                address.name,
                normalized.domain
            ],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{RawAddress, RawMessage};

    #[test]
    fn migrates_and_reports_empty_stats() {
        let db = Database::open_memory().unwrap();

        assert_eq!(
            db.stats().unwrap(),
            Stats {
                scans: 0,
                messages: 0,
                contacts: 0,
                valid_contacts: 0,
                filtered_contacts: 0,
                domains: 0,
                attachments_flagged: 0,
                sender_contacts: 0,
                recipient_contacts: 0,
                to_contacts: 0,
                cc_contacts: 0,
                bcc_contacts: 0,
                kind_counts: Vec::new(),
            }
        );
    }

    #[test]
    fn inserts_message_contacts_and_recipients() {
        let mut db = Database::open_memory().unwrap();
        let scan_id = db.begin_scan(Path::new("sample.pst")).unwrap();

        let message = RawMessage {
            internet_message_id: Some("<1@example.com>".to_string()),
            subject: Some("Hello".to_string()),
            sender: Some(RawAddress {
                name: Some("Alice".to_string()),
                email: "Alice@Example.com".to_string(),
            }),
            to: vec![RawAddress::new("bob@example.org")],
            has_attachments: true,
            ..RawMessage::default()
        };

        assert!(db.insert_message(scan_id, &message).unwrap());
        assert!(!db.insert_message(scan_id, &message).unwrap());

        let stats = db.stats().unwrap();
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.contacts, 2);
        assert_eq!(stats.valid_contacts, 2);
        assert_eq!(stats.filtered_contacts, 0);
        assert_eq!(stats.domains, 2);
        assert_eq!(stats.attachments_flagged, 1);
        assert_eq!(stats.sender_contacts, 1);
        assert_eq!(stats.recipient_contacts, 1);
        assert_eq!(stats.to_contacts, 1);
        assert_eq!(stats.cc_contacts, 0);
        assert_eq!(stats.bcc_contacts, 0);
    }

    #[test]
    fn ranks_people_above_filtered_senders() {
        let mut db = Database::open_memory().unwrap();
        let scan_id = db.begin_scan(Path::new("sample.pst")).unwrap();

        let real = RawMessage {
            internet_message_id: Some("<real@example.com>".to_string()),
            sender: Some(RawAddress::new("alice@example.com")),
            to: vec![RawAddress::new("me@example.org")],
            conversation_id: Some("thread-1".to_string()),
            ..RawMessage::default()
        };
        let newsletter = RawMessage {
            internet_message_id: Some("<news@example.com>".to_string()),
            sender: Some(RawAddress::new("newsletter@example.net")),
            headers: Some("List-Unsubscribe: <mailto:x>".to_string()),
            ..RawMessage::default()
        };

        db.insert_message(scan_id, &newsletter).unwrap();
        db.insert_message(scan_id, &real).unwrap();

        let contacts = db.contacts(&ContactQuery::new(10)).unwrap();
        assert!(
            contacts
                .iter()
                .any(|contact| contact.email == "alice@example.com")
        );
        assert!(
            !contacts
                .iter()
                .any(|contact| contact.email == "newsletter@example.net")
        );

        let all_contacts = db
            .contacts(&ContactQuery {
                limit: 10,
                include_filtered: true,
                kind: None,
                domain: None,
                role: None,
            })
            .unwrap();
        assert!(
            all_contacts
                .iter()
                .any(|contact| contact.email == "newsletter@example.net")
        );
    }

    #[test]
    fn filters_contacts_by_kind_and_domain() {
        let mut db = Database::open_memory().unwrap();
        let scan_id = db.begin_scan(Path::new("sample.pst")).unwrap();

        let message = RawMessage {
            internet_message_id: Some("<filters@example.com>".to_string()),
            sender: Some(RawAddress::new("alerts@example.com")),
            to: vec![RawAddress::new("bob@example.org")],
            ..RawMessage::default()
        };

        db.insert_message(scan_id, &message).unwrap();

        let contacts = db
            .contacts(&ContactQuery {
                limit: 10,
                include_filtered: true,
                kind: Some("automated".to_string()),
                domain: Some("example.com".to_string()),
                role: None,
            })
            .unwrap();

        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].email, "alerts@example.com");
    }

    #[test]
    fn filters_contacts_by_recipient_role() {
        let mut db = Database::open_memory().unwrap();
        let scan_id = db.begin_scan(Path::new("sample.pst")).unwrap();

        let message = RawMessage {
            internet_message_id: Some("<recipients@example.com>".to_string()),
            subject: Some("Recipient test".to_string()),
            sender: Some(RawAddress::new("alice@example.com")),
            to: vec![RawAddress::new("bob@example.org")],
            cc: vec![RawAddress::new("carol@example.net")],
            bcc: vec![RawAddress::new("dave@example.dev")],
            ..RawMessage::default()
        };

        db.insert_message(scan_id, &message).unwrap();

        let contacts = db
            .contacts(&ContactQuery {
                limit: 10,
                include_filtered: true,
                kind: None,
                domain: None,
                role: Some("cc".to_string()),
            })
            .unwrap();

        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].email, "carol@example.net");
        assert_eq!(contacts[0].cc_count, 1);

        let count = db
            .contact_count(&ContactQuery {
                limit: 1,
                include_filtered: true,
                kind: None,
                domain: None,
                role: Some("cc".to_string()),
            })
            .unwrap();
        assert_eq!(count, 1);
    }
}
