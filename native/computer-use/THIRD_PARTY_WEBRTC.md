# WebRTC M153 release evidence

Cupola pins `stasel/WebRTC` exactly at `153.0.0` in `Package.swift` and
`Package.resolved`.

- Package source: https://github.com/stasel/WebRTC.git
- Binary artifact: `WebRTC-M153.xcframework.zip`
- Published artifact size: 45,221,668 bytes
- SwiftPM checksum: `3e3a8946f27510133e3feed04d05fa23505bbe366e977620503bfc7986c2b78f`
- Resolved revision: `4266157cd08f92115de885ab12d87196a8db87e1`
- Upstream license: `WebRTC/LICENSE.md`, SHA-256
  `843529896bae499c92af3ecade86855128f930334ba97530695ccecef56e966d`
- dSYM artifact: `WebRTC-M153-dSYM.zip`, published size 414,894,487 bytes.
  It is retained only as release evidence under ignored `.local/` when
  available and is never bundled in `Cupola.app`.

The package script copies the upstream license into
`Contents/Resources/WebRTC-LICENSE.md`, embeds only the macOS framework
slice, adds the app-relative runtime rpath, and verifies the signed framework
and executable linkage after signing.
