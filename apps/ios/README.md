# Cupola for iPhone and iPad

Open `Wonder.xcodeproj` in Xcode (iOS 26 or later; iPhone and iPad). The local Swift
package `../native` holds the pairing, protocol and rendering code.

```sh
swift test --package-path apps/native
xcodebuild -project apps/ios/Wonder.xcodeproj -scheme Diagnostics \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build
```

**Signing.** `DEVELOPMENT_TEAM` is empty in the project. To run on a device, choose
your team under Signing & Capabilities for every target and use bundle identifiers
you own; the checked-in ones (`com.swaymun.wonder*`, and `com.saimun.wonder.native`
for Debug) belong to the released app. `ExportOptions.plist` carries `YOUR_TEAM_ID`.
Simulator builds with `CODE_SIGNING_ALLOWED=NO` cannot use the Keychain; for a
simulator you will actually pair from, build normally with automatic signing.

**Pairing.** On the host computer run `cupola pair`. Scan its QR code in the app,
paste its pairing link, or enter the host address and the code. Compare the
verification text and confirm on the computer where `cupola pair` asks. Creating or
scanning an offer alone grants no access.

On a physical device the app uses a Secure Enclave P-256 identity. The key and the
connection credentials live in device-only, when-unlocked Keychain items. Simulator
builds use a simulator-only software key. A device approval stays valid until
revoked (`cupola devices revoke`, which does not need the phone); session
credentials last at most one hour and renew with the device key.

Debug simulator builds accept `-read-preview` for a synthetic conversation with no
network or production cache access (not in Release or on physical devices).

See [TESTING.md](TESTING.md) for scenarios and [DESIGN.md](DESIGN.md) for the visual
design language.
