# Bundled fonts

Unmodified font files from [google/fonts](https://github.com/google/fonts) at commit
`2eb0b48d5f760f62e286216f0859a8c540dbc1bd`, each under the SIL Open Font License 1.1.
The license texts ship next to them and appear in Settings → Acknowledgements and in
each font's long-press menu in Settings → Appearance → Font. Every file matches its
upstream git blob byte for byte (checked 2026-10-09 with `git hash-object`). Fonts with a Reserved Font Name (IBM Plex: "Plex") must stay unmodified
to keep their names, so do not subset or rename these files.

| Family | Files (source path under `ofl/`) | Bytes |
| --- | --- | --- |
| Inter | `inter/Inter[opsz,wght].ttf` → `Inter.ttf`, `inter/Inter-Italic[opsz,wght].ttf` → `Inter-Italic.ttf` | 1,783,172 |
| Atkinson Hyperlegible | `atkinsonhyperlegible/AtkinsonHyperlegible-{Regular,Italic,Bold,BoldItalic}.ttf` | 219,784 |
| JetBrains Mono | `jetbrainsmono/JetBrainsMono[wght].ttf` → `JetBrainsMono.ttf`, `JetBrainsMono-Italic[wght].ttf` → `JetBrainsMono-Italic.ttf` | 378,764 |
| IBM Plex Mono | `ibmplexmono/IBMPlexMono-{Regular,Italic,Bold}.ttf` | 417,284 |

`WonderTypography` registers them for the app process on first use; nothing is added
to Info.plist.
