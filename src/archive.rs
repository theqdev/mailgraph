use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAddress {
    pub name: Option<String>,
    pub email: String,
}

impl RawAddress {
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawMessage {
    pub source_id: Option<String>,
    pub internet_message_id: Option<String>,
    pub conversation_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub subject: Option<String>,
    pub sent_at: Option<String>,
    pub received_at: Option<String>,
    pub folder_path: Option<String>,
    pub sender: Option<RawAddress>,
    pub from: Option<RawAddress>,
    pub to: Vec<RawAddress>,
    pub cc: Vec<RawAddress>,
    pub bcc: Vec<RawAddress>,
    pub headers: Option<String>,
    pub size_bytes: Option<i64>,
    pub has_attachments: bool,
}

pub trait ArchiveReader {
    fn archive_path(&self) -> &Path;
    fn messages(&mut self) -> Result<Box<dyn Iterator<Item = Result<RawMessage>> + '_>>;
}

#[derive(Debug, Clone)]
pub struct Rfc822ArchiveReader {
    path: PathBuf,
    files: Vec<PathBuf>,
}

impl Rfc822ArchiveReader {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            bail!("archive path cannot be empty");
        }

        let files = collect_message_files(&path)?;
        Ok(Self { path, files })
    }
}

impl ArchiveReader for Rfc822ArchiveReader {
    fn archive_path(&self) -> &Path {
        &self.path
    }

    fn messages(&mut self) -> Result<Box<dyn Iterator<Item = Result<RawMessage>> + '_>> {
        Ok(Box::new(self.files.iter().map(|path| {
            parse_rfc822_file(path, Some(self.path.as_path()))
                .with_context(|| format!("failed to parse {}", path.display()))
        })))
    }
}

#[derive(Debug)]
pub struct PffExportArchiveReader {
    path: PathBuf,
    export_dir: Option<TempDir>,
    message_dirs: Vec<PathBuf>,
}

impl PffExportArchiveReader {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            bail!("archive path cannot be empty");
        }

        Ok(Self {
            path,
            export_dir: None,
            message_dirs: Vec::new(),
        })
    }

    fn ensure_exported(&mut self) -> Result<()> {
        if self.export_dir.is_some() {
            return Ok(());
        }

        let tempdir = tempfile::Builder::new()
            .prefix("mailgraph-pffexport-")
            .tempdir()
            .context("failed to create temporary pffexport directory")?;

        let target_base = tempdir.path().join("archive");
        let output = Command::new("pffexport")
            .arg("-q")
            .arg("-t")
            .arg(&target_base)
            .arg(&self.path)
            .output()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    anyhow!(
                        "pffexport is required for PST/OST scans. Install it with: sudo apt install pff-tools"
                    )
                } else {
                    error.into()
                }
            })?;

        if !output.status.success() {
            bail!(
                "pffexport failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let export_roots = [
            target_base.with_extension("export"),
            target_base.with_extension("orphans"),
            target_base.with_extension("recovered"),
        ];
        self.message_dirs =
            collect_pff_message_dirs(export_roots.iter().filter(|path| path.exists()))?;
        if self.message_dirs.is_empty() {
            bail!(
                "pffexport completed, but no exported message directories were found under {}",
                tempdir.path().display()
            );
        }

        tracing::info!(
            archive = %self.path.display(),
            messages = self.message_dirs.len(),
            "pffexport completed"
        );
        self.export_dir = Some(tempdir);
        Ok(())
    }
}

impl ArchiveReader for PffExportArchiveReader {
    fn archive_path(&self) -> &Path {
        &self.path
    }

    fn messages(&mut self) -> Result<Box<dyn Iterator<Item = Result<RawMessage>> + '_>> {
        self.ensure_exported()?;
        let root = self
            .export_dir
            .as_ref()
            .map(|dir| dir.path().to_path_buf())
            .context("missing pffexport directory")?;

        Ok(Box::new(self.message_dirs.iter().map(move |path| {
            parse_pff_message_dir(path, root.as_path())
                .with_context(|| format!("failed to parse exported message {}", path.display()))
        })))
    }
}

#[derive(Debug)]
pub struct PffDirectoryArchiveReader {
    path: PathBuf,
    message_dirs: Vec<PathBuf>,
}

impl PffDirectoryArchiveReader {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            bail!("export path cannot be empty");
        }

        let message_dirs = collect_pff_message_dirs(std::iter::once(&path))?;
        if message_dirs.is_empty() {
            bail!(
                "no exported pff message directories were found under {}",
                path.display()
            );
        }

        Ok(Self { path, message_dirs })
    }
}

impl ArchiveReader for PffDirectoryArchiveReader {
    fn archive_path(&self) -> &Path {
        &self.path
    }

    fn messages(&mut self) -> Result<Box<dyn Iterator<Item = Result<RawMessage>> + '_>> {
        Ok(Box::new(self.message_dirs.iter().map(|path| {
            parse_pff_message_dir(path, self.path.as_path())
                .with_context(|| format!("failed to parse exported message {}", path.display()))
        })))
    }
}

fn collect_pff_message_dirs<'a>(roots: impl Iterator<Item = &'a PathBuf>) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for root in roots {
        collect_pff_message_dirs_recursive(root, &mut dirs)?;
    }
    dirs.sort();
    Ok(dirs)
}

fn collect_pff_message_dirs_recursive(path: &Path, dirs: &mut Vec<PathBuf>) -> Result<()> {
    if path.join("InternetHeaders.txt").is_file() || path.join("OutlookHeaders.txt").is_file() {
        dirs.push(path.to_path_buf());
        return Ok(());
    }

    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_pff_message_dirs_recursive(&path, dirs)?;
        }
    }

    Ok(())
}

fn collect_message_files(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if path.is_file() {
        files.push(path.to_path_buf());
    } else {
        collect_message_files_recursive(path, &mut files)?;
    }
    files.sort();
    Ok(files)
}

fn collect_message_files_recursive(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_message_files_recursive(&path, files)?;
        } else if looks_like_message_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn looks_like_message_file(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "eml" | "txt" | "message" | "email"
            )
        })
        .unwrap_or(true)
}

fn parse_rfc822_file(path: &Path, root: Option<&Path>) -> Result<RawMessage> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let headers = parse_headers(&text);
    let folder_path = root.and_then(|root| {
        path.parent()
            .and_then(|parent| parent.strip_prefix(root).ok())
            .map(|relative| relative.display().to_string())
            .filter(|folder| !folder.is_empty())
    });

    let source_id = Some(path.display().to_string());
    let header_text = headers
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(RawMessage {
        source_id,
        internet_message_id: first_header(&headers, &["message-id", "message id"]),
        conversation_id: first_header(&headers, &["thread-index", "conversation-id"]),
        in_reply_to: first_header(&headers, &["in-reply-to"]),
        references: first_header(&headers, &["references"]),
        subject: first_header(&headers, &["subject"]),
        sent_at: first_header(&headers, &["date"]),
        received_at: first_header(&headers, &["delivery-date", "received"]),
        folder_path,
        sender: first_header(&headers, &["sender"])
            .or_else(|| first_header(&headers, &["from"]))
            .and_then(|value| parse_addresses(&value).into_iter().next()),
        from: first_header(&headers, &["from"])
            .and_then(|value| parse_addresses(&value).into_iter().next()),
        to: first_header(&headers, &["to"])
            .map(|value| parse_addresses(&value))
            .unwrap_or_default(),
        cc: first_header(&headers, &["cc"])
            .map(|value| parse_addresses(&value))
            .unwrap_or_default(),
        bcc: first_header(&headers, &["bcc"])
            .map(|value| parse_addresses(&value))
            .unwrap_or_default(),
        headers: (!header_text.is_empty()).then_some(header_text),
        size_bytes: Some(bytes.len() as i64),
        has_attachments: text
            .to_ascii_lowercase()
            .contains("content-disposition: attachment"),
    })
}

fn parse_pff_message_dir(path: &Path, root: &Path) -> Result<RawMessage> {
    let internet_headers_path = path.join("InternetHeaders.txt");
    let outlook_headers_path = path.join("OutlookHeaders.txt");
    let recipients_path = path.join("Recipients.txt");

    let internet_headers = fs::read_to_string(&internet_headers_path).unwrap_or_default();
    let outlook_headers = fs::read_to_string(&outlook_headers_path).unwrap_or_default();
    let recipient_text = fs::read_to_string(&recipients_path).unwrap_or_default();

    let headers = parse_headers(&internet_headers);
    let outlook = parse_pff_key_values(&outlook_headers);
    let source_id = Some(path.display().to_string());
    let folder_path = path
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .map(|relative| relative.display().to_string())
        .filter(|folder| !folder.is_empty());

    let sender = first_header(&headers, &["sender"])
        .or_else(|| first_header(&headers, &["from"]))
        .and_then(|value| parse_addresses(&value).into_iter().next())
        .or_else(|| {
            let email = outlook.get("sender email address")?;
            Some(RawAddress {
                name: outlook.get("sender name").cloned(),
                email: email.clone(),
            })
        });

    let from = first_header(&headers, &["from"])
        .and_then(|value| parse_addresses(&value).into_iter().next())
        .or_else(|| sender.clone());

    let mut to = first_header(&headers, &["to"])
        .map(|value| parse_addresses(&value))
        .unwrap_or_default();
    let mut cc = first_header(&headers, &["cc"])
        .map(|value| parse_addresses(&value))
        .unwrap_or_default();
    let mut bcc = first_header(&headers, &["bcc"])
        .map(|value| parse_addresses(&value))
        .unwrap_or_default();

    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        let recipients = parse_pff_recipients(&recipient_text);
        to = recipients.to;
        cc = recipients.cc;
        bcc = recipients.bcc;
    }

    Ok(RawMessage {
        source_id,
        internet_message_id: first_header(&headers, &["message-id", "message id"]),
        conversation_id: outlook
            .get("conversation topic")
            .cloned()
            .or_else(|| first_header(&headers, &["thread-index", "conversation-id"])),
        in_reply_to: first_header(&headers, &["in-reply-to"]),
        references: first_header(&headers, &["references"]),
        subject: first_header(&headers, &["subject"]).or_else(|| outlook.get("subject").cloned()),
        sent_at: first_header(&headers, &["date"])
            .or_else(|| outlook.get("client submit time").cloned()),
        received_at: outlook
            .get("delivery time")
            .cloned()
            .or_else(|| first_header(&headers, &["delivery-date", "received"])),
        folder_path,
        sender,
        from,
        to,
        cc,
        bcc,
        headers: (!internet_headers.trim().is_empty()).then_some(internet_headers),
        size_bytes: outlook
            .get("size")
            .and_then(|size| size.parse::<i64>().ok()),
        has_attachments: path.join("Attachments").is_dir(),
    })
}

fn parse_pff_key_values(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| {
            (
                key.trim().to_ascii_lowercase(),
                value.trim().trim_matches('\t').trim().to_string(),
            )
        })
        .filter(|(_, value)| !value.is_empty())
        .collect()
}

#[derive(Default)]
struct PffRecipients {
    to: Vec<RawAddress>,
    cc: Vec<RawAddress>,
    bcc: Vec<RawAddress>,
}

fn parse_pff_recipients(text: &str) -> PffRecipients {
    let mut recipients = PffRecipients::default();
    let mut name: Option<String> = None;
    let mut email: Option<String> = None;
    let mut kind: Option<String> = None;

    for line in text.lines().chain(std::iter::once("")) {
        let line = line.trim();
        if line.is_empty() {
            if let Some(email) = email.take() {
                let recipient = RawAddress {
                    name: name.take(),
                    email,
                };
                match kind.as_deref() {
                    Some("CC") => recipients.cc.push(recipient),
                    Some("BCC") => recipients.bcc.push(recipient),
                    _ => recipients.to.push(recipient),
                }
            }
            name = None;
            kind = None;
            continue;
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };

        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "display name" => name = Some(value.to_string()),
            "email address" => email = Some(value.to_string()),
            "recipient type" => kind = Some(value.to_ascii_uppercase()),
            _ => {}
        }
    }

    recipients
}

fn parse_headers(text: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    let mut current_key: Option<String> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }

        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(key) = &current_key {
                let value = headers.entry(key.clone()).or_insert_with(String::new);
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };

        let key = key.trim().to_ascii_lowercase();
        headers
            .entry(key.clone())
            .and_modify(|existing: &mut String| {
                existing.push('\n');
                existing.push_str(value.trim());
            })
            .or_insert_with(|| value.trim().to_string());
        current_key = Some(key);
    }

    headers
}

fn first_header(headers: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| headers.get(*key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_addresses(value: &str) -> Vec<RawAddress> {
    value
        .split(',')
        .filter_map(|part| parse_address(part.trim()))
        .collect()
}

fn parse_address(part: &str) -> Option<RawAddress> {
    if part.is_empty() {
        return None;
    }

    if let Some((name, rest)) = part.rsplit_once('<') {
        let email = rest.trim().trim_end_matches('>').trim();
        if email.contains('@') {
            let name = name.trim().trim_matches('"');
            return Some(RawAddress {
                name: (!name.is_empty()).then_some(name.to_string()),
                email: email.to_string(),
            });
        }
    }

    let email = part.trim().trim_matches('"');
    email.contains('@').then(|| RawAddress::new(email))
}

#[cfg(test)]
pub(crate) struct VecArchiveReader {
    path: PathBuf,
    messages: Vec<RawMessage>,
}

#[cfg(test)]
impl VecArchiveReader {
    pub(crate) fn new(messages: Vec<RawMessage>) -> Self {
        Self {
            path: PathBuf::from("test.pst"),
            messages,
        }
    }
}

#[cfg(test)]
impl ArchiveReader for VecArchiveReader {
    fn archive_path(&self) -> &Path {
        &self.path
    }

    fn messages(&mut self) -> Result<Box<dyn Iterator<Item = Result<RawMessage>> + '_>> {
        Ok(Box::new(self.messages.clone().into_iter().map(Ok)))
    }
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    #[test]
    fn parses_rfc822_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sample.eml");
        fs::write(
            &file,
            "From: Alice <alice@example.com>\nTo: Bob <bob@example.org>\nCc: newsletter@example.net\nSubject: Hello\nMessage-ID: <1@example.com>\nDate: Tue, 30 Jun 2026 10:00:00 +0000\nList-Unsubscribe: <mailto:leave@example.net>\n\nBody ignored",
        )
        .unwrap();

        let message = parse_rfc822_file(&file, Some(dir.path())).unwrap();

        assert_eq!(
            message.internet_message_id.as_deref(),
            Some("<1@example.com>")
        );
        assert_eq!(message.sender.unwrap().email, "alice@example.com");
        assert_eq!(message.to[0].email, "bob@example.org");
        assert_eq!(message.cc[0].email, "newsletter@example.net");
        assert!(message.headers.unwrap().contains("list-unsubscribe"));
    }
}
