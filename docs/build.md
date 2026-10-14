# Building portable releases

Release builds use Rust 1.99.0 and the committed `Cargo.lock`. Install that
toolchain with rustup, then select it with `rustup default 1.99.0` or set
`RUSTUP_TOOLCHAIN=1.99.0` for the commands below. A host C linker is needed by
Cargo build scripts and test dependencies. No external cryptography library or
assembly compiler is required. The source minimum Rust version remains 1.89;
release builds use the newer pinned toolchain for optimization.

## Linux

On a Linux development host with Bash, GNU tar, gzip, and binutils:

```sh
bash scripts/build-release.sh x86_64-unknown-linux-musl
```

The script installs the Rust target, tests it, builds the executable with both
command-line and desktop interfaces, and verifies that it needs neither an ELF
interpreter nor shared libraries. It uses Rust's bundled `rust-lld` and musl
libraries, so no separate musl cross compiler is needed. The executable and its
licenses are packaged under `dist/`, alongside a SHA-256 checksum. Archives include
`THIRD_PARTY_LICENSES.txt`, covering the
locked dependency versions and bundled Rust and musl runtime notices. Update
these notices when changing dependencies or the release Rust toolchain.
Archive timestamps default to zero; set `SOURCE_DATE_EPOCH`
to choose a release timestamp.

To build and test ARM64 from an x86-64 host, install QEMU's user-mode ARM64
runner and run:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
RUSTIDE_TEST_BIN_RUNNER=qemu-aarch64 \
  bash scripts/build-release.sh aarch64-unknown-linux-musl
```

On a native ARM64 host, omit both runner environment variables. CPU acceleration
is selected at runtime. Do not add `-C target-cpu=native` or force optional CPU
features for distributed binaries.

## Windows

Build on Windows with the MSVC Rust toolchain and Visual Studio C++ build tools:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo test --locked --features gui --target x86_64-pc-windows-msvc --all-targets
cargo build --locked --features gui --release --target x86_64-pc-windows-msvc --bin rustide
```

The resulting `target/x86_64-pc-windows-msvc/release/rustide.exe` contains the C
runtime and needs only Windows system libraries. It includes both interfaces.
Copy it to any writable folder and run it; installation, administrator access,
and a runtime download are not required. The release workflow checks imported
DLL names for external C runtime dependencies and produces a standalone `.exe`
and a ZIP containing the executable, README, license, and third-party notices.

## Desktop development

Desktop support is enabled by default. Running Rustide without arguments opens
the desktop interface; `rustide gui` and `rustide --gui` open it explicitly.
Existing commands such as `rustide create` use the command-line interface. To run
the desktop interface from source:

```sh
cargo run --locked --bin rustide -- gui
```

The interface uses embedded fonts and software rendering. Linux requires an X11
display, or XWayland on a Wayland desktop; it connects directly to the display
server without needing OpenGL or a separately installed GUI runtime. Windows
uses the system window and drawing APIs. Both interfaces use the same torrent
creation and metadata editing code.

To build only the command-line interface, omit desktop dependencies:

```sh
cargo build --locked --release --no-default-features --bin rustide
```

## Release workflow

Run the `Release` GitHub Actions workflow manually to test and download build
artifacts. Select its `publish` input to also publish the version from
`Cargo.toml`, or push a matching version tag such as `v0.2.0`. Publication waits
for both Linux architectures and Windows to pass their tests, formatting and
Clippy checks, and a test run with the minimum supported Rust version. Published
downloads have stable names and a combined `SHA256SUMS` file.
