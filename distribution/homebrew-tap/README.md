# Unpublished swaymun/tap layout

Templates are kept here because there is no approved public release URL or
artifact version yet. Generate real `Casks/cupola.rb` and `Formula/cupola.rb`
from a locally verified channel manifest:

```sh
python3 scripts/prepare-cli-tap.py .local/release/feed/stable.json \
  --base-url https://OWNER-APPROVED-ARCHIVE-DIRECTORY \
  --out .local/release/homebrew-tap
```

The cask consumes the Developer ID signed, notarized and stapled macOS ZIP. It
keeps the app intact and links `cupola` inside it. The Linux formula installs
prebuilt binaries and locked runtime bridges under `libexec`; it is never built
as an unsigned macOS formula. Single-architecture feeds constrain the package
to that architecture. The cask uses Homebrew's normal Gatekeeper checks.
It installs the intact bundle as `Cupola.app`. Casks from earlier releases
named it `Cupola CLI.app`; `brew upgrade --cask cupola` moves it, and the next
`cupola service restart` (or host start) points the login service at the new
path. The bundle identity and signature do not change, so macOS permissions
carry over. Follow the previous-beta migration in `../README.md` before
replacing an installation from the previous beta name.

For offline local audit, symlink the generated directory as the local tap under
`$(brew --repository)/Library/Taps/swaymun/homebrew-tap` (or use an isolated
temporary tap name when that tap already exists), then run:

```sh
HOMEBREW_NO_AUTO_UPDATE=1 brew audit --strict --formula swaymun/tap/cupola
HOMEBREW_NO_AUTO_UPDATE=1 brew audit --strict --cask swaymun/tap/cupola
```

Remove only the temporary tap link after auditing. Audit does not establish
download availability, notarization, successful installation or public approval.
Do not create a GitHub tap repository, push it, publish a release or advertise
`brew install swaymun/tap/cupola` until the owner's publication gate is cleared.
For Homebrew-owned installations, use `brew upgrade` and `brew uninstall`.
`cupola update` refuses both channels (including `--check`) before contacting
the feed or changing files and prints the appropriate `brew upgrade` command.
Ownership follows the executable in the Cellar or the Caskroom's link to the
installed app. Cask prefix discovery uses the standard Homebrew locations,
`HOMEBREW_PREFIX`, and `brew` directories on `PATH`; set `HOMEBREW_PREFIX` for a
custom prefix when invoking the app directly without Homebrew on `PATH`.
