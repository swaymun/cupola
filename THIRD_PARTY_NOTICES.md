# Third-party notices

Cupola-owned source is licensed under MIT. Third-party code retains its original
licenses; Cupola's MIT license does not replace them.

- `licenses/RUST-NOTICES.txt` contains the native Rust dependency inventory and
  full license texts, keyed by SHA-256. `licenses/rust-dependencies.json` records
  exact versions and their published crate source URLs. The packaged dependencies
  are unmodified. Regenerate with `python3 scripts/collect-dependency-notices.py`.
- `licenses/upstream/index.json` records exact upstream sources for texts omitted
  from packaged crates. Five older crates omit a standalone license file; their
  entries include the declared standard license terms, published manifest authors,
  and any copyright notices present in their source. No copyright year is invented.
- The unmodified MPL-2.0 dependency `option-ext` 0.2.0 is available as source at
  https://crates.io/crates/option-ext/0.2.0 and
  https://static.crates.io/crates/option-ext/option-ext-0.2.0.crate.
  Its MPL terms are included in the Rust notices. It remains under MPL-2.0.
- `licenses/Rust-COPYRIGHT.html.gz` contains the complete notices supplied by the
  Rust toolchain, compressed without modification. Decompress with `gzip -dc`.
- `WebRTC-LICENSE.md` in the app Resources covers the bundled WebRTC framework.
- Native Windows/Linux computer sharing uses LiveKit's `libwebrtc` 0.3.52 and
  `webrtc-sys` 0.3.49 Rust bindings (Apache-2.0) with a source-built Google WebRTC
  library at revision `51c367e209f8f12b39bd8dd0ca70bf588f42cb48`. The build disables
  H.264/H.265 and has no FFmpeg/OpenH264 definitions; Cupola negotiates VP8 only.
  The complete generated native and Rust dependency notices are preserved in the
  source tree under `crates/wonderd/computer-helper/notices/`, in
  `ComputerSharing-Linux.md` and `ComputerSharing-Windows.md`.
  Both are available in the phone app's Settings -> Acknowledgements. Each host
  release archive also contains its actual build's full texts, GN arguments and
  hash-bound source/library/helper manifest in `licenses/computer-helper/`.
  Regenerate the app texts with the helper's `collect-app-notices.py` from the
  audited outputs of `release-manifest.py`; never substitute prebuilt notices.
- `SwiftTerm-LICENSE.md` in the iOS app covers SwiftTerm 1.18.0
  (https://github.com/migueldeicaza/SwiftTerm, MIT, revision
  7691f85b222a67a66b58499e1b2647443cf0dda7), the terminal emulator view. It is
  fetched unmodified with Swift Package Manager; only the `SwiftTerm` library is
  linked. The notice shows in Settings -> Acknowledgements.
- The iOS app bundles Inter, Atkinson Hyperlegible, JetBrains Mono and IBM Plex
  Mono, unmodified, under the SIL Open Font License 1.1 (IBM Plex has the Reserved
  Font Name "Plex"). Their license texts ship in `Fonts/` and show in Settings ->
  Acknowledgements.
- The iOS Herbalist themes adapt colours from the Apothecary
  Diary VS Code theme (https://github.com/montemurro19/apothecary-diary-theme,
  MIT, Copyright (c) 2025 Matheus Montemurro); its notice ships in the app
  (`ThemeCredits`) and shows in Settings -> Acknowledgements.
- The iOS app's Code changes pill shows the Git logo by Jason Long
  (https://git-scm.com/downloads/logos), licensed under the Creative Commons
  Attribution 3.0 Unported License (https://creativecommons.org/licenses/by/3.0/).
  It ships as a single-colour vector (`Assets.xcassets/GitLogo.imageset`) tinted
  to the theme; its shape is unaltered. The notice shows in Settings ->
  Acknowledgements.
- `apps/native/Sources/WonderPairing/Resources/Highlight/highlight.min.js` is a
  subset build of highlight.js (BSD-3-Clause) with Cupola's `wonder-spans.js`; its
  license text sits beside it (`LICENSE`) and shows in Settings -> Acknowledgements.
  `scripts/build-highlightjs` rebuilds it from `tools/highlightjs`.
- `research/codex-app-server/` holds the JSON schemas and TypeScript types that the
  OpenAI Codex CLI generates for its App Server protocol, unmodified, for the Codex
  versions Cupola checks (https://github.com/openai/codex, Apache-2.0). The host
  embeds them to verify a user's installed Codex. The full Apache License 2.0 text is
  reproduced in `licenses/RUST-NOTICES.txt`.
- The provider and connector icons in `apps/ios/Wonder/Assets.xcassets`
  (`Provider*`, `Connector*`) are the trademarks of their owners (OpenAI, Anthropic,
  GitHub, Google, Linear, Adobe and others). They identify those services inside
  Cupola and do not imply endorsement; the MIT license does not cover them.
- `node/LICENSE` in app Resources contains Node.js and its bundled third-party
  notices. The bundled npm distribution retains its license files.
- `claude-runtime/node_modules` retains the license files of the locked production
  dependencies listed in `services/claude-runtime/package-lock.json`, including
  Anthropic's separately licensed Agent SDK/native executable, the Model Context
  Protocol SDK and Zod. Cupola's MIT license does not cover the Agent SDK.

- Portions of `crates/wonderd/src/provider_switch.rs` (the provider-session
  transition decision, switch planning, context-handoff budget, history
  selection and delivery rules) are ported from t3code
  (https://github.com/pingdotgg/t3code, `apps/server/src/orchestration-v2/`),
  which is licensed under the MIT License:

  > MIT License
  >
  > Copyright (c) 2026 T3 Tools Inc.
  >
  > Permission is hereby granted, free of charge, to any person obtaining a copy
  > of this software and associated documentation files (the "Software"), to deal
  > in the Software without restriction, including without limitation the rights
  > to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
  > copies of the Software, and to permit persons to whom the Software is
  > furnished to do so, subject to the following conditions:
  >
  > The above copyright notice and this permission notice shall be included in all
  > copies or substantial portions of the Software.
  >
  > THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
  > IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
  > FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
  > AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
  > LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
  > OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
  > SOFTWARE.

The push Worker has no runtime npm dependencies. Its development dependencies
are recorded in `services/push/package-lock.json` and retain their own licenses.
Apple SDKs, ChatGPT, Codex and other user-installed runtimes are separately
licensed. Proprietary reference IPAs and extracted bundles are not distributed.
