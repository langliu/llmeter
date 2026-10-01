# Provider logo sources

LobeHub Icons is the preferred source. These assets use provider-specific icons
from `@lobehub/icons@5.16.0`:

- Codex: https://icons.lobehub.com/components/codex
- Grok: https://icons.lobehub.com/components/grok

The Hermes Agent asset is the provider-specific SVG from
`@lobehub/icons-static-svg@1.94.0`:

- Hermes Agent: https://icons.lobehub.com/components/hermes-agent

The following provider-specific assets come from
`@lobehub/icons-static-svg@1.94.0`:

- Cursor: https://icons.lobehub.com/components/cursor
- Qoder: https://icons.lobehub.com/components/qoder
- TRAE: https://icons.lobehub.com/components/trae

The following fallback assets use the SVG paths and brand colors from
`simple-icons@16.28.0`:

- Claude: https://simpleicons.org/?q=claude
- OpenCode: https://simpleicons.org/?q=opencode
- Pi: https://simpleicons.org/?q=pi
- Zed Industries: https://simpleicons.org/?q=zed

Oh My Pi is not in LobeHub Icons or Simple Icons. `omp.svg` is the official
product mark from https://github.com/can1357/oh-my-pi/blob/main/assets/icon.svg
(π bar + orange plugin connector). The dark `#0d0d0d` rounded ground is added
so the light mark stays readable at LLMeter sizes on light and dark themes.

ZCode is not in LobeHub Icons or Simple Icons either. `zcode.png` is the
official app icon distributed in the ZCode desktop app bundle
(`/Applications/ZCode.app/Contents/Resources/icon.png`, app version 3.14.4),
resized to 256×256. LLMeter renders it on a white rounded chip so the dark
tile stays readable on both themes.

The Copilot CLI, Cline, Roo Code, and Kilo Code assets are the
provider-specific monochrome glyphs from `@lobehub/icons-static-svg@1.95.1`
(LobeHub/lobe-icons):

- Copilot CLI: https://icons.lobehub.com/components/github-copilot
  (`copilot.svg`, slug `githubcopilot` — the `copilot` slug is the adjacent
  Microsoft Copilot product and must not be substituted)
- Cline: https://icons.lobehub.com/components/cline (`cline.svg`)
- Roo Code: https://icons.lobehub.com/components/roo-code (`roo.svg`)
- Kilo Code: https://icons.lobehub.com/components/kilo-code (`kilo.svg`)

All four are `currentColor` glyphs; LLMeter renders them on the white
rounded chip like the other monochrome marks. Cline and Kilo Code are also
in Simple Icons, but LobeHub takes precedence per the sourcing rules.


The names and logos are trademarks of their respective owners. The assets are
used only to identify the local coding-agent session source in LLMeter.
