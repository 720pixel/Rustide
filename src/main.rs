use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use rustide::{
    bencode,
    config::Config,
    create::{self, CreateOptions, Protocol},
    metadata::{self, EditOptions, ListMode},
    system::System,
};

#[derive(Parser)]
#[command(
    name = "rustide",
    version,
    about = "Fast torrent creation, inspection, and editing",
    long_about = "Create a torrent with: rustide <input> <output.torrent>\n\nAutomatic pieces range from 16 KiB to 16 MiB. Larger pieces require --piece-size."
)]
struct Cli {
    #[arg(long, global = true, value_name = "JSON")]
    config: Option<PathBuf>,
    #[arg(short = 'P', long, global = true)]
    profile: Option<String>,
    #[arg(short, long, global = true)]
    quiet: bool,
    #[arg(long)]
    checksum_algorithms: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Hash a file or directory and create a torrent")]
    Create(CreateArgs),
    #[command(about = "Change metadata without hashing the payload again")]
    Edit(EditArgs),
    #[command(about = "Inspect a torrent")]
    Info(InfoArgs),
    #[command(about = "Print a field or query a bencode path")]
    Show(ShowArgs),
    #[command(about = "Verify local files against a torrent")]
    Verify(VerifyArgs),
    #[command(about = "Generate a magnet URI")]
    Magnet(MagnetArgs),
    #[command(about = "Create a torrent's missing padding files")]
    Pad { torrent: PathBuf, target: PathBuf },
    #[command(about = "Show detected CPU, memory, and storage")]
    System {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Args)]
#[command(args_override_self = true)]
struct CreateArgs {
    input: PathBuf,
    #[arg(value_name = "OUTPUT.TORRENT", conflicts_with = "output")]
    destination: Option<PathBuf>,
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    #[arg(short = 'v', long, default_value = "v1")]
    protocol: Protocol,
    #[arg(short = 'l', long, default_value = "auto", value_name = "SIZE")]
    piece_size: String,
    #[arg(short = 't', long, default_value = "auto", value_name = "COUNT")]
    threads: String,
    #[arg(
        long,
        value_enum,
        default_value = "auto",
        help = "Select automatic, mapped, or sequential buffered reads"
    )]
    io: create::IoMode,
    #[arg(short = 'a', long, num_args = 1.., value_name = "URL")]
    announce: Vec<String>,
    #[arg(short = 'g', long, num_args = 1.., value_name = "GROUP")]
    announce_group: Vec<String>,
    #[arg(short = 'w', long, num_args = 1.., value_name = "URL")]
    web_seed: Vec<String>,
    #[arg(long, num_args = 1.., value_name = "URL")]
    http_seed: Vec<String>,
    #[arg(short = 'd', long, num_args = 1.., value_name = "HOST:PORT")]
    dht_node: Vec<String>,
    #[arg(short = 'p', long, num_args = 0..=1, default_missing_value = "true", value_parser = parse_bool)]
    private: Option<bool>,
    #[arg(short = 's', long)]
    source: Option<String>,
    #[arg(short = 'n', long)]
    name: Option<String>,
    #[arg(short = 'c', long)]
    comment: Option<String>,
    #[arg(long, conflicts_with = "no_created_by")]
    created_by: Option<String>,
    #[arg(long)]
    no_created_by: bool,
    #[arg(
        long,
        conflicts_with = "no_creation_date",
        value_name = "TIMESTAMP",
        allow_hyphen_values = true
    )]
    creation_date: Option<String>,
    #[arg(long)]
    no_creation_date: bool,
    #[arg(long, num_args = 1.., value_name = "REGEX")]
    include: Vec<String>,
    #[arg(long, num_args = 1.., value_name = "REGEX")]
    exclude: Vec<String>,
    #[arg(long)]
    include_hidden: bool,
    #[arg(long)]
    follow_symlinks: bool,
    #[arg(long, num_args = 1..)]
    checksum: Vec<String>,
    #[arg(long, num_args = 1..)]
    collection: Vec<String>,
    #[arg(long, num_args = 1.., value_name = "HASH_OR_TORRENT")]
    similar: Vec<String>,
    #[arg(long, conflicts_with = "no_cross_seed")]
    cross_seed: bool,
    #[arg(long)]
    no_cross_seed: bool,
    #[arg(long, alias = "overwrite")]
    force: bool,
    #[arg(long)]
    stdout: bool,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    simple_progress: bool,
}

#[derive(Args)]
#[command(args_override_self = true)]
struct EditArgs {
    torrent: PathBuf,
    #[arg(conflicts_with = "output")]
    destination: Option<PathBuf>,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(short = 'n', long)]
    name: Option<String>,
    #[arg(short = 's', long)]
    source: Option<String>,
    #[arg(short = 'p', long, num_args = 0..=1, default_missing_value = "true", value_parser = parse_bool)]
    private: Option<bool>,
    #[arg(short = 'c', long)]
    comment: Option<String>,
    #[arg(long)]
    created_by: Option<String>,
    #[arg(long)]
    no_created_by: bool,
    #[arg(long, allow_hyphen_values = true)]
    creation_date: Option<String>,
    #[arg(long)]
    no_creation_date: bool,
    #[arg(short = 'a', long, num_args = 0..)]
    announce: Option<Vec<String>>,
    #[arg(short = 'g', long, num_args = 1..)]
    announce_group: Vec<String>,
    #[arg(short = 'w', long, num_args = 0..)]
    web_seed: Option<Vec<String>>,
    #[arg(long, num_args = 0..)]
    http_seed: Option<Vec<String>>,
    #[arg(short = 'd', long, num_args = 0..)]
    dht_node: Option<Vec<String>>,
    #[arg(short = 'm', long, default_value = "replace", value_parser = ["replace", "append", "prepend"])]
    list_mode: String,
    #[arg(long, num_args = 0..)]
    collection: Option<Vec<String>>,
    #[arg(long, num_args = 0..)]
    similar: Option<Vec<String>>,
    #[arg(long, alias = "overwrite")]
    force: bool,
    #[arg(long)]
    stdout: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct InfoArgs {
    torrent: PathBuf,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    raw: bool,
    #[arg(long)]
    show_pieces: bool,
    #[arg(long)]
    show_padding_files: bool,
}

#[derive(Args)]
struct ShowArgs {
    field: String,
    torrent: PathBuf,
    pointer: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    flat: bool,
    #[arg(long)]
    tree: bool,
    #[arg(short = 'v', long)]
    protocol: Option<Protocol>,
    #[arg(short = 't', long)]
    truncate: bool,
    #[arg(short = 'H', long)]
    human_readable: bool,
}

#[derive(Args)]
struct VerifyArgs {
    torrent: PathBuf,
    input: PathBuf,
    #[arg(short = 't', long, default_value = "auto")]
    threads: String,
    #[arg(short = 'v', long)]
    protocol: Option<Protocol>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct MagnetArgs {
    torrent: PathBuf,
    #[arg(long)]
    no_trackers: bool,
    #[arg(long)]
    base32: bool,
}

fn parse_bool(text: &str) -> std::result::Result<bool, String> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        _ => Err("use on/off or true/false".into()),
    }
}

fn threads(text: &str) -> Result<Option<usize>> {
    if text.eq_ignore_ascii_case("auto") || text == "0" {
        return Ok(None);
    }
    let n: usize = text
        .parse()
        .context("threads must be auto or a positive number")?;
    ensure!(
        n > 0 && n <= 4096,
        "thread count must be between 1 and 4096"
    );
    Ok(Some(n))
}

fn timestamp(text: &str) -> Result<i64> {
    if let Ok(number) = text.parse() {
        return Ok(number);
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|date| date.timestamp())
        .context("date must be a Unix timestamp or ISO-8601 timestamp with timezone")
}

fn similar(values: &[String]) -> Result<Vec<Vec<u8>>> {
    values
        .iter()
        .map(|text| {
            if Path::new(text).is_file() {
                let doc = metadata::load(Path::new(text))?;
                let hashes = metadata::info_hashes(&doc)?;
                return hashes
                    .v1
                    .map(|h| h.to_vec())
                    .context("similar torrents need a v1 infohash");
            }
            let hash = hex::decode(text)
                .context("similar torrent needs a hexadecimal infohash or a torrent file")?;
            ensure!(hash.len() == 20, "similar infohash must contain 20 bytes");
            Ok(hash)
        })
        .collect()
}

fn normalize_args() -> Vec<OsString> {
    let mut args: Vec<_> = std::env::args_os().collect();
    let commands = [
        "create", "edit", "info", "show", "verify", "magnet", "pad", "system", "help",
    ];
    let mut index = 1;
    while let Some(value) = args.get(index).and_then(|s| s.to_str()) {
        if ["--config", "--profile", "-P"].contains(&value) {
            index += 2;
            continue;
        }
        if ["--quiet", "-q"].contains(&value)
            || value.starts_with("--config=")
            || value.starts_with("--profile=")
            || (value.starts_with("-P") && value.len() > 2)
        {
            index += 1;
            continue;
        }
        if commands.contains(&value) {
            let command = args.remove(index);
            args.insert(1, command);
        } else if !["--help", "-h", "--version", "-V", "--checksum-algorithms"].contains(&value) {
            args.insert(1, "create".into());
        }
        break;
    }
    args
}

fn binary_stdout(value: &bencode::Value) -> Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    bencode::encode_to_writer(value, &mut out)?;
    out.flush()?;
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(io::stdout().lock(), value)?;
    println!();
    Ok(())
}

fn selected_document(
    mut document: metadata::Document,
    wanted: Option<Protocol>,
) -> Result<metadata::Document> {
    let Some(wanted) = wanted else {
        return Ok(document);
    };
    let original = metadata::protocol(&document)?;
    ensure!(
        wanted == original || original == Protocol::Hybrid,
        "requested protocol is absent from this torrent"
    );
    if wanted == original {
        return Ok(document);
    }
    let info = document
        .value
        .get_mut(b"info")
        .and_then(bencode::Value::as_dict_mut)
        .context("invalid info")?;
    match wanted {
        Protocol::V1 => {
            info.remove(b"meta version".as_slice());
            info.remove(b"file tree".as_slice());
            document
                .value
                .as_dict_mut()
                .unwrap()
                .remove(b"piece layers".as_slice());
        }
        Protocol::V2 => {
            for key in [
                b"pieces".as_slice(),
                b"files".as_slice(),
                b"length".as_slice(),
            ] {
                info.remove(key);
            }
        }
        Protocol::Hybrid => {}
    }
    metadata::from_value(document.value)
}

fn run(cli: Cli) -> Result<()> {
    if cli.checksum_algorithms {
        println!("sha1\nsha256\nsha512\nmd5");
        return Ok(());
    }
    let config = Config::load(cli.config.as_deref())?;
    let Some(command) = cli.command else {
        Cli::parse_from(["rustide", "--help"]);
        return Ok(());
    };
    match command {
        Command::Create(args) => {
            let started = Instant::now();
            let output = args.output.or(args.destination).unwrap_or_else(|| {
                PathBuf::from(format!(
                    "{}.torrent",
                    args.name.as_deref().unwrap_or_else(|| args
                        .input
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("data"))
                ))
            });
            ensure!(
                !output.exists() || args.force || args.stdout || args.dry_run,
                "output exists: {} (use --force)",
                output.display()
            );
            if output.exists() {
                ensure!(
                    output.canonicalize()? != args.input.canonicalize()?,
                    "output cannot replace the input"
                );
            }
            let (announces, private, source) =
                config.announces(&args.announce, &args.announce_group)?;
            let options = CreateOptions {
                target: args.input,
                output: Some(output.clone()),
                protocol: args.protocol,
                piece_length: create::parse_piece_length(&args.piece_size)?,
                threads: threads(&args.threads)?,
                io: args.io,
                name: args.name,
                announces,
                private: args.private.or(private),
                source: args.source.or(source),
                web_seeds: args.web_seed,
                http_seeds: args.http_seed,
                dht_nodes: args.dht_node,
                comment: args.comment,
                created_by: args.created_by,
                creation_date: args.creation_date.as_deref().map(timestamp).transpose()?,
                no_created_by: args.no_created_by,
                no_creation_date: args.no_creation_date,
                include: args.include,
                exclude: args.exclude,
                include_hidden: args.include_hidden,
                follow_symlinks: args.follow_symlinks,
                checksums: args.checksum,
                collections: args.collection,
                similar: similar(&args.similar)?,
            };
            create::validate_options(&options)?;
            if args.dry_run {
                let files = create::scan(&options)?;
                let total = files.iter().try_fold(0u64, |n, f| {
                    n.checked_add(f.length).context("size overflow")
                })?;
                print_json(
                    &serde_json::json!({"files":files.len(),"payload_bytes":total,"piece_bytes":options.piece_length.unwrap_or_else(||create::automatic_piece_length(total)),"system":System::detect(&options.target)}),
                )?;
                return Ok(());
            }
            let mut result = create::create(&options)?;
            if args.cross_seed {
                let joined = options
                    .announces
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                result
                    .value
                    .get_mut(b"info")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert(
                        b"x_cross_seed".to_vec(),
                        bencode::Value::Bytes(
                            rustide::hash::sha1::hash(joined.as_bytes()).to_vec(),
                        ),
                    );
            }
            metadata::validate(&metadata::from_value(result.value.clone())?)?;
            if args.stdout {
                binary_stdout(&result.value)?;
            } else {
                metadata::write_atomic(&output, &result.value, args.force)?;
            }
            result.report.total_seconds = started.elapsed().as_secs_f64();
            if args.json && !args.stdout {
                print_json(&result.report)?;
            } else if !cli.quiet {
                eprintln!(
                    "{} files, {:.2} GiB, {} KiB pieces, {} workers ({})",
                    result.report.files,
                    result.report.payload_bytes as f64 / 1073741824.0,
                    result.report.piece_bytes / 1024,
                    result.report.workers,
                    result.report.system.hash_backend
                );
                eprintln!(
                    "Hashed in {:.3}s ({:.2} GiB/s)",
                    result.report.hash_seconds, result.report.gib_per_second
                );
                if !args.stdout {
                    eprintln!("Written to {}", output.display());
                }
            }
        }
        Command::Edit(args) => {
            let document = metadata::load(&args.torrent)?;
            let supplied_announces = args.announce.is_some() || !args.announce_group.is_empty();
            let (announces, _, _) = config.announces(
                args.announce.as_deref().unwrap_or(&[]),
                &args.announce_group,
            )?;
            let options = EditOptions {
                name: args.name,
                source: args.source,
                private: args.private,
                comment: args.comment,
                created_by: args.created_by,
                creation_date: args.creation_date.as_deref().map(timestamp).transpose()?,
                remove_created_by: args.no_created_by,
                remove_creation_date: args.no_creation_date,
                announces: supplied_announces.then_some(announces),
                web_seeds: args.web_seed,
                http_seeds: args.http_seed,
                dht_nodes: args.dht_node,
                list_mode: match args.list_mode.as_str() {
                    "append" => ListMode::Append,
                    "prepend" => ListMode::Prepend,
                    _ => ListMode::Replace,
                },
                collections: args.collection,
                similar: args.similar.as_deref().map(similar).transpose()?,
            };
            let value = metadata::edit(&document, &options)?;
            let output = args
                .output
                .or(args.destination)
                .unwrap_or_else(|| args.torrent.clone());
            let in_place = output == args.torrent
                || (output.exists() && output.canonicalize()? == args.torrent.canonicalize()?);
            if args.stdout {
                binary_stdout(&value)?;
            } else {
                metadata::write_atomic(&output, &value, args.force || in_place)?;
            }
            if args.json && !args.stdout {
                print_json(&metadata::inspect(&metadata::from_value(value)?)?)?;
            } else if !cli.quiet && !args.stdout {
                eprintln!("Written to {}", output.display());
            }
        }
        Command::Info(args) => {
            let document = metadata::load(&args.torrent)?;
            if args.raw || args.show_pieces {
                print_json(&metadata::to_json(&document.value, args.show_pieces))?;
            } else if args.json {
                print_json(&metadata::inspect(&document)?)?;
            } else {
                let summary = metadata::inspect(&document)?;
                for key in [
                    "name",
                    "protocol",
                    "size",
                    "piece_length",
                    "file_count",
                    "info_hash_v1",
                    "info_hash_v2",
                ] {
                    if let Some(value) = summary.get(key).filter(|v| !v.is_null()) {
                        println!(
                            "{key:15} {}",
                            value
                                .as_str()
                                .map_or_else(|| value.to_string(), terminal_text)
                        );
                    }
                }
                for file in metadata::files(&document)? {
                    if !file.padding || args.show_padding_files {
                        println!(
                            "{:12} {}",
                            file.length,
                            terminal_text(
                                &file
                                    .components
                                    .iter()
                                    .map(|c| String::from_utf8_lossy(c))
                                    .collect::<Vec<_>>()
                                    .join("/")
                            )
                        );
                    }
                }
            }
        }
        Command::Show(args) => show(args)?,
        Command::Verify(args) => {
            let document = selected_document(metadata::load(&args.torrent)?, args.protocol)?;
            let result = metadata::verify(&document, &args.input, threads(&args.threads)?)?;
            if args.json {
                print_json(&result)?;
            } else {
                println!(
                    "{}",
                    if result["valid"].as_bool() == Some(true) {
                        "All pieces match"
                    } else {
                        "Piece verification failed"
                    }
                );
            }
            ensure!(
                result["valid"].as_bool() == Some(true),
                "local files do not match the torrent"
            );
        }
        Command::Magnet(args) => println!(
            "{}",
            metadata::magnet(
                &metadata::load(&args.torrent)?,
                !args.no_trackers,
                args.base32
            )?
        ),
        Command::Pad { torrent, target } => {
            let count = metadata::pad(&metadata::load(&torrent)?, &target)?;
            println!("Created {count} padding files");
        }
        Command::System { path } => print_json(&System::detect(&path))?,
    }
    Ok(())
}

fn show(args: ShowArgs) -> Result<()> {
    let document = metadata::load(&args.torrent)?;
    let summary = metadata::inspect(&document)?;
    if args.field == "infohash" {
        let hashes = metadata::info_hashes(&document)?;
        match args.protocol {
            Some(Protocol::V1) => ensure!(hashes.v1.is_some(), "v1 infohash is absent"),
            Some(Protocol::V2) => ensure!(hashes.v2.is_some(), "v2 infohash is absent"),
            Some(Protocol::Hybrid) => ensure!(
                hashes.v1.is_some() && hashes.v2.is_some(),
                "hybrid infohashes are absent"
            ),
            None => {}
        }
        if args.protocol != Some(Protocol::V2)
            && let Some(hash) = hashes.v1
        {
            println!("{}", hex::encode(hash));
        }
        if args.protocol != Some(Protocol::V1)
            && let Some(hash) = hashes.v2
        {
            println!(
                "{}",
                hex::encode(if args.truncate {
                    &hash[..20]
                } else {
                    &hash[..]
                })
            );
        }
        return Ok(());
    }
    if args.field == "files" {
        let files = metadata::files(&document)?;
        if args.json {
            print_json(&summary["files"])?;
        } else {
            for file in files.iter().filter(|f| !f.padding) {
                println!(
                    "{}{}",
                    if args.tree { "  " } else { "" },
                    terminal_text(
                        &file
                            .components
                            .iter()
                            .map(|c| String::from_utf8_lossy(c))
                            .collect::<Vec<_>>()
                            .join("/")
                    )
                );
            }
        }
        return Ok(());
    }
    if args.field == "size" || args.field == "piece-size" {
        let key = if args.field == "size" {
            "size"
        } else {
            "piece_length"
        };
        let value = summary[key].as_u64().context("missing size")?;
        if args.human_readable {
            println!("{:.2} MiB", value as f64 / 1048576.0);
        } else {
            println!("{value}");
        }
        return Ok(());
    }
    if args.field == "piece-count" {
        let info = document.value.get(b"info").context("missing info")?;
        let count = info
            .get(b"pieces")
            .and_then(bencode::Value::as_bytes)
            .map_or_else(
                || {
                    summary["files"].as_array().map_or(0, |files| {
                        files
                            .iter()
                            .map(|f| {
                                f["length"]
                                    .as_u64()
                                    .unwrap_or(0)
                                    .div_ceil(summary["piece_length"].as_u64().unwrap_or(1))
                                    as usize
                            })
                            .sum()
                    })
                },
                |pieces| pieces.len() / 20,
            );
        println!("{count}");
        return Ok(());
    }
    if args.field == "announce-urls" {
        let tiers = document
            .value
            .get(b"announce-list")
            .and_then(bencode::Value::as_list);
        if let Some(tiers) = tiers {
            for tier in tiers {
                let urls = tier.as_list().context("invalid announce list")?;
                for url in urls {
                    print!(
                        "{}{}",
                        terminal_text(url.as_str().context("invalid announce URL")?),
                        if args.flat { "\n" } else { " " }
                    );
                }
                if !args.flat {
                    println!();
                }
            }
        } else if let Some(url) = document
            .value
            .get(b"announce")
            .and_then(bencode::Value::as_str)
        {
            println!("{}", terminal_text(url));
        }
        return Ok(());
    }
    let pointer = match args.field.as_str() {
        "query" => args.pointer.as_deref().context("query needs a /path")?,
        "name" => "/info/name",
        "private" => "/info/private",
        "source" => "/info/source",
        "comment" => "/comment",
        "created-by" => "/created by",
        "creation-date" => "/creation date",
        "dht-nodes" => "/nodes",
        "web-seeds" => "/url-list",
        "http-seeds" => "/httpseeds",
        "collections" => "/info/collections",
        "similar" => "/info/similar",
        "protocol" => {
            println!("{}", summary["protocol"].as_str().unwrap_or("unknown"));
            return Ok(());
        }
        _ => bail!("unknown field; use query to select a /path"),
    };
    let value = metadata::query(&document.value, pointer)?;
    if !args.json {
        if let Some(text) = value.as_str() {
            println!("{}", terminal_text(text));
            return Ok(());
        }
        if let Some(integer) = value.as_int() {
            println!("{integer}");
            return Ok(());
        }
    }
    print_json(&metadata::to_json(value, true))
}

fn main() {
    let normalized = normalize_args();
    let positional_output =
        Cli::try_parse_from(normalized.clone())
            .ok()
            .is_some_and(|cli| match cli.command {
                Some(Command::Create(args)) => args.destination.is_some(),
                Some(Command::Edit(args)) => args.destination.is_some(),
                _ => false,
            });
    let skipped: &[&str] = if positional_output { &["output"] } else { &[] };
    let result = rustide::config::expand_profile_except(normalized, skipped).and_then(|args| {
        let cli = match Cli::try_parse_from(args) {
            Ok(cli) => cli,
            Err(error) => error.exit(),
        };
        run(cli)
    });
    if let Err(error) = result {
        eprintln!("rustide: {}", terminal_text(&format!("{error:#}")));
        std::process::exit(1);
    }
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                character.escape_default().to_string()
            } else {
                character.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::terminal_text;

    #[test]
    fn terminal_output_escapes_controls_and_keeps_unicode_names() {
        assert_eq!(
            terminal_text("film\u{1b}[2J\n東京"),
            "film\\u{1b}[2J\\n東京"
        );
        assert_eq!(terminal_text("x\u{202e}txt"), "x\\u{202e}txt");
    }
}
