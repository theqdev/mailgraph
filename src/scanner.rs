use anyhow::Result;

use crate::archive::ArchiveReader;
use crate::db::Database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanSummary {
    pub scan_id: i64,
    pub messages_seen: i64,
    pub messages_indexed: i64,
    pub messages_already_indexed: i64,
    pub malformed_messages: i64,
}

pub fn scan_archive(db: &mut Database, reader: &mut dyn ArchiveReader) -> Result<ScanSummary> {
    let scan_id = db.begin_scan(reader.archive_path())?;
    let mut summary = ScanSummary {
        scan_id,
        messages_seen: 0,
        messages_indexed: 0,
        messages_already_indexed: 0,
        malformed_messages: 0,
    };

    let result = (|| -> Result<()> {
        for item in reader.messages()? {
            summary.messages_seen += 1;

            match item {
                Ok(message) => {
                    if db.insert_message(scan_id, &message)? {
                        summary.messages_indexed += 1;
                    } else {
                        summary.messages_already_indexed += 1;
                    }
                }
                Err(error) => {
                    summary.malformed_messages += 1;
                    tracing::warn!(%error, "skipping malformed message");
                }
            }

            if summary.messages_seen % 500 == 0 {
                db.update_scan_progress(scan_id, summary.messages_seen, summary.messages_indexed)?;
                tracing::info!(
                    seen = summary.messages_seen,
                    indexed = summary.messages_indexed,
                    "scan progress"
                );
            }
        }

        db.update_scan_progress(scan_id, summary.messages_seen, summary.messages_indexed)?;
        Ok(())
    })();

    match result {
        Ok(()) => db.finish_scan(scan_id, "completed", None)?,
        Err(error) => {
            db.finish_scan(scan_id, "failed", Some(&error.to_string()))?;
            return Err(error);
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{RawAddress, RawMessage, VecArchiveReader};

    #[test]
    fn scans_messages_through_reader_trait() {
        let mut db = Database::open_memory().unwrap();
        let mut reader = VecArchiveReader::new(vec![RawMessage {
            internet_message_id: Some("<scan-test@example.com>".to_string()),
            sender: Some(RawAddress::new("alice@example.com")),
            to: vec![RawAddress::new("bob@example.org")],
            ..RawMessage::default()
        }]);

        let summary = scan_archive(&mut db, &mut reader).unwrap();

        assert_eq!(summary.messages_seen, 1);
        assert_eq!(summary.messages_indexed, 1);
        assert_eq!(summary.messages_already_indexed, 0);
        assert_eq!(db.stats().unwrap().messages, 1);
    }
}
