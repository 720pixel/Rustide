use std::{collections::BTreeMap, fs};

use rustide::{
    bencode::{self, Limits, Value},
    create::{self, CreateOptions, Protocol},
    merkle::BLOCK_LENGTH,
    metadata,
};
use sha1::Sha1;
use sha2::{Digest, Sha256};

fn reference_root(data: &[u8]) -> Option<[u8; 32]> {
    if data.is_empty() {
        return None;
    }
    let mut layer: Vec<[u8; 32]> = data
        .chunks(BLOCK_LENGTH)
        .map(|block| Sha256::digest(block).into())
        .collect();
    layer.resize(layer.len().next_power_of_two(), [0; 32]);
    while layer.len() > 1 {
        layer = layer
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| Sha256::digest(pair.concat()).into())
            .collect();
    }
    Some(layer[0])
}

#[test]
fn piece_size_changes_preserve_independently_known_file_roots() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("data");
    let bytes: Vec<u8> = (0..49275).map(|i| (i % 251) as u8).collect();
    fs::write(&path, &bytes).unwrap();
    for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
        for piece_length in [BLOCK_LENGTH, 2 * BLOCK_LENGTH, 4 * BLOCK_LENGTH] {
            let result = create::create(&CreateOptions {
                target: path.clone(),
                protocol,
                piece_length: Some(piece_length),
                threads: Some(2),
                ..CreateOptions::default()
            })
            .unwrap();
            let document = metadata::from_value(result.value).unwrap();
            assert_eq!(
                metadata::verify(&document, &path, Some(2)).unwrap()["valid"],
                true
            );
            if protocol != Protocol::V2 {
                let expected: Vec<u8> = bytes.chunks(piece_length).flat_map(Sha1::digest).collect();
                assert_eq!(
                    metadata::query(&document.value, "/info/pieces")
                        .unwrap()
                        .as_bytes()
                        .unwrap(),
                    expected
                );
            }
            if protocol != Protocol::V1 {
                let root = metadata::files(&document).unwrap()[0].pieces_root.unwrap();
                assert_eq!(
                    hex::encode(root),
                    "c116a73e0a4cdcc954d1df6d70cbb5cd9b7057c3c770b84e610029c618042dd9"
                );
            }
        }
    }
}

#[test]
fn lazy_mapping_and_zero_length_boundaries_match_independent_hashes() {
    let temp = tempfile::tempdir().unwrap();
    let mut payloads = BTreeMap::new();
    for index in 0..270 {
        let name = format!("{index:03}");
        let length = if index % 17 == 0 {
            0
        } else {
            (index * 37) % 401 + 1
        };
        let bytes: Vec<u8> = (0..length)
            .map(|i| ((index + i * 11) % 251) as u8)
            .collect();
        fs::write(temp.path().join(&name), &bytes).unwrap();
        payloads.insert(name.into_bytes(), bytes);
    }
    for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
        for piece_length in [BLOCK_LENGTH, 2 * BLOCK_LENGTH] {
            let result = create::create(&CreateOptions {
                target: temp.path().to_path_buf(),
                protocol,
                piece_length: Some(piece_length),
                threads: Some(3),
                ..CreateOptions::default()
            })
            .unwrap();
            let document = metadata::from_value(result.value).unwrap();
            let files = metadata::files(&document).unwrap();
            assert_eq!(files.iter().filter(|f| !f.padding).count(), 270);
            let mut concatenated = Vec::new();
            for file in files {
                if file.padding {
                    concatenated.resize(concatenated.len() + file.length as usize, 0);
                } else {
                    let bytes = &payloads[&file.components[0]];
                    concatenated.extend_from_slice(bytes);
                    if protocol != Protocol::V1 {
                        assert_eq!(file.pieces_root, reference_root(bytes));
                    }
                }
            }
            if protocol != Protocol::V2 {
                let expected: Vec<u8> = concatenated
                    .chunks(piece_length)
                    .flat_map(Sha1::digest)
                    .collect();
                assert_eq!(
                    metadata::query(&document.value, "/info/pieces")
                        .unwrap()
                        .as_bytes()
                        .unwrap(),
                    expected
                );
            }
            assert_eq!(
                metadata::verify(&document, temp.path(), Some(2)).unwrap()["valid"],
                true
            );
        }
    }
}

#[test]
fn filtering_and_existing_output_do_not_change_payload_selection() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("visible"), b"x").unwrap();
    fs::write(temp.path().join(".secret"), b"s").unwrap();
    fs::create_dir(temp.path().join(".hidden")).unwrap();
    fs::write(temp.path().join(".hidden/selected"), b"h").unwrap();
    let output = temp.path().join("out.torrent");
    fs::write(&output, b"existing metadata").unwrap();
    let mut options = CreateOptions {
        target: temp.path().to_path_buf(),
        output: Some(output),
        ..CreateOptions::default()
    };
    let selected = create::scan(&options).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].components, vec![b"visible".to_vec()]);
    options.include_hidden = true;
    assert_eq!(create::scan(&options).unwrap().len(), 3);
    options.include_hidden = false;
    options.include = vec!["selected$".into()];
    let selected = create::scan(&options).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0].components,
        vec![b".hidden".to_vec(), b"selected".to_vec()]
    );
}

fn multifile() -> Value {
    let path = |name: &[u8]| {
        Value::Dict(BTreeMap::from([
            (b"length".to_vec(), Value::Int(1)),
            (
                b"path".to_vec(),
                Value::List(vec![Value::Bytes(name.to_vec())]),
            ),
        ]))
    };
    Value::Dict(BTreeMap::from([(
        b"info".to_vec(),
        Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::Bytes(b"folder".to_vec())),
            (b"piece length".to_vec(), Value::Int(16384)),
            (
                b"pieces".to_vec(),
                Value::Bytes(Sha1::digest(b"ab").to_vec()),
            ),
            (b"files".to_vec(), Value::List(vec![path(b"a"), path(b"b")])),
        ])),
    )]))
}

#[test]
fn malformed_paths_lengths_and_duplicate_files_are_rejected() {
    for component in [
        b"..".as_slice(),
        b".",
        b"",
        b"/root",
        b"a/b",
        b"a\\b",
        b"C:drive",
        b"a\0b",
    ] {
        let mut value = multifile();
        let Value::List(files) = value.get_mut(b"info").unwrap().get_mut(b"files").unwrap() else {
            unreachable!()
        };
        files[0].as_dict_mut().unwrap().insert(
            b"path".to_vec(),
            Value::List(vec![Value::Bytes(component.to_vec())]),
        );
        assert!(
            metadata::from_value(value).is_err(),
            "accepted component {component:?}"
        );
    }
    let mut value = multifile();
    let Value::List(files) = value.get_mut(b"info").unwrap().get_mut(b"files").unwrap() else {
        unreachable!()
    };
    files[1] = files[0].clone();
    assert!(metadata::from_value(value).is_err());
    let mut value = multifile();
    let Value::List(files) = value.get_mut(b"info").unwrap().get_mut(b"files").unwrap() else {
        unreachable!()
    };
    files[0]
        .as_dict_mut()
        .unwrap()
        .insert(b"length".to_vec(), Value::Int(-1));
    assert!(metadata::from_value(value).is_err());
    let mut value = multifile();
    let Value::List(files) = value.get_mut(b"info").unwrap().get_mut(b"files").unwrap() else {
        unreachable!()
    };
    files[1].as_dict_mut().unwrap().insert(
        b"path".to_vec(),
        Value::List(vec![
            Value::Bytes(b"a".to_vec()),
            Value::Bytes(b"child".to_vec()),
        ]),
    );
    assert!(metadata::from_value(value).is_err());
}

#[test]
fn bencode_resource_limits_fail_before_deep_recursion_or_large_allocation() {
    let mut nested = vec![b'l'; 10000];
    nested.extend(vec![b'e'; 10000]);
    assert!(bencode::parse(&nested).is_err());
    assert!(bencode::parse(b"999999999999999999999999999999999999999999:").is_err());
    assert!(bencode::parse(b"18446744073709551615:x").is_err());
    assert!(
        bencode::parse_with_limits(
            b"li0ei0ei0ee",
            Limits {
                max_bytes: 100,
                max_depth: 10,
                max_values: 3
            }
        )
        .is_err()
    );
    assert!(
        bencode::parse_with_limits(
            b"4:data",
            Limits {
                max_bytes: 5,
                ..Limits::default()
            }
        )
        .is_err()
    );
    for bytes in [
        b"d1:ai0e1:ai1ee".as_slice(),
        b"d1:bi0e1:ai1ee",
        b"i-0e",
        b"i01e",
        b"00:",
        b"i1ee",
    ] {
        assert!(bencode::parse(bytes).is_err());
    }
}

#[cfg(unix)]
#[test]
fn verification_and_padding_refuse_symlink_escape() {
    use std::os::unix::fs::symlink;
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), b"a").unwrap();
    fs::write(source.join("b"), b"b").unwrap();
    let result = create::create(&CreateOptions {
        target: source.clone(),
        protocol: Protocol::Hybrid,
        threads: Some(1),
        ..CreateOptions::default()
    })
    .unwrap();
    let document = metadata::from_value(result.value).unwrap();
    let outside = parent.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, source.join(".pad")).unwrap();
    assert!(metadata::pad(&document, &source).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_file(source.join("a")).unwrap();
    fs::write(outside.join("a"), b"a").unwrap();
    symlink(outside.join("a"), source.join("a")).unwrap();
    assert!(metadata::verify(&document, &source, Some(1)).is_err());
}

#[test]
fn library_creation_rejects_nonstandard_similar_hash_lengths() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("payload");
    fs::write(&path, b"abc").unwrap();
    for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
        for length in [0, 19, 21, 32] {
            let result = create::create(&CreateOptions {
                target: path.clone(),
                protocol,
                threads: Some(1),
                similar: vec![vec![7; length]],
                ..CreateOptions::default()
            });
            assert!(
                result.is_err(),
                "accepted a {length}-byte similar hash for {protocol:?}"
            );
        }
        let result = create::create(&CreateOptions {
            target: path.clone(),
            protocol,
            threads: Some(1),
            similar: vec![vec![7; 20]],
            ..CreateOptions::default()
        })
        .unwrap();
        let document = metadata::from_value(result.value).unwrap();
        let similar = metadata::query(&document.value, "/info/similar")
            .unwrap()
            .as_list()
            .unwrap();
        assert_eq!(similar[0].as_bytes().unwrap(), &[7; 20]);
    }
}

#[test]
fn buffered_and_mapped_creation_match_across_file_and_piece_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    for (index, length) in [
        0,
        1,
        BLOCK_LENGTH - 1,
        BLOCK_LENGTH + 7,
        0,
        2 * BLOCK_LENGTH + 11,
    ]
    .into_iter()
    .enumerate()
    {
        let data: Vec<u8> = (0..length)
            .map(|offset| ((offset * 13 + index) % 251) as u8)
            .collect();
        fs::write(temp.path().join(format!("{index:02}")), data).unwrap();
    }
    for protocol in [Protocol::V1, Protocol::V2, Protocol::Hybrid] {
        for piece_length in [BLOCK_LENGTH, 2 * BLOCK_LENGTH] {
            let options = CreateOptions {
                target: temp.path().to_path_buf(),
                protocol,
                piece_length: Some(piece_length),
                threads: Some(2),
                no_creation_date: true,
                no_created_by: true,
                ..CreateOptions::default()
            };
            let mapped = create::create(&CreateOptions {
                io: create::IoMode::Mmap,
                ..options.clone()
            })
            .unwrap();
            let buffered = create::create(&CreateOptions {
                io: create::IoMode::Buffered,
                ..options
            })
            .unwrap();
            assert_eq!(
                mapped.value, buffered.value,
                "I/O mode changed {protocol:?} at piece size {piece_length}"
            );
            let document = metadata::from_value(buffered.value).unwrap();
            assert_eq!(
                metadata::verify(&document, temp.path(), Some(2)).unwrap()["valid"],
                true
            );
        }
    }
}
