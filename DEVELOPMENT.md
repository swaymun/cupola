# Build Cupola from source

For normal use, follow [the installation guide](INSTALL.md). A source build needs:

| Tool | For |
| --- | --- |
| Rust (stable, via [rustup](https://rustup.rs)) | The host: `crates/` |
| Xcode (with command-line tools) on a Mac | The iOS app and the Swift packages. The app targets iOS 26. |
| Xcode's Metal Toolchain (`xcodebuild -downloadComponent MetalToolchain`, once per Xcode version) | SwiftTerm's Metal shader |
| Node 22 or later | The Claude and ACP runtimes and the push Worker (`services/`) |

`scripts/setup-self-hosted.sh` checks the build tools and the installed Codex
runtime without changing anything on your machine.

## The host (`cupola`, `wonderd`)

```sh
git clone https://github.com/swaymun/cupola.git
cd cupola
cargo build --release -p wonderd --features experimental-headless --bin cupola --bin wonderd
target/release/cupola init
target/release/cupola start
target/release/cupola pair
```

The workspace has four crates: `wonder-store` (SQLite storage and migrations),
`wonder-api` (pairing and the signed HTTP protocol), `wonder-app-server` (the
Codex App Server client) and `wonderd` (the host: providers, the HTTP API,
computer sessions and the `cupola` command). Tests:

```sh
cargo test --workspace
```

Some `wonderd` and `wonder-app-server` tests only run on macOS. Linux builds the
host and runs the rest.

Known Codex runtime versions and schema hashes are in `compatibility-manifest.json`.
`WONDER_CODEX_BIN` points the host at an explicit Codex executable for development;
it passes the same schema checks. `wonderd --locate-runtime` prints the one it picked.

On macOS, computer use needs the signed windowless bundle that
`scripts/package-cli-macos.sh` builds, because Screen Recording and Accessibility
follow its signing identity. Set `WONDER_APP_SIGNING_IDENTITY` to your own Developer ID
Application identity. Do not replace a running bundle by copying individual
executables into it. The Mac bundle also downloads checksum-pinned Node and installs
the Claude adapter's locked production dependencies with lifecycle scripts disabled.

## The iOS app

Open `apps/ios/Wonder.xcodeproj`. The **Wonder** scheme builds Release; the
**Diagnostics** scheme adds the fixtures and test driver ([DIAGNOSTICS.md](apps/ios/DIAGNOSTICS.md)).

The project ships with the signing team left empty. For a simulator build you need
no team and no Apple ID:

```sh
xcodebuild -project apps/ios/Wonder.xcodeproj -scheme Diagnostics \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build
```

To run on a device, select your team on every target in Xcode (Signing &
Capabilities) and change the bundle identifiers (`WONDER_APP_BUNDLE_IDENTIFIER`,
`WONDER_WIDGET_APP_GROUP` and the extensions' identifiers in the build settings) to
ones you own: the checked-in identifiers belong to the released app. Push, Live
Activities and Sign in with Apple need your own Apple Developer setup and a push
Worker ([services/push/README.md](services/push/README.md)). Pairing, chats and
computer view do not.

The app links SwiftTerm through Swift Package Manager and the local package
`apps/native`.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
swift test --package-path apps/native
swift test --package-path native/computer-use          # macOS
(cd services/push && npm ci && npm run check && npm test)
(cd services/claude-runtime && npm ci && npm test)
(cd services/acp-runtime && npm test)
```

Repository conventions have their own checks, each printing one line per problem:
`scripts/check-theme`, `scripts/check-project-language`, `scripts/check-brand-name`,
`scripts/check-platform-wording`, `scripts/check-provider-branches`,
`scripts/check-provider-routing`, `scripts/check-privacy-manifests`. `scripts/ci-changes
--plan BASE HEAD` prints which groups of checks a change needs.

The protocol authority is `packages/protocol/schemas/wonder-http-v1.json`. Native
tests and their fixtures use the product renderers; fixture success is not device
or network qualification.

For simulator scenarios and screenshots see [apps/ios/TESTING.md](apps/ios/TESTING.md).
