use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use crate::{
    bencode::{self, Value},
    config::Config,
    create::{self, CreateOptions, IoMode, Protocol},
    metadata::{self, Document, EditOptions},
};

const MAX_EDIT_BYTES: usize = 8 * 1024 * 1024;
const MAX_EDIT_DEPTH: usize = 64;
const MAX_EDIT_VALUES: usize = 100_000;

#[derive(Clone, Debug)]
pub struct CreateForm {
    pub input: String,
    pub output: String,
    pub name: String,
    pub trackers: String,
    pub web_seeds: String,
    pub dht_nodes: String,
    pub source: String,
    pub comment: String,
    pub workers: String,
    pub protocol: Protocol,
    pub piece_length: Option<usize>,
    pub private: bool,
    pub io: IoMode,
    pub include_hidden: bool,
    pub follow_symlinks: bool,
    pub checksums: Vec<String>,
}

impl Default for CreateForm {
    fn default() -> Self {
        Self {
            input: String::new(),
            output: String::new(),
            name: String::new(),
            trackers: String::new(),
            web_seeds: String::new(),
            dht_nodes: String::new(),
            source: String::new(),
            comment: String::new(),
            workers: "auto".into(),
            protocol: Protocol::V1,
            piece_length: None,
            private: true,
            io: IoMode::Auto,
            include_hidden: false,
            follow_symlinks: false,
            checksums: Vec::new(),
        }
    }
}

impl CreateForm {
    pub fn options(&self) -> Result<CreateOptions> {
        let input = expand_path(&self.input).context("choose an input file or folder")?;
        let output = expand_path(&self.output).context("choose an output torrent path")?;
        validate_create_paths(&input, &output)?;
        let workers = self.workers.trim();
        let threads = if workers.is_empty() || workers.eq_ignore_ascii_case("auto") {
            None
        } else {
            let count: usize = workers
                .parse()
                .context("workers must be auto or a positive number")?;
            ensure!(count > 0, "workers must be positive");
            ensure!(count <= 4096, "workers must not exceed 4096");
            Some(count)
        };
        let options = CreateOptions {
            target: input,
            output: Some(output),
            protocol: self.protocol,
            piece_length: self.piece_length,
            threads,
            io: self.io,
            name: optional_text(&self.name),
            announces: parse_trackers(&self.trackers)?,
            web_seeds: parse_web_seeds(&self.web_seeds)?,
            dht_nodes: parse_dht_nodes(&self.dht_nodes)?,
            private: self.private.then_some(true),
            source: optional_text(&self.source),
            comment: optional_text(&self.comment),
            include_hidden: self.include_hidden,
            follow_symlinks: self.follow_symlinks,
            checksums: self.checksums.clone(),
            ..CreateOptions::default()
        };
        ensure!(
            !self.private || options.dht_nodes.is_empty(),
            "private torrents cannot include DHT nodes"
        );
        create::validate_options(&options)?;
        Ok(options)
    }
}

fn optional_text(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

pub fn expand_path(text: &str) -> Result<PathBuf> {
    let text = text.trim();
    ensure!(!text.is_empty(), "path must not be empty");
    ensure!(!text.contains('\0'), "path contains NUL");
    if text == "~" || text.starts_with("~/") || (cfg!(windows) && text.starts_with("~\\")) {
        #[cfg(windows)]
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let directory = std::env::var_os("HOMEPATH")?;
                let mut combined = drive;
                combined.push(directory);
                Some(PathBuf::from(combined))
            });
        #[cfg(not(windows))]
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let home = home.context("home directory is unavailable; use a full path")?;
        return Ok(if text.len() == 1 {
            home
        } else {
            home.join(&text[2..])
        });
    }
    ensure!(
        !text.starts_with('~'),
        "use ~/ for your home directory or a full path"
    );
    Ok(PathBuf::from(text))
}

fn validate_output_path(output: &Path) -> Result<()> {
    ensure!(output.file_name().is_some(), "output must name a file");
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(
        parent.is_dir(),
        "output folder does not exist: {}",
        parent.display()
    );
    match fs::symlink_metadata(output) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "output must be a regular file, not a folder or link"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect output path"),
    }
    Ok(())
}

pub fn validate_create_paths(input: &Path, output: &Path) -> Result<()> {
    let input_metadata =
        fs::metadata(input).with_context(|| format!("cannot read input {}", input.display()))?;
    ensure!(
        input_metadata.is_file() || input_metadata.is_dir(),
        "input must be a regular file or folder"
    );
    validate_output_path(output)?;
    if output.exists() {
        ensure!(
            input.canonicalize()? != output.canonicalize()?,
            "output cannot replace the input"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let output_metadata = fs::metadata(output)?;
            ensure!(
                input_metadata.dev() != output_metadata.dev()
                    || input_metadata.ino() != output_metadata.ino(),
                "output cannot replace a hard link to the input"
            );
        }
    }
    Ok(())
}

pub fn parse_trackers(text: &str) -> Result<Vec<Vec<String>>> {
    ensure!(text.len() <= 4 * 1024 * 1024, "tracker text exceeds 4 MiB");
    let mut tiers = Vec::new();
    let mut tier = Vec::new();
    for line in text.lines() {
        let url = line.trim();
        if url.is_empty() {
            if !tier.is_empty() {
                tiers.push(std::mem::take(&mut tier));
            }
        } else {
            ensure!(
                !url.chars().any(char::is_whitespace),
                "use one tracker URL per line"
            );
            tier.push(url.to_owned());
        }
    }
    if !tier.is_empty() {
        tiers.push(tier);
    }
    let urls = tiers.iter().flatten().cloned().collect::<Vec<_>>();
    Config::default().announces(&urls, &[])?;
    Ok(tiers)
}

fn parse_web_seeds(text: &str) -> Result<Vec<String>> {
    ensure!(text.len() <= 4 * 1024 * 1024, "web seed text exceeds 4 MiB");
    let urls = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(urls.len() <= 4096, "too many web seed URLs");
    for url in &urls {
        ensure!(url.len() <= 16 * 1024, "web seed URL exceeds 16 KiB");
        ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "web seed URL needs http:// or https://"
        );
        ensure!(
            !url.chars().any(char::is_whitespace),
            "use one web seed URL per line"
        );
    }
    Ok(urls)
}

fn parse_dht_nodes(text: &str) -> Result<Vec<String>> {
    ensure!(text.len() <= 1024 * 1024, "DHT node text exceeds 1 MiB");
    let nodes = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(nodes.len() <= 4096, "too many DHT nodes");
    for node in &nodes {
        ensure!(
            !node.chars().any(char::is_whitespace),
            "use one DHT host:port per line"
        );
        create::parse_node(node)?;
    }
    Ok(nodes)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorForm {
    pub name: String,
    pub private: bool,
    pub source: String,
    pub comment: String,
    pub created_by: String,
    pub creation_date: String,
    pub trackers: String,
    pub web_seeds: String,
    pub dht_nodes: String,
}

impl EditorForm {
    pub fn from_document(document: &Document) -> Self {
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_bytes)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default()
        };
        let info = document.value.get(b"info");
        let info_value = |key: &[u8]| info.and_then(|value| value.get(key));
        let tiers = document
            .value
            .get(b"announce-list")
            .and_then(Value::as_list)
            .map(|tiers| {
                tiers
                    .iter()
                    .filter_map(Value::as_list)
                    .map(|tier| {
                        tier.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .filter(|tier| !tier.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .filter(|tiers| !tiers.is_empty())
            .unwrap_or_else(|| text(document.value.get(b"announce")));
        let web_seeds = match document.value.get(b"url-list") {
            Some(Value::List(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            value => text(value),
        };
        let dht_nodes = document
            .value
            .get(b"nodes")
            .and_then(Value::as_list)
            .map(|list| {
                list.iter()
                    .filter_map(|entry| {
                        let parts = entry.as_list()?;
                        if parts.len() != 2 {
                            return None;
                        }
                        let host = parts[0].as_str()?;
                        let port = parts[1].as_int()?;
                        Some(if host.contains(':') {
                            format!("[{host}]:{port}")
                        } else {
                            format!("{host}:{port}")
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Self {
            name: text(info_value(b"name")),
            private: info_value(b"private").and_then(Value::as_int) == Some(1),
            source: text(info_value(b"source")),
            comment: text(document.value.get(b"comment")),
            created_by: text(document.value.get(b"created by")),
            creation_date: document
                .value
                .get(b"creation date")
                .and_then(Value::as_int)
                .map(|n| n.to_string())
                .unwrap_or_default(),
            trackers: tiers,
            web_seeds,
            dht_nodes,
        }
    }

    pub fn apply(&self, document: &Document) -> Result<Document> {
        let original = Self::from_document(document);
        if *self == original {
            return Ok(document.clone());
        }
        let mut options = EditOptions::default();
        if self.name != original.name {
            ensure!(!self.name.is_empty(), "torrent name must not be empty");
            options.name = Some(self.name.clone());
        }
        if self.private != original.private {
            options.private = Some(self.private);
        }
        if self.source != original.source {
            options.source = Some(self.source.clone());
        }
        if self.comment != original.comment {
            options.comment = Some(self.comment.clone());
        }
        if self.created_by != original.created_by {
            if self.created_by.is_empty() {
                options.remove_created_by = true;
            } else {
                options.created_by = Some(self.created_by.clone());
            }
        }
        if self.creation_date != original.creation_date {
            if self.creation_date.trim().is_empty() {
                options.remove_creation_date = true;
            } else {
                options.creation_date = Some(
                    self.creation_date
                        .trim()
                        .parse()
                        .context("creation date must be a Unix timestamp in seconds")?,
                );
            }
        }
        if self.trackers != original.trackers {
            options.announces = Some(parse_trackers(&self.trackers)?);
        }
        if self.web_seeds != original.web_seeds {
            options.web_seeds = Some(parse_web_seeds(&self.web_seeds)?);
        }
        if self.dht_nodes != original.dht_nodes {
            options.dht_nodes = Some(parse_dht_nodes(&self.dht_nodes)?);
        }
        ensure!(
            !self.private
                || self.dht_nodes.trim().is_empty()
                || (self.private == original.private && self.dht_nodes == original.dht_nodes),
            "private torrents cannot include DHT nodes"
        );
        let mut value = metadata::edit(document, &options)?;
        if self.source != original.source && self.source.is_empty() {
            value
                .get_mut(b"info")
                .and_then(Value::as_dict_mut)
                .context("info must be a dictionary")?
                .remove(b"source" as &[u8]);
        }
        if self.comment != original.comment && self.comment.is_empty() {
            value
                .as_dict_mut()
                .context("metainfo must be a dictionary")?
                .remove(b"comment" as &[u8]);
        }
        checked_document(document, value, false)
    }
}

pub fn save_document(document: &Document, path: &Path, allow_overwrite: bool) -> Result<()> {
    validate_structure(&document.value)?;
    metadata::validate(document)?;
    validate_output_path(path)?;
    metadata::write_atomic(path, &document.value, allow_overwrite)
}

fn is_hash_key(key: &[u8]) -> bool {
    matches!(key, b"pieces" | b"pieces root" | b"piece layers")
}

struct DigestWriter(Sha256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn protected_values(value: &Value, output: &mut BTreeMap<(Vec<u8>, [u8; 32]), usize>) {
    match value {
        Value::Dict(dict) => {
            for (key, value) in dict {
                if is_hash_key(key) {
                    let mut digest = DigestWriter(Sha256::new());
                    bencode::encode_to_writer(value, &mut digest)
                        .expect("hashing bencode cannot fail");
                    *output
                        .entry((key.clone(), digest.0.finalize().into()))
                        .or_default() += 1;
                }
                protected_values(value, output);
            }
        }
        Value::List(list) => {
            for value in list {
                protected_values(value, output);
            }
        }
        _ => {}
    }
}

fn checked_document(original: &Document, value: Value, allow_hash_edit: bool) -> Result<Document> {
    validate_structure(&value)?;
    if !allow_hash_edit {
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();
        protected_values(&original.value, &mut before);
        protected_values(&value, &mut after);
        ensure!(
            before == after,
            "piece hashes are protected; explicitly enable hash editing to change them"
        );
    }
    metadata::from_value(value)
}

fn validate_structure(value: &Value) -> Result<()> {
    fn walk(value: &Value, depth: usize, values: &mut usize) -> Result<()> {
        let limits = bencode::Limits::default();
        ensure!(
            depth <= limits.max_depth,
            "torrent nesting exceeds the bencode limit"
        );
        *values += 1;
        ensure!(
            *values <= limits.max_values,
            "torrent contains too many bencode values"
        );
        match value {
            Value::Dict(dict) => {
                for value in dict.values() {
                    walk(value, depth + 1, values)?;
                }
            }
            Value::List(list) => {
                for value in list {
                    walk(value, depth + 1, values)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    struct LimitWriter(usize);
    impl Write for LimitWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|total| *total <= bencode::Limits::default().max_bytes)
                .ok_or_else(|| io::Error::other("torrent exceeds the bencode size limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    walk(value, 0, &mut 0)?;
    bencode::encode_to_writer(value, &mut LimitWriter(0))
        .context("torrent exceeds the bencode size limit")
}

pub fn typed_json(value: &Value) -> String {
    fn convert(value: &Value) -> Json {
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
                _ => json!({"$bytes": hex::encode(bytes)}),
            },
            Value::List(list) => Json::Array(list.iter().map(convert).collect()),
            Value::Dict(dict) => {
                if dict.keys().all(|key| {
                    std::str::from_utf8(key).is_ok() && key != b"$bytes" && key != b"$dict"
                }) {
                    Json::Object(
                        dict.iter()
                            .map(|(key, value)| {
                                (String::from_utf8(key.clone()).unwrap(), convert(value))
                            })
                            .collect(),
                    )
                } else {
                    json!({"$dict": dict.iter().map(|(key, value)| json!({"key": hex::encode(key), "value": convert(value)})).collect::<Vec<_>>()})
                }
            }
        }
    }
    serde_json::to_string_pretty(&convert(value)).expect("bencode values are valid JSON")
}

pub fn field_json(value: &Value) -> Result<String> {
    fn walk(value: &Value, depth: usize, values: &mut usize, bytes: &mut usize) -> Result<()> {
        ensure!(
            depth <= MAX_EDIT_DEPTH,
            "select a smaller field; this field exceeds 64 levels"
        );
        *values += 1;
        ensure!(
            *values <= MAX_EDIT_VALUES,
            "select a smaller field; this field has too many values"
        );
        match value {
            Value::Bytes(value) => *bytes += value.len(),
            Value::Dict(dict) => {
                for (key, value) in dict {
                    *bytes += key.len();
                    ensure!(
                        *bytes <= MAX_EDIT_BYTES / 2,
                        "select a smaller field; this field is too large for the editor"
                    );
                    walk(value, depth + 1, values, bytes)?;
                }
            }
            Value::List(list) => {
                for value in list {
                    walk(value, depth + 1, values, bytes)?;
                }
            }
            Value::Int(_) => {}
        }
        ensure!(
            *bytes <= MAX_EDIT_BYTES / 2,
            "select a smaller field; this field is too large for the editor"
        );
        Ok(())
    }
    walk(value, 0, &mut 0, &mut 0)?;
    let text = typed_json(value);
    ensure!(
        text.len() <= MAX_EDIT_BYTES,
        "select a smaller field; this field is too large for the editor"
    );
    Ok(text)
}

pub fn parse_typed_json(text: &str) -> Result<Value> {
    ensure!(text.len() <= MAX_EDIT_BYTES, "field edit exceeds 8 MiB");
    let json = serde_json::from_str::<UniqueJson>(text)
        .context("field must be valid JSON with unique dictionary keys")?
        .0;
    fn convert(json: Json, depth: usize, values: &mut usize, bytes: &mut usize) -> Result<Value> {
        ensure!(depth <= MAX_EDIT_DEPTH, "field nesting exceeds 64 levels");
        *values += 1;
        ensure!(*values <= MAX_EDIT_VALUES, "field contains too many values");
        let mut charge = |size: usize| -> Result<()> {
            *bytes = bytes.checked_add(size).context("field size overflow")?;
            ensure!(*bytes <= MAX_EDIT_BYTES, "field decoded size exceeds 8 MiB");
            Ok(())
        };
        match json {
            Json::Number(number) => {
                Ok(Value::Int(number.as_i64().context(
                    "bencode needs a signed 64-bit integer; decimals are unsupported",
                )?))
            }
            Json::String(text) => {
                charge(text.len())?;
                Ok(Value::Bytes(text.into_bytes()))
            }
            Json::Array(list) => Ok(Value::List(
                list.into_iter()
                    .map(|value| convert(value, depth + 1, values, bytes))
                    .collect::<Result<Vec<_>>>()?,
            )),
            Json::Object(mut dict) => {
                if dict.len() == 1 && dict.contains_key("$bytes") {
                    let value = dict.remove("$bytes").unwrap();
                    let text = value
                        .as_str()
                        .context("$bytes must contain a hexadecimal string")?;
                    charge(text.len() / 2)?;
                    return Ok(Value::Bytes(
                        hex::decode(text).context("invalid hexadecimal byte string")?,
                    ));
                }
                if dict.len() == 1 && dict.contains_key("$dict") {
                    let value = dict.remove("$dict").unwrap();
                    let entries = value
                        .as_array()
                        .context("$dict must contain an array of {key,value} entries")?;
                    let mut result = BTreeMap::new();
                    for entry in entries {
                        let fields = entry.as_object().context("$dict entries must be objects")?;
                        ensure!(
                            fields.len() == 2
                                && fields.contains_key("key")
                                && fields.contains_key("value"),
                            "$dict entries require exactly key and value"
                        );
                        let key_text = fields["key"]
                            .as_str()
                            .context("$dict key must be hexadecimal text")?;
                        let key =
                            hex::decode(key_text).context("invalid hexadecimal dictionary key")?;
                        *bytes = bytes
                            .checked_add(key.len())
                            .context("field size overflow")?;
                        ensure!(*bytes <= MAX_EDIT_BYTES, "field decoded size exceeds 8 MiB");
                        let value = convert(fields["value"].clone(), depth + 1, values, bytes)?;
                        ensure!(
                            result.insert(key, value).is_none(),
                            "duplicate dictionary key"
                        );
                    }
                    return Ok(Value::Dict(result));
                }
                let mut result = BTreeMap::new();
                for (key, value) in dict {
                    *bytes = bytes
                        .checked_add(key.len())
                        .context("field size overflow")?;
                    ensure!(*bytes <= MAX_EDIT_BYTES, "field decoded size exceeds 8 MiB");
                    result.insert(key.into_bytes(), convert(value, depth + 1, values, bytes)?);
                }
                Ok(Value::Dict(result))
            }
            _ => bail!(
                "bencode supports strings, integers, lists and dictionaries; booleans and null are unsupported"
            ),
        }
    }
    convert(json, 0, &mut 0, &mut 0)
}

struct UniqueJson(Json);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = UniqueJson;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("JSON with unique dictionary keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(json!(value)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(Json::Null))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(json!(value)))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(json!(value)))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Err(E::custom("bencode does not support decimals"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(Json::String(value.to_owned())))
            }
            fn visit_string<E: de::Error>(
                self,
                value: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(Json::String(value)))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut list = Vec::new();
                while let Some(value) = access.next_element::<UniqueJson>()? {
                    if list.len() >= MAX_EDIT_VALUES {
                        return Err(de::Error::custom("field contains too many values"));
                    }
                    list.push(value.0);
                }
                Ok(UniqueJson(Json::Array(list)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut dict = serde_json::Map::new();
                while let Some(key) = access.next_key::<String>()? {
                    if dict.len() >= MAX_EDIT_VALUES {
                        return Err(de::Error::custom("field contains too many values"));
                    }
                    if dict.contains_key(&key) {
                        return Err(de::Error::custom("duplicate dictionary key"));
                    }
                    dict.insert(key, access.next_value::<UniqueJson>()?.0);
                }
                Ok(UniqueJson(Json::Object(dict)))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

#[derive(Clone, Debug)]
pub struct TreeRow {
    pub pointer: String,
    pub label: String,
    pub kind: &'static str,
    pub preview: String,
    pub depth: usize,
    pub protected: bool,
}

fn pointer_token(key: &[u8]) -> String {
    match std::str::from_utf8(key) {
        Ok(text) if !text.starts_with("@hex:") => text.replace('~', "~0").replace('/', "~1"),
        _ => format!("@hex:{}", hex::encode(key)),
    }
}

pub fn tree_rows(value: &Value) -> Vec<TreeRow> {
    const MAX_VIEW_BYTES: usize = 8 * 1024 * 1024;
    const MAX_VIEW_KEY: usize = 4096;
    const MAX_VIEW_POINTER: usize = 8192;
    const OMITTED_NOTICE: &str = " · large fields omitted from tree";
    struct TreeState {
        rows: Vec<TreeRow>,
        bytes: usize,
        omitted: bool,
    }

    fn bytes_preview(bytes: &[u8]) -> String {
        let prefix = &bytes[..bytes.len().min(72 * 4)];
        let text = match std::str::from_utf8(prefix) {
            Ok(text) => Some(text),
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&prefix[..error.valid_up_to()]).ok()
            }
            _ => None,
        };
        if let Some(text) = text.filter(|text| !text.is_empty() || bytes.is_empty()) {
            let mut sample = text.chars().take(72).collect::<String>();
            if !sample.chars().any(char::is_control) {
                if sample.len() < bytes.len() {
                    sample.push('…');
                }
                return format!("\"{sample}\" · {} B", bytes.len());
            }
        }
        format!(
            "{} bytes · {}{}",
            bytes.len(),
            hex::encode(&bytes[..bytes.len().min(16)]),
            if bytes.len() > 16 { "…" } else { "" }
        )
    }

    fn walk(
        value: &Value,
        pointer: String,
        label: String,
        depth: usize,
        inherited: bool,
        state: &mut TreeState,
    ) -> bool {
        if state.rows.len() >= MAX_EDIT_VALUES || depth > bencode::Limits::default().max_depth {
            state.omitted = true;
            return false;
        }
        let (kind, preview) = match value {
            Value::Int(number) => ("integer", number.to_string()),
            Value::Bytes(bytes) => ("bytes", bytes_preview(bytes)),
            Value::List(list) => ("list", format!("{} entries", list.len())),
            Value::Dict(dict) => ("dictionary", format!("{} fields", dict.len())),
        };
        let row_bytes = pointer.len() + label.len() + preview.len();
        if row_bytes > MAX_VIEW_BYTES - OMITTED_NOTICE.len() - state.bytes {
            state.omitted = true;
            return false;
        }
        state.bytes += row_bytes;
        let index = state.rows.len();
        state.rows.push(TreeRow {
            pointer: pointer.clone(),
            label,
            kind,
            preview,
            depth,
            protected: inherited,
        });
        let mut protected = inherited;
        match value {
            Value::Dict(dict) => {
                for (key, child) in dict {
                    if state.rows.len() >= MAX_EDIT_VALUES {
                        state.omitted = true;
                        break;
                    }
                    if key.len() > MAX_VIEW_KEY {
                        state.omitted = true;
                        continue;
                    }
                    let token_length = match std::str::from_utf8(key) {
                        Ok(text) if !text.starts_with("@hex:") => {
                            key.len()
                                + key
                                    .iter()
                                    .filter(|byte| matches!(byte, b'~' | b'/'))
                                    .count()
                        }
                        _ => 5 + key.len() * 2,
                    };
                    if pointer.len() + 1 + token_length > MAX_VIEW_POINTER {
                        state.omitted = true;
                        continue;
                    }
                    let label = std::str::from_utf8(key)
                        .map(str::to_owned)
                        .unwrap_or_else(|_| format!("@hex:{}", hex::encode(key)));
                    protected |= walk(
                        child,
                        format!("{pointer}/{}", pointer_token(key)),
                        label,
                        depth + 1,
                        inherited || is_hash_key(key),
                        state,
                    );
                }
            }
            Value::List(list) => {
                for (index, child) in list.iter().enumerate() {
                    if state.rows.len() >= MAX_EDIT_VALUES {
                        state.omitted = true;
                        break;
                    }
                    let token = index.to_string();
                    if pointer.len() + 1 + token.len() > MAX_VIEW_POINTER {
                        state.omitted = true;
                        continue;
                    }
                    protected |= walk(
                        child,
                        format!("{pointer}/{token}"),
                        token,
                        depth + 1,
                        inherited,
                        state,
                    );
                }
            }
            _ => {}
        }
        state.rows[index].protected = protected;
        protected
    }
    let mut state = TreeState {
        rows: Vec::new(),
        bytes: 0,
        omitted: false,
    };
    walk(value, String::new(), "Torrent".into(), 0, false, &mut state);
    if state.omitted {
        state.rows[0].preview.push_str(OMITTED_NOTICE);
    }
    state.rows
}

fn tokens(pointer: &str) -> Result<Vec<String>> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(pointer.starts_with('/'), "field path must start with /");
    pointer[1..]
        .split('/')
        .map(|token| {
            let mut decoded = String::new();
            let mut chars = token.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    decoded.push(match chars.next() {
                        Some('0') => '~',
                        Some('1') => '/',
                        _ => bail!("invalid field path escape"),
                    });
                } else {
                    decoded.push(c);
                }
            }
            Ok(decoded)
        })
        .collect()
}

fn dictionary_key(token: &str) -> Result<Vec<u8>> {
    token
        .strip_prefix("@hex:")
        .map(|text| hex::decode(text).context("invalid hexadecimal dictionary key"))
        .unwrap_or_else(|| Ok(token.as_bytes().to_vec()))
}

fn list_index(token: &str) -> Result<usize> {
    ensure!(
        !token.is_empty()
            && token.bytes().all(|b| b.is_ascii_digit())
            && (token.len() == 1 || !token.starts_with('0')),
        "invalid list index"
    );
    token.parse().context("list index overflow")
}

fn query_mut<'a>(value: &'a mut Value, path: &[String]) -> Result<&'a mut Value> {
    let mut current = value;
    for token in path {
        current = match current {
            Value::Dict(dict) => dict
                .get_mut(&dictionary_key(token)?)
                .context("field dictionary key not found")?,
            Value::List(list) => list
                .get_mut(list_index(token)?)
                .context("field list index out of bounds")?,
            _ => bail!("field path traverses a scalar"),
        };
    }
    Ok(current)
}

pub fn replace_at(
    document: &Document,
    pointer: &str,
    json: &str,
    allow_hash_edit: bool,
) -> Result<Document> {
    let replacement = parse_typed_json(json)?;
    let mut value = document.value.clone();
    *query_mut(&mut value, &tokens(pointer)?)? = replacement;
    checked_document(document, value, allow_hash_edit)
}

pub fn remove_at(document: &Document, pointer: &str, allow_hash_edit: bool) -> Result<Document> {
    let mut path = tokens(pointer)?;
    let last = path.pop().context("the torrent root cannot be removed")?;
    let mut value = document.value.clone();
    match query_mut(&mut value, &path)? {
        Value::Dict(dict) => {
            dict.remove(&dictionary_key(&last)?)
                .context("field dictionary key not found")?;
        }
        Value::List(list) => {
            let index = list_index(&last)?;
            ensure!(index < list.len(), "field list index out of bounds");
            list.remove(index);
        }
        _ => bail!("parent field must be a list or dictionary"),
    }
    checked_document(document, value, allow_hash_edit)
}

pub fn add_at(
    document: &Document,
    parent: &str,
    key_or_index: &str,
    json: &str,
    allow_hash_edit: bool,
) -> Result<Document> {
    let added = parse_typed_json(json)?;
    let mut value = document.value.clone();
    match query_mut(&mut value, &tokens(parent)?)? {
        Value::Dict(dict) => {
            let key = dictionary_key(key_or_index)?;
            ensure!(
                !dict.contains_key(&key),
                "dictionary field already exists; select it to replace its value"
            );
            dict.insert(key, added);
        }
        Value::List(list) => {
            let index = if key_or_index == "-" {
                list.len()
            } else {
                list_index(key_or_index)?
            };
            ensure!(index <= list.len(), "new list index out of bounds");
            list.insert(index, added);
        }
        _ => bail!("parent field must be a list or dictionary"),
    }
    checked_document(document, value, allow_hash_edit)
}
