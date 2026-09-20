use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt};

use crate::archive::{PffDirectoryArchiveReader, PffExportArchiveReader, Rfc822ArchiveReader};
use crate::db::{ContactQuery, ContactSummary, Database};
use crate::scanner::scan_archive;

#[derive(Debug, Parser)]
#[command(name = "mailgraph")]
#[command(about = "Build a private graph of people and conversations from mail archives")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scan an Outlook PST/OST archive into a local SQLite database.
    Scan {
        /// Path to the PST/OST archive.
        archive: PathBuf,

        /// SQLite database path.
        #[arg(long)]
        db: Option<PathBuf>,
    },

    /// List indexed contacts.
    Contacts(Box<ContactsArgs>),

    /// Scan exported RFC822/EML files. Useful for fixtures and pffexport output.
    ScanEml {
        /// Path to an EML file or directory of exported message files.
        path: PathBuf,

        /// SQLite database path.
        #[arg(long)]
        db: Option<PathBuf>,
    },

    /// Scan a directory already produced by pffexport.
    ScanPffExport {
        /// Path to a .export, .orphans, or .recovered directory.
        path: PathBuf,

        /// SQLite database path.
        #[arg(long)]
        db: Option<PathBuf>,
    },

    /// Print index statistics.
    Stats {
        /// SQLite database path.
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

#[derive(Debug, Args)]
struct ContactsArgs {
    /// SQLite database path.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Maximum contacts to print.
    #[arg(long, default_value_t = 50)]
    limit: i64,

    /// Show every indexed contact, including filtered senders, using a high limit.
    #[arg(long)]
    all: bool,

    /// Include no-reply, bulk, newsletter, promotional, and automated senders.
    #[arg(long)]
    include_filtered: bool,

    /// Only show one sender kind: person, no_reply, bulk, newsletter, promotional, automated, invalid.
    #[arg(long)]
    kind: Option<String>,

    /// Exclude a sender kind: no_reply, bulk, newsletter, promotional, automated, invalid, or person. Can be repeated.
    #[arg(long)]
    exclude_kind: Vec<String>,

    /// Only show contacts from this normalized domain.
    #[arg(long)]
    domain: Option<String>,

    /// Only show contacts whose domain contains this text.
    #[arg(long)]
    domain_contains: Vec<String>,

    /// Exclude an exact email address. Can be repeated.
    #[arg(long)]
    exclude_email: Vec<String>,

    /// Exclude contacts whose email address contains this text. Can be repeated.
    #[arg(long)]
    exclude_email_contains: Vec<String>,

    /// Exclude an exact domain. Can be repeated.
    #[arg(long)]
    exclude_domain: Vec<String>,

    /// Exclude contacts whose domain contains this text.
    #[arg(long)]
    exclude_domain_contains: Vec<String>,

    /// Only show contacts seen in this role: sender, recipient, to, cc, or bcc.
    #[arg(long)]
    role: Option<String>,

    /// Output contacts as CSV.
    #[arg(long)]
    csv: bool,

    /// CSV columns to export, comma-separated. Aliases: name, score, to.
    #[arg(long)]
    columns: Option<String>,

    /// CSV column preset: address-book, ranked, or full.
    #[arg(long)]
    preset: Option<String>,
}

pub fn run() -> Result<()> {
    init_logging();
    run_with_args(Cli::parse())
}

fn run_with_args(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Scan { archive, db } => {
            let db = resolve_scan_db_path(&archive, db)?;
            remember_current_db(&db)?;
            let mut database = Database::open(&db)?;
            let mut reader = PffExportArchiveReader::open(archive)?;
            let started = Instant::now();
            let summary = scan_archive(&mut database, &mut reader)?;

            print_scan_report(summary, &db, started.elapsed());
        }
        Command::ScanEml { path, db } => {
            let db = resolve_scan_db_path(&path, db)?;
            remember_current_db(&db)?;
            let mut database = Database::open(&db)?;
            let mut reader = Rfc822ArchiveReader::open(path)?;
            let started = Instant::now();
            let summary = scan_archive(&mut database, &mut reader)?;

            print_scan_report(summary, &db, started.elapsed());
        }
        Command::ScanPffExport { path, db } => {
            let db = resolve_scan_db_path(&path, db)?;
            remember_current_db(&db)?;
            let mut database = Database::open(&db)?;
            let mut reader = PffDirectoryArchiveReader::open(path)?;
            let started = Instant::now();
            let summary = scan_archive(&mut database, &mut reader)?;

            print_scan_report(summary, &db, started.elapsed());
        }
        Command::Contacts(args) => {
            let db = resolve_existing_db_path(args.db.clone())?;
            let database = Database::open(db)?;
            let query = build_contact_query(&args)?;
            let contacts = database.contacts(&query)?;
            let matched_contacts = if args.csv {
                None
            } else {
                Some(database.contact_count(&query)?)
            };
            let csv_columns = args
                .csv
                .then(|| resolve_csv_columns(args.columns.as_deref(), args.preset.as_deref()))
                .transpose()?;

            if contacts.is_empty() {
                if args.csv {
                    print_contacts_csv(&contacts, &csv_columns.expect("csv columns are resolved"));
                } else {
                    print_contacts_summary(&query, 0, matched_contacts.unwrap_or(0));
                    println!("No contacts matched.");
                }
            } else if let Some(columns) = csv_columns {
                print_contacts_csv(&contacts, &columns);
            } else {
                print_contacts_summary(&query, contacts.len(), matched_contacts.unwrap_or(0));
                print_contacts_human(&contacts);
            }
        }
        Command::Stats { db } => {
            let db = resolve_existing_db_path(db)?;
            let database = Database::open(db)?;
            let stats = database.stats()?;

            println!("scans: {}", stats.scans);
            println!("messages: {}", stats.messages);
            println!("domains: {}", stats.domains);
            println!("messages_with_attachments: {}", stats.attachments_flagged);
            println!();
            println!("contacts_total: {}", stats.contacts);
            println!("contacts_valid: {}", stats.valid_contacts);
            println!("contacts_filtered: {}", stats.filtered_contacts);
            println!("contacts_as_senders: {}", stats.sender_contacts);
            println!("contacts_as_recipients: {}", stats.recipient_contacts);
            println!("contacts_to: {}", stats.to_contacts);
            println!("contacts_cc: {}", stats.cc_contacts);
            println!("contacts_bcc: {}", stats.bcc_contacts);
            if !stats.kind_counts.is_empty() {
                println!();
                println!("contacts_by_kind:");
                for (kind, count) in stats.kind_counts {
                    println!("  {kind}: {count}");
                }
            }
            println!();
            println!("Useful exports:");
            println!("  cargo run -- contacts --csv --preset address-book > valid-contacts.csv");
            println!(
                "  cargo run -- contacts --all --csv --preset address-book > all-contacts.csv"
            );
            println!(
                "  cargo run -- contacts --role cc --all --csv --preset address-book > cc-contacts.csv"
            );
        }
    }

    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt().with_env_filter(filter).without_time().try_init();
}

fn print_scan_report(summary: crate::scanner::ScanSummary, db: &Path, elapsed: Duration) {
    println!("Mailgraph scan complete");
    println!("Database: {}", db.display());
    println!("Elapsed: {}", format_duration(elapsed));
    println!("Scan ID: {}", summary.scan_id);
    println!("Messages seen: {}", summary.messages_seen);
    println!("Messages indexed: {}", summary.messages_indexed);
    println!("Already indexed: {}", summary.messages_already_indexed);
    println!("Malformed messages: {}", summary.malformed_messages);
    println!();
    println!("Next commands:");
    println!("1. cargo run -- stats");
    println!("2. cargo run -- contacts --all");
    println!("3. cargo run -- contacts --all --csv --preset address-book > all-contacts.csv");
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let millis = duration.subsec_millis();

    if total_seconds >= 3600 {
        let hours = total_seconds / 3600;
        let minutes = (total_seconds % 3600) / 60;
        let seconds = total_seconds % 60;
        format!("{hours}h {minutes}m {seconds}s")
    } else if total_seconds >= 60 {
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("{minutes}m {seconds}s")
    } else if total_seconds > 0 {
        format!("{total_seconds}.{millis:03}s")
    } else {
        format!("{millis}ms")
    }
}

fn resolve_scan_db_path(input: &Path, db: Option<PathBuf>) -> Result<PathBuf> {
    let path = db.unwrap_or_else(|| generated_db_path(input));
    make_absolute(&path)
}

fn resolve_existing_db_path(db: Option<PathBuf>) -> Result<PathBuf> {
    let path = match db {
        Some(path) => path,
        None => read_current_db()?.unwrap_or_else(|| PathBuf::from("mailgraph.sqlite")),
    };
    make_absolute(&path)
}

fn generated_db_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("mailgraph");
    PathBuf::from(format!("{stem}.mailgraph.sqlite"))
}

fn make_absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("failed to determine current directory")?
            .join(path))
    }
}

fn current_db_pointer_path() -> PathBuf {
    PathBuf::from(".mailgraph").join("current-db")
}

fn remember_current_db(db: &Path) -> Result<()> {
    let pointer = current_db_pointer_path();
    if let Some(parent) = pointer.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&pointer, db.display().to_string())
        .with_context(|| format!("failed to write {}", pointer.display()))?;
    Ok(())
}

fn read_current_db() -> Result<Option<PathBuf>> {
    let pointer = current_db_pointer_path();
    if !pointer.exists() {
        return Ok(None);
    }

    let value = fs::read_to_string(&pointer)
        .with_context(|| format!("failed to read {}", pointer.display()))?;
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(value)))
    }
}

fn build_contact_query(args: &ContactsArgs) -> Result<ContactQuery> {
    let limit = if args.all { 100_000 } else { args.limit };
    if limit < 1 {
        bail!("--limit must be at least 1");
    }

    let kind = args
        .kind
        .as_ref()
        .map(|kind| kind.trim().to_ascii_lowercase());
    if let Some(kind) = &kind
        && !is_valid_contact_kind(kind)
    {
        bail!(
            "unknown contact kind '{kind}'. Expected one of: person, no_reply, bulk, newsletter, promotional, automated, invalid"
        );
    }

    let exclude_kinds = normalize_filter_list(&args.exclude_kind);
    if let Some(exclude_kinds) = &exclude_kinds {
        for kind in exclude_kinds.split(',') {
            if !is_valid_contact_kind(kind) {
                bail!(
                    "unknown excluded contact kind '{kind}'. Expected one of: person, no_reply, bulk, newsletter, promotional, automated, invalid"
                );
            }
        }
    }

    let domain = normalize_optional_filter(args.domain.as_deref());
    let domain_contains = normalize_filter_list(&args.domain_contains);
    if domain.is_some() && domain_contains.is_some() {
        bail!("use either --domain or --domain-contains, not both");
    }

    let exclude_emails = normalize_filter_list(&args.exclude_email);
    let exclude_email_contains = normalize_filter_list(&args.exclude_email_contains);
    let exclude_domains = normalize_filter_list(&args.exclude_domain);
    let exclude_domain_contains = normalize_filter_list(&args.exclude_domain_contains);

    let role = args
        .role
        .as_ref()
        .map(|role| role.trim().to_ascii_lowercase());
    if let Some(role) = &role {
        let valid = matches!(role.as_str(), "sender" | "recipient" | "to" | "cc" | "bcc");
        if !valid {
            bail!("unknown contact role '{role}'. Expected one of: sender, recipient, to, cc, bcc");
        }
    }

    Ok(ContactQuery {
        limit,
        include_filtered: args.all || args.include_filtered || kind.is_some(),
        kind,
        exclude_kinds,
        domain,
        role,
        domain_contains,
        exclude_emails,
        exclude_email_contains,
        exclude_domains,
        exclude_domain_contains,
    })
}

fn is_valid_contact_kind(kind: &str) -> bool {
    matches!(
        kind,
        "person" | "no_reply" | "bulk" | "newsletter" | "promotional" | "automated" | "invalid"
    )
}

fn normalize_optional_filter(value: Option<&str>) -> Option<String> {
    value
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn normalize_filter_list(values: &[String]) -> Option<String> {
    let values = values
        .iter()
        .flat_map(|value| {
            value
                .split(',')
                .map(|part| part.trim().to_ascii_lowercase())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    (!values.is_empty()).then(|| values.join(","))
}

fn print_contacts_human(contacts: &[ContactSummary]) {
    for contact in contacts {
        let name = contact.display_name.as_deref().unwrap_or("-");
        println!(
            "{}\t{}\t{}\t{}\tmessages={}\tsent={}\tto={}\tcc={}\tbcc={}\tconversations={}\tscore={}",
            contact.email,
            name,
            contact.domain,
            contact.sender_kind,
            contact.message_count,
            contact.sent_count,
            contact.direct_count,
            contact.cc_count,
            contact.bcc_count,
            contact.conversation_count,
            contact.rank_score
        );
    }
}

fn print_contacts_summary(query: &ContactQuery, listed: usize, matched: i64) {
    println!("Contacts matched: {matched}");
    println!("Contacts listed: {listed}");
    println!("Limit: {}", query.limit);

    let filters = contact_filter_labels(query);
    if !filters.is_empty() {
        println!("Filters: {}", filters.join(", "));
    } else if query.include_filtered {
        println!("Filters: include_filtered=true");
    } else {
        println!("Filters: valid contacts only");
    }

    println!();
}

fn contact_filter_labels(query: &ContactQuery) -> Vec<String> {
    let mut labels = Vec::new();

    if query.include_filtered {
        labels.push("include_filtered=true".to_string());
    } else {
        labels.push("valid_only=true".to_string());
    }

    if let Some(kind) = &query.kind {
        labels.push(format!("kind={kind}"));
    }
    if let Some(exclude_kinds) = &query.exclude_kinds {
        labels.push(format!("exclude_kind={exclude_kinds}"));
    }
    if let Some(domain) = &query.domain {
        labels.push(format!("domain={domain}"));
    }
    if let Some(domain_contains) = &query.domain_contains {
        labels.push(format!("domain_contains={domain_contains}"));
    }
    if let Some(role) = &query.role {
        labels.push(format!("role={role}"));
    }
    if let Some(exclude_emails) = &query.exclude_emails {
        labels.push(format!("exclude_email={exclude_emails}"));
    }
    if let Some(exclude_email_contains) = &query.exclude_email_contains {
        labels.push(format!("exclude_email_contains={exclude_email_contains}"));
    }
    if let Some(exclude_domains) = &query.exclude_domains {
        labels.push(format!("exclude_domain={exclude_domains}"));
    }
    if let Some(exclude_domain_contains) = &query.exclude_domain_contains {
        labels.push(format!("exclude_domain_contains={exclude_domain_contains}"));
    }

    labels
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContactColumn {
    Email,
    DisplayName,
    Domain,
    Kind,
    Messages,
    Sent,
    To,
    Cc,
    Bcc,
    Conversations,
    RankScore,
}

impl ContactColumn {
    fn canonical_name(self) -> &'static str {
        match self {
            ContactColumn::Email => "email",
            ContactColumn::DisplayName => "display_name",
            ContactColumn::Domain => "domain",
            ContactColumn::Kind => "kind",
            ContactColumn::Messages => "messages",
            ContactColumn::Sent => "sent",
            ContactColumn::To => "to",
            ContactColumn::Cc => "cc",
            ContactColumn::Bcc => "bcc",
            ContactColumn::Conversations => "conversations",
            ContactColumn::RankScore => "rank_score",
        }
    }

    fn value(self, contact: &ContactSummary) -> String {
        match self {
            ContactColumn::Email => contact.email.clone(),
            ContactColumn::DisplayName => contact.display_name.clone().unwrap_or_default(),
            ContactColumn::Domain => contact.domain.clone(),
            ContactColumn::Kind => contact.sender_kind.clone(),
            ContactColumn::Messages => contact.message_count.to_string(),
            ContactColumn::Sent => contact.sent_count.to_string(),
            ContactColumn::To => contact.direct_count.to_string(),
            ContactColumn::Cc => contact.cc_count.to_string(),
            ContactColumn::Bcc => contact.bcc_count.to_string(),
            ContactColumn::Conversations => contact.conversation_count.to_string(),
            ContactColumn::RankScore => contact.rank_score.to_string(),
        }
    }
}

fn resolve_csv_columns(columns: Option<&str>, preset: Option<&str>) -> Result<Vec<ContactColumn>> {
    if columns.is_some() && preset.is_some() {
        bail!("use either --columns or --preset, not both");
    }

    let resolved = if let Some(columns) = columns {
        parse_contact_columns(columns)?
    } else {
        match preset
            .map(|preset| preset.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("full") => full_contact_columns(),
            Some("address-book") | Some("address_book") => vec![
                ContactColumn::Email,
                ContactColumn::DisplayName,
                ContactColumn::Domain,
                ContactColumn::Kind,
            ],
            Some("ranked") => vec![
                ContactColumn::Email,
                ContactColumn::DisplayName,
                ContactColumn::Domain,
                ContactColumn::Kind,
                ContactColumn::Messages,
                ContactColumn::Conversations,
                ContactColumn::RankScore,
            ],
            Some(other) => {
                bail!("unknown CSV preset '{other}'. Expected one of: address-book, ranked, full")
            }
        }
    };

    if resolved.is_empty() {
        bail!("CSV export needs at least one column");
    }

    Ok(resolved)
}

fn parse_contact_columns(columns: &str) -> Result<Vec<ContactColumn>> {
    columns
        .split(',')
        .map(|column| parse_contact_column(column.trim()))
        .collect()
}

fn parse_contact_column(column: &str) -> Result<ContactColumn> {
    match column.trim().to_ascii_lowercase().as_str() {
        "email" => Ok(ContactColumn::Email),
        "display_name" | "display-name" | "name" => Ok(ContactColumn::DisplayName),
        "domain" => Ok(ContactColumn::Domain),
        "kind" | "sender_kind" | "sender-kind" => Ok(ContactColumn::Kind),
        "messages" | "message_count" | "message-count" => Ok(ContactColumn::Messages),
        "sent" | "sent_count" | "sent-count" => Ok(ContactColumn::Sent),
        "to" | "direct" | "direct_count" | "direct-count" => Ok(ContactColumn::To),
        "cc" | "cc_count" | "cc-count" => Ok(ContactColumn::Cc),
        "bcc" | "bcc_count" | "bcc-count" => Ok(ContactColumn::Bcc),
        "conversations" | "conversation_count" | "conversation-count" => {
            Ok(ContactColumn::Conversations)
        }
        "rank_score" | "rank-score" | "score" => Ok(ContactColumn::RankScore),
        "" => bail!("empty CSV column name"),
        other => bail!(
            "unknown CSV column '{other}'. Expected columns: email, display_name, domain, kind, messages, sent, to, cc, bcc, conversations, rank_score"
        ),
    }
}

fn full_contact_columns() -> Vec<ContactColumn> {
    vec![
        ContactColumn::Email,
        ContactColumn::DisplayName,
        ContactColumn::Domain,
        ContactColumn::Kind,
        ContactColumn::Messages,
        ContactColumn::Sent,
        ContactColumn::To,
        ContactColumn::Cc,
        ContactColumn::Bcc,
        ContactColumn::Conversations,
        ContactColumn::RankScore,
    ]
}

fn print_contacts_csv(contacts: &[ContactSummary], columns: &[ContactColumn]) {
    println!(
        "{}",
        columns
            .iter()
            .map(|column| column.canonical_name())
            .collect::<Vec<_>>()
            .join(",")
    );
    for contact in contacts {
        println!(
            "{}",
            columns
                .iter()
                .map(|column| csv_field(&column.value(contact)))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}
