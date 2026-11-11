# Opt-in native integration checks

The no-model HTTP smoke checks the one-line reply to a pairing link opened in a browser, the absence of any browser page, and native API origin/session boundaries. It does not pair a device or consume model usage.

```text
WONDER_PUBLIC_ORIGIN=https://wonder.example.ts.net node tests/live/native-http-smoke.mjs
```

Native interaction QA uses signed iPhone/iPad simulator builds and the installed Mac app. Pair through the native QR/code flow and confirm matching verification text on the Mac. Test real work only with explicit authorization, and remove disposable fixtures afterward. Build-only and simulator checks do not replace physical-device acceptance.

The retired browser source, Playwright harnesses and browser build jobs have been removed. Existing historical research and dated verification records are retained as history, not current setup instructions.
