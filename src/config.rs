use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub profiles: BTreeMap<String, Profile>,
    #[serde(alias = "tracker-groups")]
    pub tracker_groups: BTreeMap<String, Vec<String>>,
    #[serde(alias = "tracker-parameters")]
    pub tracker_parameters: BTreeMap<String, BTreeMap<String, String>>,
    pub trackers: Vec<Tracker>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub command: String,
    pub options: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Tracker {
    pub name: String,
    pub abbreviation: String,
    #[serde(alias = "announce-url")]
    pub announce_url: String,
    pub private: bool,
}

pub type AnnounceSettings = (Vec<Vec<String>>, Option<bool>, Option<String>);

fn default_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("rustide/config.json"));
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
            .map(|p| p.join("rustide/config.json"))
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let explicit = path.is_some();
        let default = default_path();
        let Some(path) = path.or(default.as_deref()) else {
            return Ok(Self::default());
        };
        if !path.exists() && !explicit {
            return Ok(Self::default());
        }
        let mut bytes = Vec::new();
        File::open(path)
            .with_context(|| format!("cannot read configuration {}", path.display()))?
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "configuration exceeds 4 MiB"
        );
        serde_json::from_slice(&bytes).context("configuration must be valid JSON")
    }

    pub fn announces(&self, values: &[String], groups: &[String]) -> Result<AnnounceSettings> {
        let mut tiers = Vec::new();
        let mut private = None;
        let mut source = None;
        let mut all = values.to_vec();
        for group in groups {
            all.extend(
                self.tracker_groups
                    .get(group)
                    .with_context(|| format!("unknown announce group: {group}"))?
                    .iter()
                    .cloned(),
            );
        }
        for value in all {
            let grouped = value
                .trim()
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'));
            let tokens: Vec<_> =
                grouped.map_or_else(|| vec![value.as_str()], |s| s.split_whitespace().collect());
            let mut tier = Vec::new();
            for token in tokens {
                if let Some(tracker) = self.trackers.iter().find(|t| {
                    t.name.eq_ignore_ascii_case(token)
                        || (!t.abbreviation.is_empty()
                            && t.abbreviation.eq_ignore_ascii_case(token))
                }) {
                    let mut url = tracker.announce_url.clone();
                    while let Some(begin) = url.find('{') {
                        let end = url[begin..]
                            .find('}')
                            .map(|i| begin + i)
                            .context("unclosed announce parameter")?;
                        let key = &url[begin + 1..end];
                        let replacement = self
                            .tracker_parameters
                            .get(&tracker.name)
                            .and_then(|p| p.get(key))
                            .with_context(|| {
                                format!("missing announce parameter {key} for {}", tracker.name)
                            })?;
                        ensure!(
                            !replacement.contains(['{', '}']),
                            "announce parameter contains braces"
                        );
                        url.replace_range(begin..=end, replacement);
                    }
                    tier.push(url);
                    private = Some(private.unwrap_or(false) || tracker.private);
                    if tracker.private && source.is_none() {
                        source = Some(tracker.name.clone());
                    }
                } else {
                    tier.push(token.to_owned());
                }
            }
            for url in &tier {
                ensure!(
                    url.starts_with("http://")
                        || url.starts_with("https://")
                        || url.starts_with("udp://")
                        || url.starts_with("ws://")
                        || url.starts_with("wss://"),
                    "announce URL needs a supported scheme"
                );
            }
            if !tier.is_empty() {
                tiers.push(tier);
            }
        }
        Ok((tiers, private, source))
    }
}

pub fn expand_profile(args: Vec<OsString>) -> Result<Vec<OsString>> {
    expand_profile_except(args, &[])
}

pub fn expand_profile_except(mut args: Vec<OsString>, skipped: &[&str]) -> Result<Vec<OsString>> {
    let text: Vec<_> = args
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let argument = |names: &[&str]| -> Option<String> {
        for (index, value) in text.iter().enumerate().skip(1) {
            if names.contains(&value.as_str()) {
                return text.get(index + 1).cloned();
            }
            for name in names {
                if let Some(value) = value.strip_prefix(&format!("{name}=")) {
                    return Some(value.to_owned());
                }
                if *name == "-P"
                    && let Some(value) = value.strip_prefix("-P").filter(|value| !value.is_empty())
                {
                    return Some(value.to_owned());
                }
            }
        }
        None
    };
    let Some(name) = argument(&["--profile", "-P"]) else {
        return Ok(args);
    };
    if text
        .iter()
        .any(|value| ["--help", "-h", "--version", "-V"].contains(&value.as_str()))
    {
        return Ok(args);
    }
    let config_path = argument(&["--config"]);
    let config = Config::load(config_path.as_deref().map(Path::new))?;
    let profile = config
        .profiles
        .get(&name)
        .with_context(|| format!("unknown profile: {name}"))?;
    let command = text.get(1).map(String::as_str).unwrap_or("create");
    if !profile.command.is_empty() {
        ensure!(
            profile.command == command,
            "profile belongs to {}",
            profile.command
        );
    }
    for (key, value) in &profile.options {
        let normalized = key.replace('_', "-");
        if skipped.contains(&normalized.as_str()) {
            continue;
        }
        let opposite = match normalized.as_str() {
            "created-by" => Some("--no-created-by"),
            "creation-date" => Some("--no-creation-date"),
            "no-created-by" | "set-created-by" => Some("--created-by"),
            "no-creation-date" | "set-creation-date" => Some("--creation-date"),
            "cross-seed" => Some("--no-cross-seed"),
            "no-cross-seed" => Some("--cross-seed"),
            _ => None,
        };
        if opposite.is_some_and(|flag| {
            text.iter()
                .any(|value| value == flag || value.starts_with(&format!("{flag}=")))
        }) {
            continue;
        }
        let flag = format!("--{normalized}");
        let short = match normalized.as_str() {
            "protocol" => Some("-v"),
            "output" => Some("-o"),
            "announce" => Some("-a"),
            "announce-group" => Some("-g"),
            "private" => Some("-p"),
            "piece-size" => Some("-l"),
            "threads" => Some("-t"),
            "source" => Some("-s"),
            "name" => Some("-n"),
            "comment" => Some("-c"),
            "web-seed" => Some("-w"),
            "dht-node" => Some("-d"),
            "list-mode" => Some("-m"),
            _ => None,
        };
        if text.iter().any(|v| {
            v == &flag
                || v.starts_with(&format!("{flag}="))
                || short.is_some_and(|s| v == s || (v.starts_with(s) && v.len() > s.len()))
        }) {
            continue;
        }
        if matches!(normalized.as_str(), "set-created-by" | "set-creation-date") {
            if value == &serde_json::Value::Bool(false) {
                args.push(format!("--no-{}", normalized.trim_start_matches("set-")).into());
            }
            continue;
        }
        match value {
            serde_json::Value::Bool(enabled) if normalized != "private" => {
                if *enabled {
                    args.push(flag.into());
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    args.push(flag.clone().into());
                    args.push(scalar(value)?.into());
                }
            }
            _ => {
                args.push(flag.into());
                args.push(scalar(value)?.into());
            }
        }
    }
    Ok(args)
}

fn scalar(value: &serde_json::Value) -> Result<String> {
    match value {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        _ => bail!("profile values must be strings, numbers, booleans, or arrays of those"),
    }
}
