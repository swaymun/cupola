---
name: Cupola native
description: Native Project conversations on iPhone and iPad
---

# Cupola design system

## Overview

Cupola is a compact native messenger on iPhone and iPad, hosted by the `cupola`
command on the owner's computer. Preserve the existing app's visual design while moving
all conversations to Project threads. The product model lives in
[PRODUCT.md](PRODUCT.md) and [the product contract](docs/WONDER_PRODUCT_AND_UI.md).

This document records source inspected on October 13, 2026. It does not assert
new visual acceptance. Native source owns exact values; do not introduce a
parallel token layer or apply old PWA rules to SwiftUI.

## Colors

On iOS, `apps/native/Sources/WonderPairing/WonderThemeCatalog.swift` owns palettes
and `apps/ios/Wonder/WonderTheme.swift` resolves them once into the environment.
The default System theme uses system page, text and grouped-control treatments,
with neutral conversation bubbles. Cupola's own themes, Cupola Light (sky blue)
and Cupola Dark (night sky), and the classic and photo themes use their authored
palette, syntax colors and contrast checks. Reuse `page`, `surface`, `text`,
`rule` and bubble tokens rather than hard-coding another screen's colors.

Actions use the existing neutral text tint. Selection and primary emphasis use
the established control treatment; do not recolor every action amber. Photo
backgrounds are drawn once behind the conversation with a scrim, and become a
solid surface with Reduce Transparency. Prominent buttons use
`WonderProminentButtonStyle` so their labels remain legible in dark themes.

## Typography

`apps/ios/Wonder/WonderTypography.swift` owns message and code faces. System
text styles and Dynamic Type are the default. The saved message choices are
Default, New York, Rounded, Monospaced, Inter and Atkinson Hyperlegible; code
uses SF Mono, JetBrains Mono or IBM Plex Mono. Bundled faces scale through the
same text styles. Preserve font selection and its rendering-cache invalidation.

Navigation, controls and labels use native hierarchy. Keep headers compact,
body text readable and secondary labels restrained. Let text wrap or adapt at
large sizes instead of shrinking it to preserve a decorative layout.

## Layout

The conversation and composer are the main surface. On iPhone the selected
thread occupies the available width; Chats opens as the existing sidebar and
has an obvious close/back action. iPad uses its wider sidebar layout, preserving
its visibility choice. Safe areas, keyboard avoidance and the bottom composer
must remain usable on both.

Use the existing bubble alignment: agent left, user right. Keep the header,
activity groups and composer controls dense. Provider and Project identity
replace retired decorative conversation avatars. Files, requests and details
use the existing native navigation or sheet patterns. Do not rebuild these as
web drawers or apply a fixed browser breakpoint system.

## Elevation & Depth

Use tonal separation and native sheets/materials where already present. The
conversation's composer and pill row sit on its backdrop; do not add another
framed panel. Keep the small timeline fade and its Reduce Transparency fallback.
Avoid nested cards and repeated status boxes.

## Shapes

Preserve the existing bubble, pill and native control shapes. Use few borders
and restrained corners; do not replace compact rows with oversized rounded
cards. Keep native hit areas at least 44 points on iOS even when an icon is
smaller. Icons use SF Symbols or the existing provider/brand assets, with
accessible names when they convey an action or identity.

## Components

- **Chats:** compact Project/thread names and previews with truthful work state.
  Preserve search, selection and read acknowledgements.
- **Header:** provider and Project identity, short title and existing actions.
  Keep decoration independent of streaming state.
- **Timeline:** readable messages and folded routine activity; actionable
  requests, failures, file changes and artifacts stay discoverable. Preserve
  paging, search navigation, text selection and reading position.
- **Composer:** the existing editor and adjacent controls. Preserve drafts,
  attachments, model/access choices, stop and delivery feedback. Do not restyle
  it as a side effect of adding a feature.
- **Approvals/questions:** existing request pills and focused request views,
  with actual choices, consequences and retryable failure on the affected item.
- **Files/diffs:** theme-aware content with preview/download fallbacks and
  legible code. Long content must scroll without expanding the whole screen.

## Do's and Don'ts

- Do reuse theme, typography, native navigation and the current control owners.
- Do preserve Dynamic Type, VoiceOver labels, keyboard focus, selection, Reduce
  Motion and Reduce Transparency. Avoid motion that is needed to read status.
- Do keep names, values and action labels; explain only meaningful choices or
  actionable recovery. Keep failure feedback next to its cause.
- Do capture the real app on iPhone and iPad when a UI change is verified, with
  the build and limitations recorded on its PR per AGENTS.md.
- Don't introduce decorative metadata, protocol labels, refresh buttons or
  multiple boxes explaining the same state.
- Don't restore Bot avatars, Group Chat framing, a PWA, a Mac Chats client or a GUI host app.
- Don't treat this source-derived record as proof of accessibility or device
  acceptance, or invent new palettes, dimensions and capabilities from it.
