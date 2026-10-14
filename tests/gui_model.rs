#![cfg(feature = "gui")]

use std::{collections::BTreeMap, fs};

use rustide::{
    bencode::Value,
    create::{self, IoMode, Protocol},
    gui::model::{
        CreateForm, EditorForm, add_at, expand_path, field_json, parse_trackers, parse_typed_json,
        remove_at, replace_at, save_document, tree_rows, typed_json,
    },
    metadata::{self, Document},
};
use sha2::{Digest, Sha256};

fn fixture() -> Document {
    // Published SHA-1 vector for the one-byte payload "x".
    metadata::from_value(Value::Dict(BTreeMap::from([
        (
            b"info".to_vec(),
            Value::Dict(BTreeMap::from([
                (b"length".to_vec(), Value::Int(1)),
                (b"name".to_vec(), Value::Bytes(b"x".to_vec())),
                (b"piece length".to_vec(), Value::Int(16384)),
                (
                    b"pieces".to_vec(),
                    Value::Bytes(hex::decode("11f6ad8ec52a2984abaafd7c3b516503785c2072").unwrap()),
                ),
                (b"extension".to_vec(), Value::Bytes(vec![0, 255, 17])),
            ])),
        ),
        (
            vec![0, 255],
            Value::List(vec![Value::Int(-7), Value::Bytes(vec![0, 254])]),
        ),
        (b"created by".to_vec(), Value::Bytes(vec![255, 128])),
    ])))
    .unwrap()
}

#[test]
fn unchanged_form_preserves_absent_and_binary_metadata() {
    let document = fixture();
    let form = EditorForm::from_document(&document);
    assert!(!form.private);
    let edited = form.apply(&document).unwrap();
    assert_eq!(edited.value, document.value);
    assert_eq!(edited.raw_info, document.raw_info);
    assert!(edited.value.get(b"comment").is_none());
    assert!(edited.value.get(b"info").unwrap().get(b"private").is_none());
}

#[test]
fn basic_edit_preserves_unknown_fields_and_piece_bytes() {
    let document = fixture();
    let mut form = EditorForm::from_document(&document);
    form.comment = "A useful note".into();
    form.trackers =
        "https://a.example/announce\nudp://b.example:80/announce\n\nhttps://c.example/announce"
            .into();
    let edited = form.apply(&document).unwrap();
    assert_eq!(edited.raw_info, document.raw_info);
    assert_eq!(edited.value.get(&[0, 255]), document.value.get(&[0, 255]));
    assert_eq!(
        edited.value.get(b"created by"),
        document.value.get(b"created by")
    );
    assert_eq!(
        metadata::query(&edited.value, "/info/extension").unwrap(),
        metadata::query(&document.value, "/info/extension").unwrap()
    );
    let tiers = edited
        .value
        .get(b"announce-list")
        .unwrap()
        .as_list()
        .unwrap();
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers[0].as_list().unwrap().len(), 2);
}

#[test]
fn clearing_fields_removes_only_requested_fields() {
    let document = fixture();
    let mut form = EditorForm::from_document(&document);
    form.comment = "note".into();
    form.source = "source".into();
    form.creation_date = "12345".into();
    let document = form.apply(&document).unwrap();
    let mut form = EditorForm::from_document(&document);
    form.comment.clear();
    form.source.clear();
    form.creation_date.clear();
    let edited = form.apply(&document).unwrap();
    assert!(edited.value.get(b"comment").is_none());
    assert!(edited.value.get(b"creation date").is_none());
    assert!(edited.value.get(b"info").unwrap().get(b"source").is_none());
    assert_eq!(
        edited.value.get(b"created by"),
        document.value.get(b"created by")
    );
    assert_eq!(
        metadata::query(&edited.value, "/info/pieces").unwrap(),
        metadata::query(&document.value, "/info/pieces").unwrap()
    );
}

#[test]
fn typed_json_round_trips_binary_keys_and_reserved_fields() {
    let value = Value::Dict(BTreeMap::from([
        (vec![255, 0], Value::Bytes(vec![0, 255, 128])),
        (b"$bytes".to_vec(), Value::Bytes(b"00".to_vec())),
        (
            b"$dict".to_vec(),
            Value::List(vec![Value::Int(i64::MIN), Value::Int(i64::MAX)]),
        ),
        (
            b"nested".to_vec(),
            Value::Dict(BTreeMap::from([(
                b"$bytes".to_vec(),
                Value::Bytes(b"literal".to_vec()),
            )])),
        ),
    ]));
    assert_eq!(parse_typed_json(&typed_json(&value)).unwrap(), value);
    assert_eq!(
        parse_typed_json(r#"{"$bytes":"00ff"}"#).unwrap(),
        Value::Bytes(vec![0, 255])
    );
    assert_eq!(
        parse_typed_json(r#"[1,"text",{}]"#).unwrap(),
        Value::List(vec![
            Value::Int(1),
            Value::Bytes(b"text".to_vec()),
            Value::Dict(BTreeMap::new())
        ])
    );
}

#[test]
fn typed_json_rejects_invalid_values_and_resource_abuse() {
    for text in [
        "true",
        "false",
        "null",
        "1.5",
        "9223372036854775808",
        r#"{"$bytes":"fg"}"#,
        r#"{"$bytes":1}"#,
        r#"{"$dict":[{"key":"ff","value":1},{"key":"ff","value":2}]}"#,
        r#"{"same":1,"same":2}"#,
        r#"{"nested":{"same":1,"same":2}}"#,
    ] {
        assert!(parse_typed_json(text).is_err(), "accepted {text}");
    }
    let deep = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    assert!(parse_typed_json(&deep).is_err());
    let huge = format!("\"{}\"", "a".repeat(8 * 1024 * 1024));
    assert!(parse_typed_json(&huge).is_err());
    let many = format!("[{}0]", "0,".repeat(100_000));
    assert!(parse_typed_json(&many).is_err());
    assert!(field_json(&Value::Bytes(vec![0; 4 * 1024 * 1024 + 1])).is_err());
    assert_eq!(
        parse_typed_json(&field_json(&fixture().value).unwrap()).unwrap(),
        fixture().value
    );
}

#[test]
fn advanced_edits_keep_types_and_address_binary_or_escaped_keys() {
    let original = fixture();
    let document = add_at(&original, "", "a/b~", r#"[1,{"$bytes":"00ff"}]"#, false).unwrap();
    let document = replace_at(&document, "/a~1b~0/0", "-8", false).unwrap();
    let document = add_at(&document, "/a~1b~0", "-", r#""tail""#, false).unwrap();
    assert_eq!(
        metadata::query(&document.value, "/a~1b~0/0").unwrap(),
        &Value::Int(-8)
    );
    assert_eq!(
        metadata::query(&document.value, "/a~1b~0/1").unwrap(),
        &Value::Bytes(vec![0, 255])
    );
    let document = replace_at(&document, "/@hex:00ff/0", "11", false).unwrap();
    assert_eq!(
        metadata::query(&document.value, "/@hex:00ff/0").unwrap(),
        &Value::Int(11)
    );
    let document = remove_at(&document, "/a~1b~0/2", false).unwrap();
    assert_eq!(
        metadata::query(&document.value, "/a~1b~0")
            .unwrap()
            .as_list()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(original.raw_info, document.raw_info);
    assert!(add_at(&document, "", "info", "{}", false).is_err());
    assert!(replace_at(&document, "/a~2b", "0", false).is_err());
    assert!(remove_at(&document, "/a~1b~0/01", false).is_err());
    assert!(remove_at(&document, "", false).is_err());
}

#[test]
fn parent_replacement_cannot_bypass_piece_hash_protection() {
    let document = fixture();
    let fake_hash = format!(r#"{{"$bytes":"{}"}}"#, "00".repeat(20));
    assert!(replace_at(&document, "/info/pieces", &fake_hash, false).is_err());
    let mut info = document.value.get(b"info").unwrap().clone();
    info.as_dict_mut()
        .unwrap()
        .insert(b"pieces".to_vec(), Value::Bytes(vec![0; 20]));
    assert!(replace_at(&document, "/info", &typed_json(&info), false).is_err());
    let mut root = document.value.clone();
    root.as_dict_mut().unwrap().insert(b"info".to_vec(), info);
    assert!(replace_at(&document, "", &typed_json(&root), false).is_err());
    let edited = replace_at(&document, "/info/pieces", &fake_hash, true).unwrap();
    assert_eq!(
        metadata::query(&edited.value, "/info/pieces").unwrap(),
        &Value::Bytes(vec![0; 20])
    );
    assert_eq!(
        metadata::query(&document.value, "/info/pieces")
            .unwrap()
            .as_bytes()
            .unwrap(),
        hex::decode("11f6ad8ec52a2984abaafd7c3b516503785c2072").unwrap()
    );
    let rows = tree_rows(&document.value);
    assert!(
        rows.iter()
            .find(|row| row.pointer == "/info/pieces")
            .unwrap()
            .protected
    );
    assert!(
        rows.iter()
            .find(|row| row.pointer == "/info")
            .unwrap()
            .protected
    );
    assert!(
        !rows
            .iter()
            .find(|row| row.pointer == "/info/name")
            .unwrap()
            .protected
    );
}

#[test]
fn replacing_v2_ancestors_requires_explicit_hash_editing() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("data");
    let form = CreateForm {
        input: input.display().to_string(),
        output: temporary.path().join("new.torrent").display().to_string(),
        protocol: Protocol::V2,
        piece_length: Some(16 * 1024),
        io: IoMode::Buffered,
        ..Default::default()
    };
    fs::write(&input, vec![7; 40_003]).unwrap();
    let before =
        metadata::from_value(create::create(&form.options().unwrap()).unwrap().value).unwrap();
    fs::write(&input, vec![8; 40_003]).unwrap();
    let after =
        metadata::from_value(create::create(&form.options().unwrap()).unwrap().value).unwrap();
    assert!(replace_at(&before, "", &typed_json(&after.value), false).is_err());
    assert!(remove_at(&before, "/piece layers", false).is_err());
    let edited = replace_at(&before, "", &typed_json(&after.value), true).unwrap();
    assert_eq!(
        metadata::verify(&edited, &input, Some(1)).unwrap()["valid"],
        true
    );
}

#[test]
fn nested_insertion_cannot_exceed_the_saved_bencode_depth_limit() {
    let mut value = fixture().value;
    let mut nested = Value::Int(0);
    for _ in 0..100 {
        nested = Value::List(vec![nested]);
    }
    value
        .as_dict_mut()
        .unwrap()
        .insert(b"unknown".to_vec(), nested);
    let document = metadata::from_value(value).unwrap();
    let pointer = format!("/unknown{}", "/0".repeat(100));
    let replacement = format!("{}0{}", "[".repeat(64), "]".repeat(64));
    assert!(parse_typed_json(&replacement).is_ok());
    assert!(replace_at(&document, &pointer, &replacement, false).is_err());
    assert_eq!(
        metadata::query(&document.value, &pointer).unwrap(),
        &Value::Int(0)
    );
}

#[test]
fn tree_pointers_handle_keys_that_look_like_binary_addresses() {
    let document = add_at(&fixture(), "", "@hex:406865783a6666", "3", false).unwrap();
    let row = tree_rows(&document.value)
        .into_iter()
        .find(|row| row.label == "@hex:ff")
        .unwrap();
    assert_eq!(
        metadata::query(&document.value, &row.pointer).unwrap(),
        &Value::Int(3)
    );
    assert!(row.preview.len() < 128);
}

#[test]
fn tree_view_bounds_large_keys_deep_paths_and_total_text_without_mutation() {
    let original = fixture();
    let mut value = original.value.clone();
    let root = value.as_dict_mut().unwrap();
    root.insert(vec![b'a'; 4097], Value::Bytes(vec![9; 1024 * 1024]));
    root.insert(vec![255; 4096], Value::Int(1));
    root.insert(vec![b'/'; 4096], Value::Int(2));
    let mut nested = Value::Int(3);
    for _ in 0..20 {
        nested = Value::Dict(BTreeMap::from([(vec![b'k'; 4096], nested)]));
    }
    root.insert(b"deep".to_vec(), nested);
    let many = (0..10_000)
        .map(|index| {
            let key = format!("{index:05}{}", "x".repeat(800)).into_bytes();
            (key, Value::Int(index))
        })
        .collect();
    root.insert(b"many".to_vec(), Value::Dict(many));
    let mut text = "é".repeat(1024 * 1024).into_bytes();
    text.push(255);
    root.insert(b"long text".to_vec(), Value::Bytes(text));
    let document = metadata::from_value(value).unwrap();
    let unchanged = document.value.clone();
    let rows = tree_rows(&document.value);
    assert!(rows[0].preview.ends_with("large fields omitted from tree"));
    assert!(rows.len() <= 100_000);
    assert!(rows.iter().all(|row| row.pointer.len() <= 8192));
    assert!(
        rows.iter()
            .map(|row| row.pointer.len() + row.label.len() + row.preview.len())
            .sum::<usize>()
            <= 8 * 1024 * 1024
    );
    for row in &rows {
        assert!(metadata::query(&document.value, &row.pointer).is_ok());
    }
    let preview = &rows
        .iter()
        .find(|row| row.pointer == "/long text")
        .unwrap()
        .preview;
    assert!(preview.len() < 400);
    assert!(preview.contains('…'));
    assert_eq!(document.value, unchanged);
    assert_eq!(document.raw_info, original.raw_info);
    assert_eq!(
        metadata::query(&document.value, "/info/pieces").unwrap(),
        metadata::query(&original.value, "/info/pieces").unwrap()
    );
}

#[test]
fn tracker_tiers_and_dht_hosts_are_validated_and_preserved() {
    let tiers = parse_trackers("\nhttps://a.example/announce\nhttps://b.example/announce\n\n\nudp://c.example:80/announce\n").unwrap();
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers[0].len(), 2);
    assert!(parse_trackers("ftp://example/announce").is_err());
    assert!(parse_trackers("https://example/announce https://b.example/announce").is_err());
    let mut form = EditorForm::from_document(&fixture());
    form.dht_nodes = "router.example:6881\n[2001:db8::1]:6882".into();
    let document = form.apply(&fixture()).unwrap();
    assert_eq!(
        EditorForm::from_document(&document).dht_nodes,
        form.dht_nodes
    );
    form.private = true;
    assert!(form.apply(&document).is_err());
    form.dht_nodes.clear();
    assert!(form.apply(&document).unwrap().value.get(b"nodes").is_none());
}

#[test]
fn unrelated_edits_preserve_legacy_private_dht_nodes() {
    let mut value = fixture().value;
    value
        .get_mut(b"info")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .insert(b"private".to_vec(), Value::Int(1));
    value.as_dict_mut().unwrap().insert(
        b"nodes".to_vec(),
        Value::List(vec![Value::List(vec![
            Value::Bytes(b"router.example".to_vec()),
            Value::Int(6881),
        ])]),
    );
    let document = metadata::from_value(value).unwrap();
    let mut form = EditorForm::from_document(&document);
    form.comment = "new note".into();
    let edited = form.apply(&document).unwrap();
    assert_eq!(edited.value.get(b"nodes"), document.value.get(b"nodes"));
    assert_eq!(edited.raw_info, document.raw_info);
    form.dht_nodes = "another.example:6881".into();
    assert!(form.apply(&document).is_err());
}

#[test]
fn create_defaults_and_invalid_paths_are_safe() {
    let temporary = tempfile::tempdir().unwrap();
    let payload = temporary.path().join("payload");
    fs::write(&payload, b"x").unwrap();
    let mut form = CreateForm {
        input: payload.display().to_string(),
        output: temporary.path().join("new.torrent").display().to_string(),
        ..Default::default()
    };
    let options = form.options().unwrap();
    assert_eq!(options.piece_length, None);
    assert_eq!(options.private, Some(true));
    assert_eq!(options.threads, None);
    form.output = payload.display().to_string();
    assert!(form.options().is_err());
    form.output = temporary
        .path()
        .join("missing/new.torrent")
        .display()
        .to_string();
    assert!(form.options().is_err());
    form.output = temporary.path().display().to_string();
    assert!(form.options().is_err());
    form.output = temporary.path().join("new.torrent").display().to_string();
    form.workers = "0".into();
    assert!(form.options().is_err());
    form.workers = "4097".into();
    assert!(form.options().is_err());
    form.workers = "4096".into();
    assert_eq!(form.options().unwrap().threads, Some(4096));
    form.workers = "auto".into();
    form.private = true;
    form.dht_nodes = "router.example:6881".into();
    assert!(form.options().is_err());
    form.dht_nodes.clear();
    assert_eq!(form.options().unwrap().private, Some(true));
    form.piece_length = Some(64 * 1024 * 1024);
    assert_eq!(form.options().unwrap().piece_length, form.piece_length);
    form.piece_length = Some(12345);
    assert!(form.options().is_err());
    assert!(expand_path("").is_err());
    assert!(expand_path("~another/payload").is_err());
    assert!(expand_path("bad\0path").is_err());
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME") {
        assert_eq!(
            expand_path("~/payload").unwrap(),
            std::path::PathBuf::from(home).join("payload")
        );
    }
}

#[cfg(unix)]
#[test]
fn output_aliases_and_links_cannot_replace_payload() {
    use std::os::unix::fs::symlink;
    let temporary = tempfile::tempdir().unwrap();
    let payload = temporary.path().join("payload");
    fs::write(&payload, b"x").unwrap();
    let hardlink = temporary.path().join("hardlink");
    fs::hard_link(&payload, &hardlink).unwrap();
    let form = CreateForm {
        input: payload.display().to_string(),
        output: hardlink.display().to_string(),
        ..Default::default()
    };
    assert!(form.options().is_err());
    let alias = temporary.path().join("alias");
    symlink(&payload, &alias).unwrap();
    let form = CreateForm {
        output: alias.display().to_string(),
        ..form
    };
    assert!(form.options().is_err());
    assert!(save_document(&fixture(), &alias, true).is_err());
    assert_eq!(fs::read(payload).unwrap(), b"x");
}

#[test]
fn atomic_save_requires_explicit_overwrite_and_rejects_invalid_document() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("saved.torrent");
    let document = fixture();
    save_document(&document, &path, false).unwrap();
    assert_eq!(metadata::load(&path).unwrap().value, document.value);
    let original = fs::read(&path).unwrap();
    let mut form = EditorForm::from_document(&document);
    form.comment = "changed".into();
    let edited = form.apply(&document).unwrap();
    assert!(save_document(&edited, &path, false).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    save_document(&edited, &path, true).unwrap();
    assert_eq!(metadata::load(&path).unwrap().value, edited.value);
    let invalid = Document {
        value: Value::Int(0),
        raw_info: Vec::new(),
    };
    assert!(save_document(&invalid, &path, true).is_err());
    assert_eq!(metadata::load(&path).unwrap().value, edited.value);
}

#[test]
fn all_protocols_create_empty_files_single_files_and_directory_boundaries() {
    let temporary = tempfile::tempdir().unwrap();
    let empty = temporary.path().join("empty");
    let single = temporary.path().join("single");
    let directory = temporary.path().join("folder");
    let data = (0..40_003).map(|n| (n % 251) as u8).collect::<Vec<_>>();
    fs::write(&empty, b"").unwrap();
    fs::write(&single, &data).unwrap();
    fs::create_dir(&directory).unwrap();
    fs::create_dir(directory.join("nested")).unwrap();
    fs::write(directory.join("a"), &data[..17_000]).unwrap();
    fs::write(directory.join("nested/b"), &data[17_000..]).unwrap();
    fs::write(directory.join("nested/empty"), b"").unwrap();
    for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
        for payload in [&empty, &single, &directory] {
            let form = CreateForm {
                input: payload.display().to_string(),
                output: temporary
                    .path()
                    .join("created.torrent")
                    .display()
                    .to_string(),
                protocol,
                piece_length: Some(16 * 1024),
                workers: "2".into(),
                io: IoMode::Buffered,
                ..Default::default()
            };
            let document =
                metadata::from_value(create::create(&form.options().unwrap()).unwrap().value)
                    .unwrap();
            assert_eq!(
                metadata::verify(&document, payload, Some(2)).unwrap()["valid"],
                true
            );
            if payload == &single && protocol != Protocol::V2 {
                let expected = data
                    .chunks(16 * 1024)
                    .flat_map(|chunk| sha1::Sha1::digest(chunk).to_vec())
                    .collect::<Vec<_>>();
                assert_eq!(
                    metadata::query(&document.value, "/info/pieces")
                        .unwrap()
                        .as_bytes()
                        .unwrap(),
                    expected
                );
            }
            if payload == &directory && protocol == Protocol::V1 {
                let expected = data
                    .chunks(16 * 1024)
                    .flat_map(|chunk| sha1::Sha1::digest(chunk).to_vec())
                    .collect::<Vec<_>>();
                assert_eq!(
                    metadata::query(&document.value, "/info/pieces")
                        .unwrap()
                        .as_bytes()
                        .unwrap(),
                    expected
                );
            }
            if payload == &single && protocol != Protocol::V1 {
                let leaves = data
                    .chunks(16 * 1024)
                    .map(|chunk| <[u8; 32]>::from(Sha256::digest(chunk)))
                    .collect::<Vec<_>>();
                let left = Sha256::digest([leaves[0].as_slice(), leaves[1].as_slice()].concat());
                let right = Sha256::digest([leaves[2].as_slice(), &[0; 32]].concat());
                let root = Sha256::digest([left.as_slice(), right.as_slice()].concat());
                assert_eq!(
                    metadata::query(&document.value, "/info/file tree/single//pieces root")
                        .unwrap()
                        .as_bytes()
                        .unwrap(),
                    root.as_slice()
                );
            }
        }
    }
}

#[test]
fn hybrid_single_file_rename_preserves_both_hashes_and_payload() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("before");
    fs::write(&input, b"abc").unwrap();
    let form = CreateForm {
        input: input.display().to_string(),
        output: temporary.path().join("new.torrent").display().to_string(),
        protocol: Protocol::Hybrid,
        io: IoMode::Buffered,
        ..Default::default()
    };
    let document =
        metadata::from_value(create::create(&form.options().unwrap()).unwrap().value).unwrap();
    let pieces = metadata::query(&document.value, "/info/pieces")
        .unwrap()
        .clone();
    let root = metadata::query(&document.value, "/info/file tree/before//pieces root")
        .unwrap()
        .clone();
    let mut form = EditorForm::from_document(&document);
    form.name = "after".into();
    let edited = form.apply(&document).unwrap();
    assert_eq!(
        metadata::query(&edited.value, "/info/pieces").unwrap(),
        &pieces
    );
    assert_eq!(
        metadata::query(&edited.value, "/info/file tree/after//pieces root").unwrap(),
        &root
    );
    assert_eq!(root.as_bytes().unwrap(), Sha256::digest(b"abc").as_slice());
    assert_eq!(
        metadata::verify(&edited, &input, Some(1)).unwrap()["valid"],
        true
    );
}
