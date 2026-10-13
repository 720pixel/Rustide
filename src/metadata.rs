use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value as Json, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::{
    bencode::{self, Value},
    create::Protocol,
    merkle,
};

type Dict = BTreeMap<Vec<u8>, Value>;

#[derive(Clone, Debug)]
pub struct Document {
    pub value: Value,
    pub raw_info: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentFile {
    pub components: Vec<Vec<u8>>,
    pub length: u64,
    pub padding: bool,
    pub pieces_root: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoHashes {
    pub v1: Option<[u8; 20]>,
    pub v2: Option<[u8; 32]>,
}

pub fn load(path: &Path) -> Result<Document> {
    let limit = bencode::Limits::default().max_bytes;
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    let (value, range) = bencode::parse_with_info_range(&bytes)?;
    let range = range.context("metainfo has no info dictionary")?;
    let document = Document {
        value,
        raw_info: bytes[range].to_vec(),
    };
    validate(&document)?;
    Ok(document)
}

pub fn from_value(value: Value) -> Result<Document> {
    let raw_info = bencode::encode(
        value
            .get(b"info")
            .context("metainfo has no info dictionary")?,
    );
    let document = Document { value, raw_info };
    validate(&document)?;
    Ok(document)
}

pub fn write_atomic(path: &Path, value: &Value, overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("cannot create temporary file in {}", parent.display()))?;
    bencode::encode_to_writer(value, temporary.as_file_mut())?;
    temporary.as_file_mut().flush()?;
    temporary.as_file().sync_all()?;
    if overwrite {
        temporary.persist(path).map_err(|e| e.error)?;
    } else {
        temporary
            .persist_noclobber(path)
            .map_err(|e| e.error)
            .with_context(|| {
                format!(
                    "cannot create {} (use overwrite to replace an existing file)",
                    path.display()
                )
            })?;
    }
    Ok(())
}

fn info(document: &Document) -> Result<&Dict> {
    document
        .value
        .as_dict()
        .context("metainfo must be a dictionary")?;
    document
        .value
        .get(b"info")
        .and_then(Value::as_dict)
        .context("info must be a dictionary")
}

fn required<'a>(dict: &'a Dict, key: &[u8]) -> Result<&'a Value> {
    dict.get(key)
        .with_context(|| format!("missing {}", String::from_utf8_lossy(key)))
}

fn unsigned(value: &Value, label: &str) -> Result<u64> {
    let number = value
        .as_int()
        .with_context(|| format!("{label} must be an integer"))?;
    u64::try_from(number).with_context(|| format!("{label} must not be negative"))
}

fn piece_length(document: &Document) -> Result<usize> {
    let size = unsigned(required(info(document)?, b"piece length")?, "piece length")?;
    ensure!(size > 0, "piece length must be positive");
    usize::try_from(size).context("piece length exceeds this platform's address space")
}

pub fn protocol(document: &Document) -> Result<Protocol> {
    let info = info(document)?;
    let v2 = match info.get(b"meta version" as &[u8]) {
        None => false,
        Some(value) => {
            ensure!(
                value.as_int() == Some(2),
                "unsupported meta version; expected 2"
            );
            true
        }
    };
    match (info.contains_key(b"pieces" as &[u8]), v2) {
        (true, false) => Ok(Protocol::V1),
        (false, true) => Ok(Protocol::V2),
        (true, true) => Ok(Protocol::Hybrid),
        (false, false) => bail!("info has neither v1 pieces nor a supported meta version"),
    }
}

fn safe_component(component: &[u8]) -> Result<()> {
    ensure!(
        !component.is_empty() && component != b"." && component != b"..",
        "empty or unsafe path component"
    );
    ensure!(
        !component.iter().any(|c| matches!(c, 0 | b'/' | b'\\')),
        "path component contains a separator or NUL"
    );
    // A drive prefix can turn a relative path into an absolute Windows path.
    ensure!(
        !component.contains(&b':'),
        "path component contains a drive separator"
    );
    Ok(())
}

fn v1_files(document: &Document) -> Result<Vec<TorrentFile>> {
    let info = info(document)?;
    if let Some(list) = info.get(b"files" as &[u8]) {
        ensure!(
            !info.contains_key(b"length" as &[u8]),
            "info cannot contain both length and files"
        );
        let list = list.as_list().context("files must be a list")?;
        ensure!(!list.is_empty(), "files must not be empty");
        list.iter()
            .map(|entry| {
                let entry = entry.as_dict().context("file entry must be a dictionary")?;
                let path = required(entry, b"path")?
                    .as_list()
                    .context("file path must be a list")?;
                ensure!(
                    !path.is_empty(),
                    "file path must have at least one component"
                );
                let components = path
                    .iter()
                    .map(|part| {
                        let part = part
                            .as_bytes()
                            .context("file path component must be a byte string")?;
                        safe_component(part)?;
                        Ok(part.to_vec())
                    })
                    .collect::<Result<Vec<_>>>()?;
                let padding = entry
                    .get(b"attr" as &[u8])
                    .and_then(Value::as_bytes)
                    .is_some_and(|a| a.contains(&b'p'));
                Ok(TorrentFile {
                    components,
                    length: unsigned(required(entry, b"length")?, "file length")?,
                    padding,
                    pieces_root: None,
                })
            })
            .collect()
    } else {
        let name = required(info, b"name")?
            .as_bytes()
            .context("name must be a byte string")?;
        safe_component(name)?;
        Ok(vec![TorrentFile {
            components: vec![name.to_vec()],
            length: unsigned(required(info, b"length")?, "file length")?,
            padding: false,
            pieces_root: None,
        }])
    }
}

fn v2_files(document: &Document) -> Result<Vec<TorrentFile>> {
    fn walk(node: &Dict, path: &mut Vec<Vec<u8>>, output: &mut Vec<TorrentFile>) -> Result<()> {
        if let Some(properties) = node.get(b"" as &[u8]) {
            ensure!(
                !path.is_empty() && node.len() == 1,
                "file tree file must have a path and no sibling entries"
            );
            let properties = properties
                .as_dict()
                .context("file properties must be a dictionary")?;
            let length = unsigned(required(properties, b"length")?, "file length")?;
            let pieces_root = properties
                .get(b"pieces root" as &[u8])
                .map(|v| {
                    let bytes = v.as_bytes().context("pieces root must be a byte string")?;
                    <[u8; 32]>::try_from(bytes).context("pieces root must be 32 bytes")
                })
                .transpose()?;
            ensure!(
                pieces_root.is_some() == (length > 0),
                "nonempty files need a pieces root; empty files must omit it"
            );
            let padding = properties
                .get(b"attr" as &[u8])
                .and_then(Value::as_bytes)
                .is_some_and(|a| a.contains(&b'p'));
            output.push(TorrentFile {
                components: path.clone(),
                length,
                padding,
                pieces_root,
            });
        } else {
            ensure!(!node.is_empty(), "file tree contains an empty directory");
            for (component, child) in node {
                safe_component(component)?;
                path.push(component.clone());
                walk(
                    child
                        .as_dict()
                        .context("file tree child must be a dictionary")?,
                    path,
                    output,
                )?;
                path.pop();
            }
        }
        Ok(())
    }
    let tree = required(info(document)?, b"file tree")?
        .as_dict()
        .context("file tree must be a dictionary")?;
    let mut output = Vec::new();
    walk(tree, &mut Vec::new(), &mut output)?;
    Ok(output)
}

pub fn files(document: &Document) -> Result<Vec<TorrentFile>> {
    match protocol(document)? {
        Protocol::V1 => v1_files(document),
        Protocol::V2 => v2_files(document),
        Protocol::Hybrid => {
            let mut files = v1_files(document)?;
            let roots: BTreeMap<_, _> = v2_files(document)?
                .into_iter()
                .map(|f| (f.components, f.pieces_root))
                .collect();
            for file in &mut files {
                file.pieces_root = roots.get(&file.components).copied().flatten();
            }
            Ok(files)
        }
    }
}

fn validate_paths(files: &[TorrentFile]) -> Result<()> {
    let mut paths = BTreeSet::new();
    for file in files {
        ensure!(paths.insert(file.components.clone()), "duplicate file path");
    }
    for path in &paths {
        for length in 1..path.len() {
            ensure!(
                !paths.contains(&path[..length]),
                "file path conflicts with a directory"
            );
        }
    }
    Ok(())
}

pub fn validate(document: &Document) -> Result<()> {
    let protocol = protocol(document)?;
    let info = info(document)?;
    safe_component(
        required(info, b"name")?
            .as_bytes()
            .context("name must be a byte string")?,
    )?;
    if let Some(private) = info.get(b"private" as &[u8]) {
        ensure!(
            matches!(private.as_int(), Some(0 | 1)),
            "private must be 0 or 1"
        );
    }
    let piece_length = piece_length(document)?;
    if matches!(protocol, Protocol::V1 | Protocol::Hybrid) {
        let files = v1_files(document)?;
        validate_paths(&files)?;
        let size = files.iter().try_fold(0u64, |total, f| {
            total.checked_add(f.length).context("total length overflow")
        })?;
        let pieces = required(info, b"pieces")?
            .as_bytes()
            .context("pieces must be a byte string")?;
        ensure!(
            pieces.len() % 20 == 0,
            "v1 pieces length must be a multiple of 20"
        );
        ensure!(
            pieces.len() as u64 / 20 == size.div_ceil(piece_length as u64),
            "v1 piece count does not match file lengths"
        );
    }
    if matches!(protocol, Protocol::V2 | Protocol::Hybrid) {
        ensure!(
            piece_length >= merkle::BLOCK_LENGTH && piece_length.is_power_of_two(),
            "v2 piece length must be a power of two of at least 16 KiB"
        );
        let v2 = v2_files(document)?;
        validate_paths(&v2)?;
        let layers = document
            .value
            .get(b"piece layers")
            .and_then(Value::as_dict)
            .context("v2 metainfo needs a piece layers dictionary")?;
        let mut expected = BTreeSet::new();
        for file in &v2 {
            if file.length <= piece_length as u64 {
                continue;
            }
            let root = file
                .pieces_root
                .context("nonempty file has no pieces root")?;
            let layer = layers
                .get(root.as_slice())
                .and_then(Value::as_bytes)
                .context("missing piece layer for file root")?;
            ensure!(
                layer.len() % 32 == 0
                    && layer.len() as u64 / 32 == file.length.div_ceil(piece_length as u64),
                "piece layer length does not match file length"
            );
            if expected.insert(root.to_vec()) {
                ensure!(
                    merkle::combine_root(
                        layer.as_chunks::<32>().0,
                        (piece_length / merkle::BLOCK_LENGTH).trailing_zeros() as usize
                    ) == root,
                    "piece layer does not match file root"
                );
            }
        }
        ensure!(
            layers.keys().all(|key| expected.contains(key)),
            "piece layers contains an unreferenced root"
        );
        if matches!(protocol, Protocol::Hybrid) {
            let v1 = v1_files(document)?;
            let actual: Vec<_> = v1
                .iter()
                .filter(|f| !f.padding)
                .map(|f| (&f.components, f.length))
                .collect();
            let expected: Vec<_> = v2
                .iter()
                .filter(|f| !f.padding)
                .map(|f| (&f.components, f.length))
                .collect();
            ensure!(
                actual == expected,
                "hybrid v1 and v2 file paths, lengths, or order differ"
            );
            let mut offset = 0u64;
            for file in &v1 {
                if !file.padding && file.length > 0 {
                    ensure!(
                        offset.is_multiple_of(piece_length as u64),
                        "hybrid file does not start on a piece boundary"
                    );
                }
                offset = offset
                    .checked_add(file.length)
                    .context("total length overflow")?;
            }
        }
    }
    Ok(())
}

pub fn info_hashes(document: &Document) -> Result<InfoHashes> {
    let protocol = protocol(document)?;
    Ok(InfoHashes {
        v1: matches!(protocol, Protocol::V1 | Protocol::Hybrid)
            .then(|| Sha1::digest(&document.raw_info).into()),
        v2: matches!(protocol, Protocol::V2 | Protocol::Hybrid)
            .then(|| Sha256::digest(&document.raw_info).into()),
    })
}

pub fn to_json(value: &Value, show_binary: bool) -> Json {
    match value {
        Value::Int(number) => json!(number),
        Value::Bytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(text)
                if !text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
            {
                json!(text)
            }
            _ if show_binary => json!({"$bytes": hex::encode(bytes)}),
            _ => json!({"$bytes_length": bytes.len()}),
        },
        Value::List(list) => Json::Array(list.iter().map(|v| to_json(v, show_binary)).collect()),
        Value::Dict(dict) => {
            if dict.keys().all(|key| std::str::from_utf8(key).is_ok()) {
                Json::Object(
                    dict.iter()
                        .map(|(key, value)| {
                            (
                                String::from_utf8(key.clone()).unwrap(),
                                to_json(value, show_binary),
                            )
                        })
                        .collect(),
                )
            } else {
                json!({"$dict": dict.iter().map(|(key, value)| json!({"key":hex::encode(key), "value":to_json(value, show_binary)})).collect::<Vec<_>>()})
            }
        }
    }
}

fn announce_tiers(document: &Document) -> Vec<Vec<String>> {
    let tiers = document
        .value
        .get(b"announce-list")
        .and_then(Value::as_list)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_list)
                .map(|tier| {
                    tier.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|tier| !tier.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if tiers.is_empty() {
        document
            .value
            .get(b"announce")
            .and_then(Value::as_str)
            .map(|url| vec![vec![url.to_owned()]])
            .unwrap_or_default()
    } else {
        tiers
    }
}

pub fn inspect(document: &Document) -> Result<Json> {
    let info = info(document)?;
    let files = files(document)?;
    let hashes = info_hashes(document)?;
    let bytes = files.iter().filter(|f| !f.padding).try_fold(0u64, |n, f| {
        n.checked_add(f.length).context("payload length overflow")
    })?;
    let padding = files.iter().filter(|f| f.padding).try_fold(0u64, |n, f| {
        n.checked_add(f.length).context("padding length overflow")
    })?;
    let kind = match protocol(document)? {
        Protocol::V1 => "v1",
        Protocol::V2 => "v2",
        Protocol::Hybrid => "hybrid",
    };
    Ok(json!({
        "name": String::from_utf8_lossy(required(info, b"name")?.as_bytes().context("invalid name")?),
        "protocol": kind,
        "info_hash_v1": hashes.v1.map(hex::encode), "info_hash_v2": hashes.v2.map(hex::encode),
        "piece_length": piece_length(document)?, "size":bytes, "padding_size":padding,
        "file_count":files.iter().filter(|f| !f.padding).count(),
        "private":info.get(b"private" as &[u8]).and_then(Value::as_int) == Some(1),
        "announce": announce_tiers(document),
        "files": files.iter().map(|f| json!({
            "path":f.components.iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join("/"),
            "length":f.length,"padding":f.padding,"pieces_root":f.pieces_root.map(hex::encode)
        })).collect::<Vec<_>>(),
        "metadata":to_json(&document.value, false)
    }))
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut result = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            result.push(*byte as char);
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").unwrap();
        }
    }
    result
}

fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut output = String::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | *byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            output.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        output.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    output
}

pub fn magnet(document: &Document, include_trackers: bool, use_base32: bool) -> Result<String> {
    let hashes = info_hashes(document)?;
    let mut parts = Vec::new();
    if let Some(v1) = hashes.v1 {
        parts.push(format!(
            "xt=urn:btih:{}",
            if use_base32 {
                base32(&v1)
            } else {
                hex::encode(v1)
            }
        ));
    }
    if let Some(v2) = hashes.v2 {
        parts.push(format!("xt=urn:btmh:1220{}", hex::encode(v2)));
    }
    parts.push(format!(
        "dn={}",
        percent_encode(
            required(info(document)?, b"name")?
                .as_bytes()
                .context("invalid name")?
        )
    ));
    if include_trackers {
        let mut seen = BTreeSet::new();
        for url in announce_tiers(document).into_iter().flatten() {
            if seen.insert(url.clone()) {
                parts.push(format!("tr={}", percent_encode(url.as_bytes())));
            }
        }
    }
    Ok(format!("magnet:?{}", parts.join("&")))
}

pub fn query<'a>(value: &'a Value, pointer: &str) -> Result<&'a Value> {
    if pointer.is_empty() {
        return Ok(value);
    }
    ensure!(
        pointer.starts_with('/'),
        "query must be empty or start with /"
    );
    let mut current = value;
    for token in pointer[1..].split('/') {
        let mut decoded = String::new();
        let mut chars = token.chars();
        while let Some(c) = chars.next() {
            if c == '~' {
                decoded.push(match chars.next() {
                    Some('0') => '~',
                    Some('1') => '/',
                    _ => bail!("invalid JSON pointer escape"),
                });
            } else {
                decoded.push(c);
            }
        }
        current = match current {
            Value::Dict(dict) => {
                let key = if let Some(hex) = decoded.strip_prefix("@hex:") {
                    hex::decode(hex).context("invalid hexadecimal dictionary key")?
                } else {
                    decoded.into_bytes()
                };
                dict.get(&key).context("query dictionary key not found")?
            }
            Value::List(list) => {
                ensure!(
                    !decoded.is_empty()
                        && decoded.bytes().all(|b| b.is_ascii_digit())
                        && (decoded.len() == 1 || !decoded.starts_with('0')),
                    "invalid list index"
                );
                list.get(decoded.parse::<usize>().context("list index overflow")?)
                    .context("list index out of bounds")?
            }
            _ => bail!("query traverses a scalar value"),
        };
    }
    Ok(current)
}

#[derive(Clone, Copy, Debug, Default)]
pub enum ListMode {
    #[default]
    Replace,
    Append,
    Prepend,
}

#[derive(Clone, Debug, Default)]
pub struct EditOptions {
    pub name: Option<String>,
    pub source: Option<String>,
    pub private: Option<bool>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
    pub remove_creation_date: bool,
    pub remove_created_by: bool,
    pub announces: Option<Vec<Vec<String>>>,
    pub web_seeds: Option<Vec<String>>,
    pub http_seeds: Option<Vec<String>>,
    pub dht_nodes: Option<Vec<String>>,
    pub list_mode: ListMode,
    pub collections: Option<Vec<String>>,
    pub similar: Option<Vec<Vec<u8>>>,
}

fn merge_list(old: Vec<Value>, new: Vec<Value>, mode: ListMode) -> Vec<Value> {
    let list = match mode {
        ListMode::Replace => new,
        ListMode::Append => old.into_iter().chain(new).collect(),
        ListMode::Prepend => new.into_iter().chain(old).collect(),
    };
    let mut unique = Vec::new();
    for item in list {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    unique
}

fn replace_list(dict: &mut Dict, key: &[u8], items: Vec<Value>, mode: ListMode) {
    let old = match dict.get(key) {
        Some(Value::List(list)) => list.clone(),
        Some(Value::Bytes(bytes)) => vec![Value::Bytes(bytes.clone())],
        _ => Vec::new(),
    };
    let result = merge_list(old, items, mode);
    if result.is_empty() {
        dict.remove(key);
    } else {
        dict.insert(key.to_vec(), Value::List(result));
    }
}

fn string_values(values: &[String]) -> Vec<Value> {
    values
        .iter()
        .map(|v| Value::Bytes(v.as_bytes().to_vec()))
        .collect()
}

pub fn edit(document: &Document, options: &EditOptions) -> Result<Value> {
    let mut value = document.value.clone();
    let root = value
        .as_dict_mut()
        .context("metainfo must be a dictionary")?;
    for (key, text) in [
        (b"comment" as &[u8], &options.comment),
        (b"created by", &options.created_by),
    ] {
        if let Some(text) = text {
            root.insert(key.to_vec(), Value::Bytes(text.as_bytes().to_vec()));
        }
    }
    if options.remove_created_by {
        root.remove(b"created by" as &[u8]);
    }
    if let Some(date) = options.creation_date {
        root.insert(b"creation date".to_vec(), Value::Int(date));
    }
    if options.remove_creation_date {
        root.remove(b"creation date" as &[u8]);
    }
    if let Some(announces) = &options.announces {
        ensure!(
            announces
                .iter()
                .all(|tier| !tier.is_empty() && tier.iter().all(|url| !url.trim().is_empty())),
            "tracker tiers must contain nonempty URLs"
        );
        let previous = announce_tiers(document)
            .into_iter()
            .map(|tier| Value::List(string_values(&tier)))
            .collect();
        let next = announces
            .iter()
            .map(|tier| Value::List(string_values(tier)))
            .collect();
        let tiers = merge_list(previous, next, options.list_mode);
        if tiers.is_empty() {
            root.remove(b"announce" as &[u8]);
            root.remove(b"announce-list" as &[u8]);
        } else {
            let first = tiers[0].as_list().unwrap()[0].clone();
            root.insert(b"announce".to_vec(), first);
            root.insert(b"announce-list".to_vec(), Value::List(tiers));
        }
    }
    for (key, strings) in [
        (b"url-list" as &[u8], &options.web_seeds),
        (b"httpseeds", &options.http_seeds),
    ] {
        if let Some(strings) = strings {
            replace_list(root, key, string_values(strings), options.list_mode);
        }
    }
    if let Some(nodes) = &options.dht_nodes {
        let nodes = nodes
            .iter()
            .map(|node| {
                let (host, port) = node
                    .rsplit_once(':')
                    .context("DHT node must be host:port")?;
                let host = host
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(host);
                ensure!(!host.is_empty(), "DHT node host must not be empty");
                let port: u16 = port.parse().context("invalid DHT node port")?;
                ensure!(port != 0, "DHT node port must be positive");
                Ok(Value::List(vec![
                    Value::Bytes(host.as_bytes().to_vec()),
                    Value::Int(port as i64),
                ]))
            })
            .collect::<Result<Vec<_>>>()?;
        replace_list(root, b"nodes", nodes, options.list_mode);
    }
    let info = root
        .get_mut(b"info" as &[u8])
        .and_then(Value::as_dict_mut)
        .context("info must be a dictionary")?;
    if let Some(name) = &options.name {
        safe_component(name.as_bytes())?;
        if info.contains_key(b"length" as &[u8]) && info.contains_key(b"meta version" as &[u8]) {
            let old_name = required(info, b"name")?
                .as_bytes()
                .context("invalid name")?
                .to_vec();
            let tree = info
                .get_mut(b"file tree" as &[u8])
                .and_then(Value::as_dict_mut)
                .context("invalid file tree")?;
            let entry = tree
                .remove(&old_name)
                .context("hybrid single-file path differs from name")?;
            tree.insert(name.as_bytes().to_vec(), entry);
        }
    }
    for (key, text) in [
        (b"name" as &[u8], &options.name),
        (b"source", &options.source),
    ] {
        if let Some(text) = text {
            info.insert(key.to_vec(), Value::Bytes(text.as_bytes().to_vec()));
        }
    }
    if let Some(private) = options.private {
        info.insert(b"private".to_vec(), Value::Int(i64::from(private)));
    }
    if let Some(collections) = &options.collections {
        replace_list(
            info,
            b"collections",
            string_values(collections),
            options.list_mode,
        );
    }
    if let Some(similar) = &options.similar {
        ensure!(
            similar.iter().all(|hash| hash.len() == 20),
            "similar torrent hashes must be 20 bytes"
        );
        replace_list(
            info,
            b"similar",
            similar.iter().cloned().map(Value::Bytes).collect(),
            options.list_mode,
        );
    }
    from_value(value.clone())?;
    Ok(value)
}

#[cfg(any(windows, test))]
fn windows_component_safe(component: &str) -> bool {
    if component.ends_with(['.', ' '])
        || component
            .chars()
            .any(|c| c < ' ' || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return false;
    }
    let upper = component.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or("").trim_end_matches(' ');
    if matches!(stem, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return false;
    }
    let port = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    !port.is_some_and(|suffix| {
        matches!(
            suffix,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    })
}

fn component_path(component: &[u8]) -> Result<PathBuf> {
    safe_component(component)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(component.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        let component = std::str::from_utf8(component).context("file path is not valid UTF-8")?;
        #[cfg(windows)]
        ensure!(
            windows_component_safe(component),
            "path component is reserved or ambiguous on Windows"
        );
        Ok(PathBuf::from(component))
    }
}

fn relative_path(components: &[Vec<u8>]) -> Result<PathBuf> {
    let mut path = PathBuf::new();
    for component in components {
        path.push(component_path(component)?);
    }
    Ok(path)
}

fn payload_root(document: &Document, target: &Path, files: &[TorrentFile]) -> Result<PathBuf> {
    ensure!(target.is_dir(), "payload target must be a directory");
    let target = target.canonicalize()?;
    let first = files
        .iter()
        .find(|f| !f.padding)
        .context("torrent has no payload files")?;
    if target.join(relative_path(&first.components)?).exists() {
        return Ok(target);
    }
    let name = required(info(document)?, b"name")?
        .as_bytes()
        .context("invalid name")?;
    let named = target.join(component_path(name)?);
    if named.join(relative_path(&first.components)?).exists() {
        let canonical = named.canonicalize()?;
        ensure!(
            canonical.starts_with(&target),
            "payload directory escapes target"
        );
        return Ok(canonical);
    }
    Ok(target)
}

fn checked_payload_path(root: &Path, components: &[Vec<u8>], must_exist: bool) -> Result<PathBuf> {
    let relative = relative_path(components)?;
    let path = root.join(relative);
    let mut walk = root.to_path_buf();
    for component in components {
        walk.push(component_path(component)?);
        match fs::symlink_metadata(&walk) {
            Ok(meta) => {
                let is_link = meta.file_type().is_symlink();
                #[cfg(windows)]
                let is_link = {
                    use std::os::windows::fs::MetadataExt;
                    is_link || meta.file_attributes() & 0x400 != 0
                };
                ensure!(
                    !is_link,
                    "payload path contains a symbolic link or reparse point: {}",
                    walk.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !must_exist => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot access {}", walk.display()));
            }
        }
    }
    Ok(path)
}

pub fn verify(document: &Document, target: &Path, workers: Option<usize>) -> Result<Json> {
    validate(document)?;
    let torrent_files = files(document)?;
    let payload_files: Vec<_> = torrent_files.iter().filter(|f| !f.padding).collect();
    let exact_file = target.is_file();
    ensure!(
        !exact_file || payload_files.len() == 1,
        "a multifile payload requires a directory target"
    );
    let root = if exact_file {
        None
    } else {
        Some(payload_root(document, target, &torrent_files)?)
    };
    let mut entries = Vec::new();
    for file in &torrent_files {
        let disk_path = if file.padding {
            PathBuf::new()
        } else if let Some(root) = &root {
            checked_payload_path(root, &file.components, true)?
        } else {
            target.to_path_buf()
        };
        if !file.padding {
            let metadata = fs::metadata(&disk_path)
                .with_context(|| format!("cannot read {}", disk_path.display()))?;
            ensure!(
                metadata.is_file() && metadata.len() == file.length,
                "payload size or type does not match: {}",
                disk_path.display()
            );
        }
        entries.push(crate::create::FileEntry {
            disk_path,
            components: file.components.clone(),
            length: file.length,
            padding: file.padding,
        });
    }
    let workers =
        workers.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    ensure!(workers > 0, "worker count must be positive");
    let protocol = protocol(document)?;
    let actual = crate::create::hash_files(&entries, piece_length(document)?, protocol, workers)?;
    let mut v1_mismatches = Vec::new();
    if matches!(protocol, Protocol::V1 | Protocol::Hybrid) {
        let expected = required(info(document)?, b"pieces")?
            .as_bytes()
            .context("invalid pieces")?;
        for (index, (expected, actual)) in expected
            .as_chunks::<20>()
            .0
            .iter()
            .zip(&actual.v1)
            .enumerate()
        {
            if expected != actual {
                v1_mismatches.push(index);
            }
        }
        ensure!(
            expected.len() / 20 == actual.v1.len(),
            "hashing returned an unexpected piece count"
        );
    }
    let mut v2_mismatches = Vec::new();
    if matches!(protocol, Protocol::V2 | Protocol::Hybrid) {
        ensure!(
            actual.v2.len() == torrent_files.len(),
            "hashing returned an unexpected file count"
        );
        for (file, actual) in torrent_files.iter().zip(&actual.v2) {
            if !file.padding && file.pieces_root != actual.root {
                v2_mismatches.push(
                    file.components
                        .iter()
                        .map(|part| String::from_utf8_lossy(part))
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
    }
    let bytes = payload_files.iter().try_fold(0u64, |n, f| {
        n.checked_add(f.length).context("payload length overflow")
    })?;
    Ok(
        json!({"valid":v1_mismatches.is_empty() && v2_mismatches.is_empty(), "files":payload_files.len(), "bytes":bytes,
        "v1_mismatches":v1_mismatches,"v2_mismatches":v2_mismatches}),
    )
}

fn check_padding_file(mut file: File, length: u64, path: &Path) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() == length,
        "existing padding file has the wrong size or type: {}",
        path.display()
    );
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..count])?;
        ensure!(
            buffer[..count].iter().all(|byte| *byte == 0),
            "existing padding file contains nonzero bytes: {}",
            path.display()
        );
        remaining -= count as u64;
    }
    ensure!(
        file.read(&mut buffer[..1])? == 0 && file.metadata()?.len() == length,
        "padding file length changed while reading {}",
        path.display()
    );
    Ok(())
}

#[cfg(unix)]
mod padding_io {
    use std::{
        ffi::{CStr, CString},
        io,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };

    use super::*;

    pub(super) struct Root {
        directory: File,
        path: PathBuf,
    }

    #[cfg(target_os = "linux")]
    const DIRECTORY_FLAGS: i32 = libc::O_PATH | libc::O_DIRECTORY;
    #[cfg(not(target_os = "linux"))]
    const DIRECTORY_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY;

    fn open_at(parent: &File, name: &CStr, flags: i32) -> io::Result<File> {
        // The path is one checked component; successful descriptors become
        // owned Files immediately, so every error path closes them.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o666,
            )
        };
        if descriptor < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: openat returned a fresh descriptor owned by this call.
            Ok(unsafe { File::from_raw_fd(descriptor) })
        }
    }

    fn directory_at(parent: &File, name: &CStr, create: bool) -> io::Result<File> {
        match open_at(parent, name, DIRECTORY_FLAGS) {
            Ok(directory) => return Ok(directory),
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // Another writer may create this component concurrently. Opening it
        // afterward with O_NOFOLLOW still rejects a substituted symbolic link.
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        open_at(parent, name, DIRECTORY_FLAGS)
    }

    impl Root {
        pub(super) fn open(path: &Path) -> Result<Self> {
            ensure!(path.is_absolute(), "padding root must be absolute");
            let mut directory = File::open("/")?;
            for component in path.components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        let name = CString::new(name.as_bytes())?;
                        directory = directory_at(&directory, &name, false)
                            .with_context(|| format!("open padding root {}", path.display()))?;
                    }
                    _ => bail!("padding root must be canonical"),
                }
            }
            Ok(Self {
                directory,
                path: path.to_path_buf(),
            })
        }

        pub(super) fn ensure(&self, components: &[Vec<u8>], length: u64) -> Result<bool> {
            let (name, parents) = components.split_last().context("empty padding path")?;
            for component in components {
                safe_component(component)?;
            }
            let path = self.path.join(relative_path(components)?);
            let mut directory = self.directory.try_clone()?;
            for parent in parents {
                directory = directory_at(&directory, &CString::new(parent.as_slice())?, true)
                    .with_context(|| format!("open padding directory for {}", path.display()))?;
            }
            let name = CString::new(name.as_slice())?;
            match open_at(
                &directory,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            ) {
                Ok(file) => {
                    file.set_len(length)?;
                    Ok(true)
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // Nonblocking open avoids waiting on a substituted FIFO.
                    // The descriptor's type is checked before any reads.
                    let file = open_at(&directory, &name, libc::O_RDONLY | libc::O_NONBLOCK)
                        .with_context(|| format!("open existing padding {}", path.display()))?;
                    check_padding_file(file, length, &path)?;
                    Ok(false)
                }
                Err(error) => {
                    Err(error).with_context(|| format!("create padding {}", path.display()))
                }
            }
        }
    }
}

#[cfg(windows)]
mod padding_io {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use super::*;

    const OPEN_REPARSE_POINT: u32 = 0x00200000;
    const BACKUP_SEMANTICS: u32 = 0x02000000;
    const READ_ATTRIBUTES: u32 = 0x80;
    const LIST_DIRECTORY: u32 = 1;
    const SHARE_READ: u32 = 1;

    pub(super) struct Root {
        path: PathBuf,
        _guards: Vec<File>,
    }

    fn guard_directory(path: &Path) -> Result<File> {
        // Keep directory handles open without delete or write sharing while
        // children are created. Junctions are inspected rather than followed.
        let file = fs::OpenOptions::new()
            .access_mode(READ_ATTRIBUTES | LIST_DIRECTORY)
            .share_mode(SHARE_READ)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
            "padding directory is not a regular directory: {}",
            path.display()
        );
        Ok(file)
    }

    impl Root {
        pub(super) fn open(root: &Path) -> Result<Self> {
            ensure!(root.is_absolute(), "padding root must be absolute");
            let mut path = PathBuf::new();
            let mut guards = Vec::new();
            for component in root.components() {
                path.push(component.as_os_str());
                match component {
                    std::path::Component::Prefix(_) => {}
                    std::path::Component::RootDir | std::path::Component::Normal(_) => {
                        guards.push(guard_directory(&path)?);
                    }
                    _ => bail!("padding root must be canonical"),
                }
            }
            Ok(Self {
                path,
                _guards: guards,
            })
        }

        pub(super) fn ensure(&self, components: &[Vec<u8>], length: u64) -> Result<bool> {
            let (name, parents) = components.split_last().context("empty padding path")?;
            let mut path = self.path.clone();
            let mut guards = Vec::new();
            for parent in parents {
                path.push(component_path(parent)?);
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error).context("create padding directory"),
                }
                guards.push(guard_directory(&path)?);
            }
            path.push(component_path(name)?);
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(SHARE_READ)
                .custom_flags(OPEN_REPARSE_POINT)
                .open(&path)
            {
                Ok(file) => {
                    file.set_len(length)?;
                    Ok(true)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let file = fs::OpenOptions::new()
                        .read(true)
                        .share_mode(SHARE_READ)
                        .custom_flags(OPEN_REPARSE_POINT)
                        .open(&path)?;
                    ensure!(
                        file.metadata()?.file_attributes() & 0x400 == 0,
                        "padding file is a reparse point"
                    );
                    check_padding_file(file, length, &path)?;
                    Ok(false)
                }
                Err(error) => Err(error).context("create padding file"),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn directory_guard_blocks_replacement_until_released() {
            let temporary = tempfile::tempdir().unwrap();
            let directory = temporary.path().join("data");
            fs::create_dir(&directory).unwrap();
            let root = Root::open(&directory.canonicalize().unwrap()).unwrap();
            assert!(fs::rename(&directory, temporary.path().join("moved")).is_err());
            let path = vec![b".pad".to_vec(), b"zero".to_vec()];
            assert!(root.ensure(&path, 16383).unwrap());
            assert!(!root.ensure(&path, 16383).unwrap());
            drop(root);
            fs::rename(&directory, temporary.path().join("moved")).unwrap();
        }

        #[test]
        fn junction_padding_directory_is_rejected() {
            let temporary = tempfile::tempdir().unwrap();
            let directory = temporary.path().join("data");
            let outside = temporary.path().join("outside");
            fs::create_dir(&directory).unwrap();
            fs::create_dir(&outside).unwrap();
            let junction = directory.join(".pad");
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&junction)
                .arg(&outside)
                .status()
                .unwrap();
            assert!(status.success());
            let root = Root::open(&directory.canonicalize().unwrap()).unwrap();
            assert!(
                root.ensure(&[b".pad".to_vec(), b"zero".to_vec()], 16383)
                    .is_err()
            );
            assert!(!outside.join("zero").exists());
            drop(root);
            fs::remove_dir(junction).unwrap();
        }
    }
}

pub fn pad(document: &Document, target: &Path) -> Result<usize> {
    validate(document)?;
    let files = files(document)?;
    let root = payload_root(document, target, &files)?;
    #[cfg(any(unix, windows))]
    let directory = padding_io::Root::open(&root)?;
    let mut created = 0;
    for file in files.iter().filter(|f| f.padding) {
        #[cfg(any(unix, windows))]
        {
            if directory.ensure(&file.components, file.length)? {
                created += 1;
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let path = checked_payload_path(&root, &file.components, false)?;
            if path.exists() {
                check_padding_file(File::open(&path)?, file.length, &path)?;
                continue;
            }
            fs::create_dir_all(path.parent().context("padding path has no parent")?)?;
            // Recheck newly created directories for links and reparse points.
            checked_payload_path(&root, &file.components, false)?;
            let output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            output.set_len(file.length)?;
            created += 1;
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple() -> Document {
        // SHA-1 of the one-byte payload "x".
        let hash = hex::decode("11f6ad8ec52a2984abaafd7c3b516503785c2072").unwrap();
        from_value(Value::Dict(BTreeMap::from([
            (
                b"info".to_vec(),
                Value::Dict(BTreeMap::from([
                    (b"length".to_vec(), Value::Int(1)),
                    (b"name".to_vec(), Value::Bytes(b"x".to_vec())),
                    (b"piece length".to_vec(), Value::Int(16384)),
                    (b"pieces".to_vec(), Value::Bytes(hash)),
                ])),
            ),
            (b"unknown".to_vec(), Value::Bytes(vec![0, 255])),
        ])))
        .unwrap()
    }

    #[test]
    fn raw_info_hash_matches_independent_vector() {
        let document = simple();
        assert_eq!(
            hex::encode(info_hashes(&document).unwrap().v1.unwrap()),
            "01692ba668f0f246953995332b03d401e1052cac"
        );
    }

    #[test]
    fn tracker_edit_preserves_info_and_unknown_metadata() {
        let document = simple();
        let edited = edit(
            &document,
            &EditOptions {
                announces: Some(vec![vec!["https://tracker.example/announce".into()]]),
                comment: Some("note".into()),
                ..EditOptions::default()
            },
        )
        .unwrap();
        let result = from_value(edited).unwrap();
        assert_eq!(result.raw_info, document.raw_info);
        assert_eq!(
            info_hashes(&result).unwrap(),
            info_hashes(&document).unwrap()
        );
        assert_eq!(result.value.get(b"unknown"), document.value.get(b"unknown"));
        assert!(
            magnet(&result, true, false)
                .unwrap()
                .contains("tr=https%3A%2F%2Ftracker.example%2Fannounce")
        );
    }

    #[test]
    fn validates_piece_counts_and_path_safety() {
        for bad_name in [b"..".as_slice(), b"a/b", b"a\\b", b"C:x", b"a\0b"] {
            let mut value = simple().value;
            value
                .get_mut(b"info")
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .insert(b"name".to_vec(), Value::Bytes(bad_name.to_vec()));
            assert!(from_value(value).is_err());
        }
        let mut value = simple().value;
        value
            .get_mut(b"info")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert(b"pieces".to_vec(), Value::Bytes(vec![]));
        assert!(from_value(value).is_err());
    }

    #[test]
    fn pointer_handles_lists_escapes_and_binary_keys() {
        let value = Value::Dict(BTreeMap::from([
            (b"a/b~".to_vec(), Value::List(vec![Value::Int(7)])),
            (vec![255], Value::Int(9)),
        ]));
        assert_eq!(query(&value, "/a~1b~0/0").unwrap().as_int(), Some(7));
        assert_eq!(query(&value, "/@hex:ff").unwrap().as_int(), Some(9));
        for path in ["/a~2", "/a~1b~0/01", "/a~1b~0/-1", "bad"] {
            assert!(query(&value, path).is_err());
        }
    }

    #[test]
    fn atomic_writes_require_explicit_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.torrent");
        let doc = simple();
        write_atomic(&path, &doc.value, false).unwrap();
        assert_eq!(load(&path).unwrap().raw_info, doc.raw_info);
        assert!(write_atomic(&path, &doc.value, false).is_err());
        write_atomic(&path, &doc.value, true).unwrap();
    }

    #[test]
    fn list_edits_keep_order_and_remove_duplicates() {
        let document = simple();
        let value = edit(
            &document,
            &EditOptions {
                web_seeds: Some(vec!["a".into(), "b".into()]),
                ..EditOptions::default()
            },
        )
        .unwrap();
        let document = from_value(value).unwrap();
        let value = edit(
            &document,
            &EditOptions {
                web_seeds: Some(vec!["b".into(), "c".into()]),
                list_mode: ListMode::Prepend,
                ..EditOptions::default()
            },
        )
        .unwrap();
        let urls = value.get(b"url-list").unwrap().as_list().unwrap();
        assert_eq!(
            urls.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
    }

    #[test]
    fn validates_v2_piece_layers_against_root() {
        let data = vec![42; 3 * merkle::BLOCK_LENGTH + 7];
        let hashes = merkle::hash_file(&data, 2 * merkle::BLOCK_LENGTH);
        let root = hashes.root.unwrap();
        let mut value = Value::Dict(BTreeMap::from([
            (
                b"info".to_vec(),
                Value::Dict(BTreeMap::from([
                    (b"name".to_vec(), Value::Bytes(b"x".to_vec())),
                    (b"meta version".to_vec(), Value::Int(2)),
                    (
                        b"piece length".to_vec(),
                        Value::Int((2 * merkle::BLOCK_LENGTH) as i64),
                    ),
                    (
                        b"file tree".to_vec(),
                        Value::Dict(BTreeMap::from([(
                            b"x".to_vec(),
                            Value::Dict(BTreeMap::from([(
                                vec![],
                                Value::Dict(BTreeMap::from([
                                    (b"length".to_vec(), Value::Int(data.len() as i64)),
                                    (b"pieces root".to_vec(), Value::Bytes(root.to_vec())),
                                ])),
                            )])),
                        )])),
                    ),
                ])),
            ),
            (
                b"piece layers".to_vec(),
                Value::Dict(BTreeMap::from([(
                    root.to_vec(),
                    Value::Bytes(hashes.piece_layer.into_iter().flatten().collect()),
                )])),
            ),
        ]));
        from_value(value.clone()).unwrap();
        let layer = value
            .get_mut(b"piece layers")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut(root.as_slice())
            .unwrap();
        if let Value::Bytes(bytes) = layer {
            bytes[0] ^= 1;
        }
        assert!(from_value(value).is_err());
    }

    #[test]
    fn verifies_payload_and_reports_corruption() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("x"), b"x").unwrap();
        let document = simple();
        assert_eq!(
            verify(&document, temp.path(), Some(1)).unwrap()["valid"],
            true
        );
        fs::write(temp.path().join("x"), b"y").unwrap();
        let result = verify(&document, temp.path(), Some(1)).unwrap();
        assert_eq!(result["valid"], false);
        assert_eq!(result["v1_mismatches"], json!([0]));
    }

    #[test]
    fn creates_and_verifies_all_protocols_with_empty_and_nested_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("sub")).unwrap();
        fs::write(temp.path().join("a"), b"").unwrap();
        fs::write(temp.path().join("b"), b"abc").unwrap();
        fs::write(
            temp.path().join("sub/c"),
            vec![42; 4 * merkle::BLOCK_LENGTH + 17],
        )
        .unwrap();
        for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
            let result = crate::create::create(&crate::create::CreateOptions {
                target: temp.path().to_path_buf(),
                protocol,
                piece_length: Some(2 * merkle::BLOCK_LENGTH),
                threads: Some(2),
                ..crate::create::CreateOptions::default()
            })
            .unwrap();
            let document = from_value(result.value).unwrap();
            assert_eq!(
                verify(&document, temp.path(), Some(2)).unwrap()["valid"],
                true
            );
            if protocol == Protocol::Hybrid {
                assert_eq!(pad(&document, temp.path()).unwrap(), 1);
                assert_eq!(pad(&document, temp.path()).unwrap(), 0);
                let padding = files(&document)
                    .unwrap()
                    .into_iter()
                    .find(|f| f.padding)
                    .unwrap();
                let path = temp
                    .path()
                    .join(relative_path(&padding.components).unwrap());
                let bytes = fs::read(&path).unwrap();
                assert_eq!(bytes.len() as u64, padding.length);
                assert!(bytes.iter().all(|b| *b == 0));
            }
        }
    }

    #[test]
    fn hybrid_single_file_rename_preserves_content_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("before");
        fs::write(&path, b"abc").unwrap();
        let result = crate::create::create(&crate::create::CreateOptions {
            target: path.clone(),
            protocol: Protocol::Hybrid,
            threads: Some(1),
            ..crate::create::CreateOptions::default()
        })
        .unwrap();
        let document = from_value(result.value).unwrap();
        let edited = from_value(
            edit(
                &document,
                &EditOptions {
                    name: Some("after".into()),
                    ..EditOptions::default()
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            files(&edited).unwrap()[0].components,
            vec![b"after".to_vec()]
        );
        assert_eq!(
            query(&edited.value, "/info/pieces").unwrap(),
            query(&document.value, "/info/pieces").unwrap()
        );
        assert_eq!(verify(&edited, &path, Some(1)).unwrap()["valid"], true);
    }
    #[test]
    fn shared_piece_layers_still_check_every_file_length() {
        let layer = [[1_u8; 32], [2_u8; 32]];
        let root = merkle::combine_root(&layer, 0);
        let properties = Value::Dict(BTreeMap::from([(
            Vec::new(),
            Value::Dict(BTreeMap::from([
                (
                    b"length".to_vec(),
                    Value::Int((2 * merkle::BLOCK_LENGTH) as i64),
                ),
                (b"pieces root".to_vec(), Value::Bytes(root.to_vec())),
            ])),
        )]));
        let value = Value::Dict(BTreeMap::from([
            (
                b"info".to_vec(),
                Value::Dict(BTreeMap::from([
                    (b"name".to_vec(), Value::Bytes(b"shared".to_vec())),
                    (b"meta version".to_vec(), Value::Int(2)),
                    (
                        b"piece length".to_vec(),
                        Value::Int(merkle::BLOCK_LENGTH as i64),
                    ),
                    (
                        b"file tree".to_vec(),
                        Value::Dict(BTreeMap::from([
                            (b"a".to_vec(), properties.clone()),
                            (b"b".to_vec(), properties),
                        ])),
                    ),
                ])),
            ),
            (
                b"piece layers".to_vec(),
                Value::Dict(BTreeMap::from([(
                    root.to_vec(),
                    Value::Bytes(layer.concat()),
                )])),
            ),
        ]));
        from_value(value.clone()).unwrap();
        let mut wrong_length = value.clone();
        wrong_length
            .get_mut(b"info")
            .unwrap()
            .get_mut(b"file tree")
            .unwrap()
            .get_mut(b"b")
            .unwrap()
            .get_mut(b"")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert(
                b"length".to_vec(),
                Value::Int((3 * merkle::BLOCK_LENGTH) as i64),
            );
        assert!(from_value(wrong_length).is_err());
        let mut corrupted = value;
        corrupted
            .get_mut(b"piece layers")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert(root.to_vec(), Value::Bytes(vec![0; 64]));
        assert!(from_value(corrupted).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn padding_directory_descriptor_survives_path_replacement() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let held = temp.path().join("held");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let directory = padding_io::Root::open(&root).unwrap();
        fs::rename(&root, &held).unwrap();
        symlink(&outside, &root).unwrap();
        let path = [b"nested".to_vec(), b"zero".to_vec()];
        assert!(directory.ensure(&path, 17).unwrap());
        assert_eq!(fs::read(held.join("nested/zero")).unwrap(), vec![0; 17]);
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert!(!directory.ensure(&path, 17).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn padding_refuses_ancestor_links_final_links_and_fifos() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("data"), b"unchanged").unwrap();
        let directory = padding_io::Root::open(&root).unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        assert!(
            directory
                .ensure(&[b"linked".to_vec(), b"new".to_vec()], 32)
                .is_err()
        );
        symlink(outside.join("data"), root.join("final")).unwrap();
        assert!(directory.ensure(&[b"final".to_vec()], 9).is_err());
        assert_eq!(fs::read(outside.join("data")).unwrap(), b"unchanged");
        assert!(!outside.join("new").exists());
        let fifo = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: the C string is live and references a path inside this test's directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(directory.ensure(&[b"fifo".to_vec()], 0).is_err());
        fs::write(root.join("nonzero"), b"x").unwrap();
        assert!(directory.ensure(&[b"nonzero".to_vec()], 1).is_err());
    }
    #[test]
    fn windows_payload_components_reject_device_and_normalization_aliases() {
        for name in [
            "NUL",
            "nul.txt",
            "CON",
            "LPT9.data",
            "COM¹",
            ".. ",
            "name.",
            "name ",
            "a?b",
            "CONOUT$",
        ] {
            assert!(!windows_component_safe(name), "accepted {name:?}");
        }
        for name in ["normal", "a.b", "COM10", "NULL", "résumé"] {
            assert!(windows_component_safe(name), "rejected {name:?}");
        }
    }
}
