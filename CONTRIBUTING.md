# Contributing to Cupola

Thanks for helping. Cupola is a native messenger for agent conversations: a Rust host
on your computer and a SwiftUI app on your phone. Please read
[AGENTS.md](AGENTS.md) first; it states what the product should feel like and what
counts as done.

## Before you start

- For anything bigger than a small fix, open an issue first so we can agree on the
  shape. A change that adds a setting, a screen or a protocol field needs a reason a
  user would recognize.
- Security problems go through [SECURITY.md](SECURITY.md), not a public issue.
- Use synthetic data in issues, fixtures and screenshots. Never post pairing links,
  tokens, APNs data, message bodies, device identifiers or database files.

## Build and test

[DEVELOPMENT.md](DEVELOPMENT.md) has the setup. In short:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # the host
swift test --package-path apps/native        # shared Swift code (macOS)
```

For app changes, build the **Diagnostics** scheme for a simulator and run the
scenarios that cover the screens you touched ([apps/ios/TESTING.md](apps/ios/TESTING.md)).
A UI change is not done until you have looked at it on iPhone, iPad and the two-screen
foldable layout, folded and unfolded, and it matches [apps/ios/DESIGN.md](apps/ios/DESIGN.md).
Say in the pull request what you ran and on what; label anything you could not run
(for example hardware-only behavior) as unverified.

## Pull requests

- One change per pull request, with a description of the problem and how you checked
  the fix. Small, reviewable diffs land fastest.
- Prove the change with a build, a simulator run or a screenshot. Add a test when it
  protects something a run cannot exercise (security, protocol, storage, migrations);
  extend the existing fixture or table instead of adding a parallel one.
- Keep user-visible text plain and consistent with the product's language: the app is
  called Cupola, and Projects and threads are the model.
- Do not commit secrets, local state, real conversations or build output. Run
  `gitleaks dir .` before you push if you have it; the repository ships a
  `.gitleaks.toml`.
- Database migrations in `crates/wonder-store/migrations` are append-only once released;
  add a new one rather than editing an old one.

## Licensing

Cupola is MIT licensed ([LICENSE](LICENSE)). By submitting a change you agree it can be
distributed under that license. Do not paste code from sources whose license is
incompatible, and add any new third-party dependency or asset to
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) (Rust crates are collected by
`scripts/collect-dependency-notices.py`).

## Maintainer tooling

The maintainers' release, signing and CI-farm tooling is not part of this repository.
Release builds, the Homebrew tap and TestFlight uploads are done by the maintainers.
