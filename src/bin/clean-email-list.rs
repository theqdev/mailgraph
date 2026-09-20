use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use mailgraph::normalize::normalize_email;

#[derive(Debug, Parser)]
#[command(name = "clean-email-list")]
#[command(about = "Clean and de-duplicate CSV email lists for safer exports")]
struct Args {
    /// Input CSV file. Reads stdin when omitted.
    #[arg(long)]
    input: Option<PathBuf>,

    /// Output CSV file. Writes stdout when omitted.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Email column name, 1-based index, or 0-based index. Auto-detects common names when omitted.
    #[arg(long)]
    email_column: Option<String>,

    /// Name column name, 1-based index, or 0-based index. Auto-detects common names when omitted.
    #[arg(long)]
    name_column: Option<String>,

    /// Output only email and name columns.
    #[arg(long, default_value_t = true)]
    email_name_only: bool,

    /// Disable built-in Mailgraph cleanup rules.
    #[arg(long)]
    no_defaults: bool,

    /// Exclude domains containing any of these comma-separated terms. Can be repeated.
    #[arg(
        long,
        env = "MAILGRAPH_CLEAN_EXCLUDE_DOMAIN_CONTAINS",
        value_delimiter = ','
    )]
    exclude_domain_contains: Vec<String>,

    /// Exclude emails containing any of these comma-separated terms. Can be repeated.
    #[arg(
        long,
        env = "MAILGRAPH_CLEAN_EXCLUDE_EMAIL_CONTAINS",
        value_delimiter = ','
    )]
    exclude_email_contains: Vec<String>,

    /// Exclude an exact email address. Can be repeated.
    #[arg(long, env = "MAILGRAPH_CLEAN_EXCLUDE_EMAILS", value_delimiter = ',')]
    exclude_email: Vec<String>,

    /// Keep free-mail providers such as gmail.com, yahoo.com, outlook.com.
    #[arg(long)]
    keep_free_mail: bool,

    /// Drop role inboxes such as support@, info@, admin@.
    #[arg(long)]
    drop_role_accounts: bool,

    /// Print dropped-row reasons to stderr.
    #[arg(long)]
    explain: bool,
}

#[derive(Debug)]
struct Filters {
    domain_contains: Vec<String>,
    email_contains: Vec<String>,
    exact_emails: HashSet<String>,
}

#[derive(Debug, Default)]
struct Summary {
    input_rows: usize,
    kept_rows: usize,
    duplicate_rows: usize,
    invalid_rows: usize,
    filtered_rows: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let rows = read_rows(args.input.as_ref())?;
    if rows.is_empty() {
        bail!("input is empty");
    }

    let header = rows[0].clone();
    let email_idx = resolve_column(
        &header,
        args.email_column.as_deref(),
        &[
            "email",
            "e-mail",
            "email_address",
            "email address",
            "address",
        ],
    )?;
    let name_idx = resolve_optional_column(
        &header,
        args.name_column.as_deref(),
        &[
            "name",
            "display_name",
            "display name",
            "full_name",
            "full name",
        ],
    )?;
    let filters = build_filters(&args);
    let mut seen = HashSet::new();
    let mut summary = Summary::default();
    let mut output_rows = Vec::new();

    if args.email_name_only {
        output_rows.push(vec!["email".to_string(), "name".to_string()]);
    } else {
        output_rows.push(header);
    }

    for (line_no, row) in rows.into_iter().enumerate().skip(1) {
        summary.input_rows += 1;

        let raw_email = row.get(email_idx).map(String::as_str).unwrap_or_default();
        let Some(normalized) = normalize_email(raw_email) else {
            summary.invalid_rows += 1;
            explain(&args, line_no + 1, raw_email, "invalid email");
            continue;
        };

        if let Some(reason) = filter_reason(&normalized.address, &normalized.domain, &filters) {
            summary.filtered_rows += 1;
            explain(&args, line_no + 1, &normalized.address, reason);
            continue;
        }

        if !seen.insert(normalized.address.clone()) {
            summary.duplicate_rows += 1;
            explain(&args, line_no + 1, &normalized.address, "duplicate email");
            continue;
        }

        summary.kept_rows += 1;
        if args.email_name_only {
            output_rows.push(vec![
                normalized.address,
                name_idx
                    .and_then(|idx| row.get(idx))
                    .map(|value| value.trim().to_string())
                    .unwrap_or_default(),
            ]);
        } else {
            let mut row = row;
            row[email_idx] = normalized.address;
            output_rows.push(row);
        }
    }

    write_rows(args.output.as_ref(), &output_rows)?;
    eprintln!(
        "Cleaned list: {} input, {} kept, {} filtered, {} invalid, {} duplicates",
        summary.input_rows,
        summary.kept_rows,
        summary.filtered_rows,
        summary.invalid_rows,
        summary.duplicate_rows
    );

    Ok(())
}

fn read_rows(path: Option<&PathBuf>) -> Result<Vec<Vec<String>>> {
    let reader: Box<dyn BufRead> = match path {
        Some(path) => Box::new(BufReader::new(
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
        )),
        None => Box::new(BufReader::new(io::stdin())),
    };

    let mut rows = Vec::new();
    for line in reader.lines() {
        rows.push(parse_csv_line(&line?)?);
    }
    Ok(rows)
}

fn write_rows(path: Option<&PathBuf>, rows: &[Vec<String>]) -> Result<()> {
    let mut writer: Box<dyn Write> = match path {
        Some(path) => Box::new(
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?,
        ),
        None => Box::new(io::stdout()),
    };

    for row in rows {
        writeln!(
            writer,
            "{}",
            row.iter()
                .map(|value| csv_field(value))
                .collect::<Vec<_>>()
                .join(",")
        )?;
    }
    Ok(())
}

fn resolve_column(
    header: &[String],
    requested: Option<&str>,
    candidates: &[&str],
) -> Result<usize> {
    if let Some(requested) = requested {
        return parse_column_ref(header, requested);
    }

    candidates
        .iter()
        .find_map(|candidate| {
            header
                .iter()
                .position(|column| column.trim().eq_ignore_ascii_case(candidate))
        })
        .with_context(|| {
            format!(
                "could not auto-detect column. Tried: {}. Pass --email-column or --name-column",
                candidates.join(", ")
            )
        })
}

fn resolve_optional_column(
    header: &[String],
    requested: Option<&str>,
    candidates: &[&str],
) -> Result<Option<usize>> {
    if let Some(requested) = requested {
        return parse_column_ref(header, requested).map(Some);
    }

    Ok(candidates.iter().find_map(|candidate| {
        header
            .iter()
            .position(|column| column.trim().eq_ignore_ascii_case(candidate))
    }))
}

fn parse_column_ref(header: &[String], value: &str) -> Result<usize> {
    if let Ok(index) = value.parse::<usize>() {
        let index = if index == 0 { 0 } else { index - 1 };
        if index < header.len() {
            return Ok(index);
        }
        bail!("column index {} is out of range", value);
    }

    header
        .iter()
        .position(|column| column.trim().eq_ignore_ascii_case(value.trim()))
        .with_context(|| format!("column '{value}' not found"))
}

fn build_filters(args: &Args) -> Filters {
    let mut domain_contains = Vec::new();
    let mut email_contains = Vec::new();
    let mut exact_emails = HashSet::new();

    if !args.no_defaults {
        email_contains.extend(split_terms(DEFAULT_EMAIL_CONTAINS));
        if !args.keep_free_mail {
            domain_contains.extend(split_terms(FREE_MAIL_DOMAINS));
        }
    }

    if args.drop_role_accounts {
        email_contains.extend(split_terms(ROLE_ACCOUNT_PREFIXES));
    }

    domain_contains.extend(split_values(&args.exclude_domain_contains));
    email_contains.extend(split_values(&args.exclude_email_contains));
    exact_emails.extend(
        split_values(&args.exclude_email)
            .into_iter()
            .filter_map(|email| normalize_email(&email).map(|normalized| normalized.address)),
    );

    domain_contains.sort();
    domain_contains.dedup();
    email_contains.sort();
    email_contains.dedup();

    Filters {
        domain_contains,
        email_contains,
        exact_emails,
    }
}

fn filter_reason<'a>(email: &str, domain: &str, filters: &'a Filters) -> Option<&'a str> {
    if filters.exact_emails.contains(email) {
        return Some("exact email excluded");
    }

    for term in &filters.domain_contains {
        if domain.contains(term) {
            return Some("domain pattern excluded");
        }
    }

    for term in &filters.email_contains {
        if email.contains(term) {
            return Some("email pattern excluded");
        }
    }

    None
}

fn split_terms(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect()
}

fn split_values(values: &[String]) -> Vec<String> {
    values.iter().flat_map(|value| split_terms(value)).collect()
}

fn explain(args: &Args, line_no: usize, email: &str, reason: &str) {
    if args.explain {
        eprintln!("line {line_no}: dropped {email} ({reason})");
    }
}

fn parse_csv_line(line: &str) -> Result<Vec<String>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(field);
                field = String::new();
            }
            _ => field.push(ch),
        }
    }

    if in_quotes {
        bail!("unterminated quoted CSV field");
    }

    fields.push(field);
    Ok(fields)
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

const DEFAULT_EMAIL_CONTAINS: &str = "\
noreply,no-reply,no_reply,do-not-reply,do_not_reply,donotreply,postmaster,mailer-daemon,\
noresponse,no-replay";

const ROLE_ACCOUNT_PREFIXES: &str = "\
support@,info@,admin@,administrator@,contact@,hello@,webmaster@,sales@,office@,feedback@,\
accounts@,account@,customer@,team@,mail@,help@,security@,reply@,service@,server@,staff@,\
domainsupport@";

const FREE_MAIL_DOMAINS: &str = "\
gmail.com,hotmail.com,yahoo.com,outlook.com,icloud.com,aol.com,live.com,me.com,msn.com,\
googlemail.com,proton.me,protonmail.com,mail.ru,gmx.com,gmx.de,t-online.de";
