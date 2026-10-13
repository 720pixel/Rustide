use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use rustide::{bencode::Value, metadata};

fn command() -> Command {
    if let Some(runner) =
        std::env::var_os("RUSTIDE_TEST_BIN_RUNNER").filter(|value| !value.is_empty())
    {
        let mut command = Command::new(runner);
        command.arg(env!("CARGO_BIN_EXE_rustide"));
        command
    } else {
        Command::new(env!("CARGO_BIN_EXE_rustide"))
    }
}

fn run(config: &Path, args: &[&str]) -> Output {
    command()
        .args(args)
        .arg("--config")
        .arg(config)
        .output()
        .unwrap()
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let input = dir.path().join("payload");
    fs::write(&config, b"{}").unwrap();
    fs::write(&input, b"abc").unwrap();
    (dir, config, input)
}

#[test]
fn direct_creation_overwrite_and_payload_protection() {
    let (dir, config, input) = setup();
    let output = dir.path().join("out.torrent");
    let args = [input.to_str().unwrap(), output.to_str().unwrap()];
    success(run(&config, &args));
    let first = fs::read(&output).unwrap();
    assert!(!run(&config, &args).status.success());
    assert_eq!(fs::read(&output).unwrap(), first);
    success(run(&config, &[args[0], args[1], "--force"]));
    assert!(
        !run(&config, &[args[0], args[0], "--force"])
            .status
            .success()
    );
    assert_eq!(fs::read(input).unwrap(), b"abc");
}

#[test]
fn global_options_before_subcommand_work() {
    let (dir, config, input) = setup();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &["create", input.to_str().unwrap(), torrent.to_str().unwrap()],
    ));
    let output = command()
        .args([
            "--config",
            config.to_str().unwrap(),
            "--quiet",
            "info",
            torrent.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    let output = success(output);
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["name"], "payload");
}

#[test]
fn negative_timestamps_are_accepted_for_create_and_edit() {
    let (dir, config, input) = setup();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--creation-date",
            "-1",
        ],
    ));
    assert_eq!(
        metadata::load(&torrent)
            .unwrap()
            .value
            .get(b"creation date")
            .and_then(Value::as_int),
        Some(-1)
    );
    success(run(
        &config,
        &["edit", torrent.to_str().unwrap(), "--creation-date", "-2"],
    ));
    assert_eq!(
        metadata::load(&torrent)
            .unwrap()
            .value
            .get(b"creation date")
            .and_then(Value::as_int),
        Some(-2)
    );
}

#[test]
fn explicit_flags_override_profile_scalars() {
    let (dir, config, input) = setup();
    fs::write(&config, br#"{"profiles":{"test":{"command":"create","options":{"protocol":"v2","piece-size":"32KiB","private":true,"source":"profile"}}}}"#).unwrap();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--profile",
            "test",
            "-v",
            "v1",
            "-l",
            "16KiB",
            "--private",
            "false",
            "--source",
            "explicit",
        ],
    ));
    let doc = metadata::load(&torrent).unwrap();
    assert_eq!(
        metadata::protocol(&doc).unwrap(),
        rustide::create::Protocol::V1
    );
    assert_eq!(
        metadata::query(&doc.value, "/info/piece length")
            .unwrap()
            .as_int(),
        Some(16384)
    );
    assert_eq!(
        metadata::query(&doc.value, "/info/private")
            .unwrap()
            .as_int(),
        Some(0)
    );
    assert_eq!(
        metadata::query(&doc.value, "/info/source")
            .unwrap()
            .as_str(),
        Some("explicit")
    );
}

#[test]
fn explicit_inverse_flags_override_profile_defaults() {
    let (dir, config, input) = setup();
    fs::write(&config, br#"{"profiles":{"test":{"command":"create","options":{"created-by":"profile","creation-date":123,"cross-seed":true}}}}"#).unwrap();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--profile",
            "test",
            "--no-created-by",
            "--no-creation-date",
            "--no-cross-seed",
        ],
    ));
    let doc = metadata::load(&torrent).unwrap();
    assert!(doc.value.get(b"created by").is_none());
    assert!(doc.value.get(b"creation date").is_none());
    assert!(
        doc.value
            .get(b"info")
            .unwrap()
            .get(b"x_cross_seed")
            .is_none()
    );
}

#[test]
fn explicit_values_override_profile_removal_defaults() {
    let (dir, config, input) = setup();
    fs::write(&config, br#"{"profiles":{"test":{"command":"create","options":{"set-created-by":false,"set-creation-date":false}}}}"#).unwrap();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--profile",
            "test",
            "--created-by",
            "explicit",
            "--creation-date",
            "123",
        ],
    ));
    let doc = metadata::load(&torrent).unwrap();
    assert_eq!(
        doc.value.get(b"created by").unwrap().as_str(),
        Some("explicit")
    );
    assert_eq!(doc.value.get(b"creation date").unwrap().as_int(), Some(123));
}

#[test]
fn explicit_destination_overrides_profile_output() {
    let (dir, config, input) = setup();
    let unwanted = dir.path().join("profile.torrent");
    fs::write(&config, serde_json::to_vec(&serde_json::json!({"profiles":{"test":{"command":"create","options":{"output":unwanted}}}})).unwrap()).unwrap();
    let torrent = dir.path().join("explicit.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--profile",
            "test",
        ],
    ));
    assert!(torrent.exists());
    assert!(!unwanted.exists());
}

#[test]
fn empty_payload_and_single_child_directory_keep_valid_layouts() {
    let (dir, config, input) = setup();
    fs::write(&input, b"").unwrap();
    let folder = dir.path().join("folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("child"), b"").unwrap();
    for protocol in ["v1", "v2", "hybrid"] {
        for source in [&input, &folder] {
            let torrent = dir.path().join(format!(
                "{protocol}-{}.torrent",
                source.file_name().unwrap().to_string_lossy()
            ));
            success(run(
                &config,
                &[
                    "create",
                    source.to_str().unwrap(),
                    torrent.to_str().unwrap(),
                    "--protocol",
                    protocol,
                ],
            ));
            let doc = metadata::load(&torrent).unwrap();
            if source == &folder && protocol != "v2" {
                assert!(doc.value.get(b"info").unwrap().get(b"files").is_some());
                assert!(doc.value.get(b"info").unwrap().get(b"length").is_none());
            }
            success(run(
                &config,
                &[
                    "verify",
                    torrent.to_str().unwrap(),
                    source.to_str().unwrap(),
                    "--json",
                ],
            ));
        }
    }
}

#[test]
fn tracker_edit_preserves_cli_infohash_and_corruption_returns_failure() {
    let (dir, config, input) = setup();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--protocol",
            "hybrid",
        ],
    ));
    let before = metadata::info_hashes(&metadata::load(&torrent).unwrap()).unwrap();
    success(run(
        &config,
        &[
            "edit",
            torrent.to_str().unwrap(),
            "--announce",
            "https://tracker.example/announce",
        ],
    ));
    assert_eq!(
        metadata::info_hashes(&metadata::load(&torrent).unwrap()).unwrap(),
        before
    );
    fs::write(&input, b"bad").unwrap();
    let output = run(
        &config,
        &[
            "verify",
            torrent.to_str().unwrap(),
            input.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], false);
}

#[test]
fn edit_short_list_mode_overrides_profile_default() {
    let (dir, config, input) = setup();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--web-seed",
            "https://seed.example/first",
        ],
    ));
    fs::write(
        &config,
        br#"{"profiles":{"test":{"command":"edit","options":{"list-mode":"replace"}}}}"#,
    )
    .unwrap();
    success(run(
        &config,
        &[
            "edit",
            torrent.to_str().unwrap(),
            "--profile",
            "test",
            "-m",
            "append",
            "--web-seed",
            "https://seed.example/second",
        ],
    ));
    let doc = metadata::load(&torrent).unwrap();
    let seeds = doc.value.get(b"url-list").unwrap().as_list().unwrap();
    assert_eq!(
        seeds
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        ["https://seed.example/first", "https://seed.example/second"]
    );
}

#[test]
fn similar_torrents_require_twenty_byte_infohashes() {
    let (dir, config, input) = setup();
    let torrent = dir.path().join("out.torrent");
    let valid = "12".repeat(20);
    let invalid = "34".repeat(32);
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--similar",
            &valid,
        ],
    ));
    let before = fs::read(&torrent).unwrap();
    assert!(
        !run(
            &config,
            &["edit", torrent.to_str().unwrap(), "--similar", &invalid]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(&torrent).unwrap(), before);
    assert!(
        !run(
            &config,
            &[
                "create",
                input.to_str().unwrap(),
                torrent.to_str().unwrap(),
                "--force",
                "--similar",
                &invalid
            ]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(&torrent).unwrap(), before);
}

#[test]
fn requesting_absent_infohash_protocol_returns_failure() {
    let (dir, config, input) = setup();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "--protocol",
            "v1",
        ],
    ));
    assert!(
        !run(
            &config,
            &[
                "show",
                "infohash",
                torrent.to_str().unwrap(),
                "--protocol",
                "v2"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn attached_short_profile_option_applies_defaults() {
    let (dir, config, input) = setup();
    fs::write(
        &config,
        br#"{"profiles":{"test":{"command":"create","options":{"protocol":"v2"}}}}"#,
    )
    .unwrap();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            torrent.to_str().unwrap(),
            "-Ptest",
        ],
    ));
    assert_eq!(
        metadata::protocol(&metadata::load(&torrent).unwrap()).unwrap(),
        rustide::create::Protocol::V2
    );
}

#[test]
fn configured_tracker_edits_preserve_infohash_without_explicit_info_changes() {
    let (dir, config, input) = setup();
    let torrent = dir.path().join("out.torrent");
    success(run(
        &config,
        &["create", input.to_str().unwrap(), torrent.to_str().unwrap()],
    ));
    let before = metadata::info_hashes(&metadata::load(&torrent).unwrap()).unwrap();
    fs::write(&config, br#"{"trackers":[{"name":"Example","abbreviation":"ex","announce-url":"https://tracker.example/announce","private":true}]}"#).unwrap();
    success(run(
        &config,
        &["edit", torrent.to_str().unwrap(), "--announce", "ex"],
    ));
    let document = metadata::load(&torrent).unwrap();
    assert_eq!(metadata::info_hashes(&document).unwrap(), before);
    assert_eq!(
        document.value.get(b"announce").unwrap().as_str(),
        Some("https://tracker.example/announce")
    );
}

#[test]
fn stdout_contains_only_bencode_and_does_not_write_destinations() {
    let (dir, config, input) = setup();
    let destination = dir.path().join("untouched.torrent");
    fs::write(&destination, b"keep this file").unwrap();
    let output = success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            destination.to_str().unwrap(),
            "--protocol",
            "hybrid",
            "--stdout",
            "--json",
        ],
    ));
    let value = rustide::bencode::parse(&output.stdout).unwrap();
    assert_eq!(rustide::bencode::encode(&value), output.stdout);
    metadata::from_value(value).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"keep this file");
    let torrent = dir.path().join("original.torrent");
    fs::write(&torrent, &output.stdout).unwrap();
    let edited = success(run(
        &config,
        &[
            "edit",
            torrent.to_str().unwrap(),
            "--comment",
            "edited",
            "--stdout",
            "--json",
        ],
    ));
    let edited_value = rustide::bencode::parse(&edited.stdout).unwrap();
    assert_eq!(rustide::bencode::encode(&edited_value), edited.stdout);
    assert_eq!(
        edited_value.get(b"comment").unwrap().as_str(),
        Some("edited")
    );
    assert_eq!(fs::read(&torrent).unwrap(), output.stdout);
}

#[test]
fn output_directory_is_rejected_without_modifying_its_contents() {
    let (dir, config, input) = setup();
    let destination = dir.path().join("destination");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), b"unchanged").unwrap();
    for force in [false, true] {
        let mut args = vec![
            "create",
            input.to_str().unwrap(),
            destination.to_str().unwrap(),
        ];
        if force {
            args.push("--force");
        }
        assert!(!run(&config, &args).status.success());
        assert!(destination.is_dir());
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
    }
    assert_eq!(fs::read(input).unwrap(), b"abc");
}

#[test]
fn dry_run_validates_metadata_options_and_never_writes_output() {
    let (dir, config, input) = setup();
    let destination = dir.path().join("dry.torrent");
    let valid = success(run(
        &config,
        &[
            "create",
            input.to_str().unwrap(),
            destination.to_str().unwrap(),
            "--dry-run",
        ],
    ));
    let report: serde_json::Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(report["files"], 1);
    assert_eq!(report["payload_bytes"], 3);
    assert!(!destination.exists());
    for (flag, value) in [
        ("--name", "../escape"),
        ("--checksum", "unsupported"),
        ("--dht-node", "host:not-a-port"),
        ("--include", "("),
    ] {
        for dry_run in [false, true] {
            let mut args = vec![
                "create",
                input.to_str().unwrap(),
                destination.to_str().unwrap(),
                flag,
                value,
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let output = run(&config, &args);
            assert!(
                !output.status.success(),
                "accepted {flag}={value} with dry_run={dry_run}"
            );
            assert!(!destination.exists());
        }
    }
    assert_eq!(fs::read(input).unwrap(), b"abc");
}

#[test]
fn help_and_version_work_after_global_flags() {
    let (dir, config, _) = setup();
    for flag in ["--help", "--version"] {
        let output = success(
            command()
                .args(["--config", config.to_str().unwrap(), "--quiet", flag])
                .current_dir(dir.path())
                .output()
                .unwrap(),
        );
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    let output = success(
        command()
            .args([
                "--quiet",
                "--config",
                config.to_str().unwrap(),
                "create",
                "--help",
            ])
            .current_dir(dir.path())
            .output()
            .unwrap(),
    );
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--piece-size"));
    assert!(help.contains("OUTPUT.TORRENT"));
}

#[test]
fn invalid_piece_sizes_and_thread_counts_fail_without_output() {
    let (dir, config, input) = setup();
    let destination = dir.path().join("invalid.torrent");
    for (flag, values) in [
        (
            "--piece-size",
            &[
                "0",
                "15KiB",
                "3MiB",
                "128MiB",
                "65",
                "18446744073709551616",
                "invalid",
            ][..],
        ),
        (
            "--threads",
            &["4097", "-1", "1.5", "18446744073709551616", "invalid"][..],
        ),
    ] {
        for value in values {
            let option = format!("{flag}={value}");
            for dry_run in [false, true] {
                let mut args = vec![
                    "create",
                    input.to_str().unwrap(),
                    destination.to_str().unwrap(),
                    &option,
                ];
                if dry_run {
                    args.push("--dry-run");
                }
                assert!(!run(&config, &args).status.success(), "accepted {option}");
                assert!(!destination.exists());
            }
        }
    }
    assert_eq!(fs::read(input).unwrap(), b"abc");
}

#[test]
fn explicit_buffered_io_matches_mmap_output_and_rejects_unknown_modes() {
    let (dir, config, input) = setup();
    fs::write(&input, vec![37; 3 * rustide::merkle::BLOCK_LENGTH + 17]).unwrap();
    for protocol in ["v1", "v2", "hybrid"] {
        let mut outputs = Vec::new();
        for mode in ["mmap", "buffered"] {
            let destination = dir.path().join(format!("{protocol}-{mode}.torrent"));
            success(run(
                &config,
                &[
                    "create",
                    input.to_str().unwrap(),
                    destination.to_str().unwrap(),
                    "--protocol",
                    protocol,
                    "--io",
                    mode,
                    "--no-creation-date",
                    "--no-created-by",
                ],
            ));
            outputs.push(fs::read(destination).unwrap());
        }
        assert_eq!(
            outputs[0], outputs[1],
            "I/O mode changed {protocol} metadata"
        );
    }
    let destination = dir.path().join("invalid-io.torrent");
    assert!(
        !run(
            &config,
            &[
                "create",
                input.to_str().unwrap(),
                destination.to_str().unwrap(),
                "--io",
                "invalid"
            ]
        )
        .status
        .success()
    );
    assert!(!destination.exists());
}
