# Host-rendered references

Six shared production presentation layouts and the native reply components, each × light/dark × standard/large text.
Record and compare with `apps/ios/snapshot-tests`; see `../../TESTING.md`.

Recorded on arm64 macOS 26.6.2 with Xcode 27.1 / Swift 6.4, at 390 points and 1× sRGB.
These are Mac SwiftUI ImageRenderer captures, not iOS app screenshots. The large
fixture uses DynamicTypeSize.accessibility3 plus explicit prepared 40pt body fonts,
since macOS does not scale semantic fonts from that environment value.
