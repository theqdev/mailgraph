# Mailgraph

Mailgraph is a fast local CLI for building a private searchable graph of people,
conversations, domains, and email history from Outlook PST/OST archives.

The first version is metadata-only. It does not extract attachments and does not
store full email bodies.

## Planned Commands

```bash
mailgraph scan path/to/archive.pst --db mailgraph.sqlite
mailgraph contacts --db mailgraph.sqlite
mailgraph stats --db mailgraph.sqlite
```

## Current State

This repository contains the first working Rust implementation scaffold:

- `clap` CLI with `scan`, `contacts`, and `stats` commands.
- SQLite schema and migration bootstrap.
- Archive-reader trait so PST/OST support stays isolated from indexing logic.
- PST/OST scanning through the `pffexport` utility from the `libpff` toolchain.
- RFC822/EML scanning for exported messages and test fixtures.
- Contact normalization and sender classification helpers.
- Ranked contacts that score real conversations higher.
- Default filtering for no-reply, bulk, newsletter, promotional, and automated senders.
- Resumable scan bookkeeping and tolerant per-message ingest boundaries.

The PST/OST backend currently shells out to `pffexport` and parses the exported
message headers. A direct `libpff` FFI backend can be added later behind the same
`ArchiveReader` interface.

## Development Setup

Mailgraph is intended to be developed and run from WSL/Linux. For best
performance, keep large PST/OST files and generated SQLite databases inside the
WSL filesystem rather than under `/mnt/c`.

Install Rust:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Install native dependencies that will be useful once the `libpff` reader lands:

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libpff-dev pff-tools
```

Build and test:

```bash
cargo fmt
cargo test
```

Run the current CLI:

```bash
cargo run -- scan sample.pst
cargo run -- scan-pff-export sample.export
cargo run -- stats
cargo run -- contacts
cargo run -- contacts --include-filtered
cargo run -- contacts --all --csv
cargo run -- contacts --all --csv --preset address-book
cargo run -- contacts --all --csv --columns email,name,domain,score
cargo run -- contacts --kind automated
cargo run -- contacts --domain example.com
cargo run -- contacts --role cc
cargo run -- contacts --role cc --all --csv --preset address-book > cc-contacts.csv
```

When `scan` is run without `--db`, Mailgraph creates a database named after the
input, such as `sample.mailgraph.sqlite`, and stores that path in
`.mailgraph/current-db`. Later commands reuse that remembered database. Pass
`--db path/to/file.sqlite` to override it.

## Architecture

- `archive`: PST/OST and RFC822 reader abstraction.
- `cli`: command-line parsing and command dispatch.
- `db`: SQLite schema, migrations, and query helpers.
- `normalize`: email/domain normalization.
- `classify`: sender classification for no-reply, bulk, newsletter,
  promotional, and automated senders.
- `scanner`: scan orchestration and message ingestion.

## License

MIT
