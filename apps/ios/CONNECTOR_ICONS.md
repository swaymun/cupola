# Connector icons

The iOS asset catalog bundles the following official product artwork for offline
display in Connected apps. It identifies integrations; it does not imply endorsement.
Unknown connectors retain the runtime-supplied HTTPS icon or the system fallback.

- Gmail: https://www.gstatic.com/images/branding/product/2x/gmail_2020q4_48dp.png
- Google Calendar: https://www.gstatic.com/images/branding/product/2x/calendar_2020q4_48dp.png
- Google Drive: https://www.gstatic.com/images/branding/product/2x/drive_2020q4_48dp.png
- Claude Docs: https://code.claude.com/docs/_mintlify/favicons/claude-code/pLsy-mRpNksna2sx/_generated/favicon/android-chrome-192x192.png
  (one image for light and dark: the dark favicon was byte-identical and was removed October 25, 2026)

Retrieved September 27, 2026. Artwork bytes are unchanged; the asset catalog scales
them to the existing 32-point icon slot. Google artwork is served by its official
static asset host. See the
[Workspace brand resources](https://knowledge.workspace.google.com/admin/getting-started/brand-your-internal-communications-with-google-workspace).
Claude artwork is the icon served by its [official documentation](https://code.claude.com/docs/en/overview).
Product names and artwork remain the property of their respective owners.

Added September 29, 2026, without changing artwork bytes:

- GitHub light/dark: installed official GitHub plugin 0.1.12 assets `github.png` and `github-dark.png`.
- OpenAI Platform: installed official OpenAI Developers plugin 1.3.6, `openai-platform.png`.
- Linear: installed official Linear plugin 5.0.1, `logo.png`.
- Sites: installed official Sites plugin 0.1.75, `icon.svg`.
- Flashloop: https://www.flashloop.ai/favicons/light/icon.svg and https://www.flashloop.ai/favicons/dark/icon.svg
- Adobe Acrobat: https://www.adobe.com/federal/assets/svgs/acrobat-pro-40.svg

Unknown connections use a system puzzle-piece icon when their remote artwork is
missing or unavailable. Connection names omit the leading Claude.ai namespace;
runtime identifiers and permission targets are preserved.

## Provider icons

Project threads identify their agent with the provider's desktop app icon, added
September 30, 2026 and scaled to 96 px for the 32-point slot:

- Codex: the monochrome OpenAI knot, installed ChatGPT for Mac 26.1013.40934,
  `Contents/Resources/icon-chatgpt.png` (2048 px, scaled to 96 px; one image for
  light and dark). Replaced the Codex app icon on October 20, 2026 at the owner's request.
- Claude: installed Claude for Mac 2.16120.0, `Contents/Resources/electron.icns`
  (128 px representation).

Checked October 2, 2026 against installed ChatGPT for Mac 26.928.40906 and
Claude for Mac 2.19675.0. The current Codex light asset is byte-identical to
the bundled 96 px image, and the dark variant is visually unchanged. The
Claude icon is visually unchanged at the 32-point display size (96 px exports
have a 0.7% mean absolute pixel difference), so no provider artwork was
replaced. These images identify the selected provider inside Cupola; they are
not Cupola branding or an endorsement claim. OpenAI's current
[brand guidelines](https://openai.com/brand/) permit service-related logo use
subject to their terms and prohibit implying endorsement. Anthropic's current
installed app is the source of the Claude icon; no broader reuse license is
claimed here.
