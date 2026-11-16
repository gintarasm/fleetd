# 0064 — GUI agent identity: a colour and an emoji from the agent id

**Status:** accepted · 2026-11-16 (M2d, issue #33)

Builds on [0029](0029-gui-core-toolkit-free.md) (toolkit-free core), [0031](0031-native-look.md) (tokens, dark and light) and [0042](0042-gui-agent-status.md) (status dots). Respects [0048](0048-light-theme-contrast.md) (light theme contrast, #68). The display name is #48's (a `name` label set at spawn), not this ADR's.

## Context

With several `claude` agents in one folder, sidebar rows and pane headers look alike. A colour per agent lets you match a row to its pane at a glance. It must be the same every time the GUI starts, on every client, and read in dark and light.

## Decision

- **Derived from the id alone**, in `core::identity`: `agent_colour(id)` is `AGENT_COLOURS[(id × 3) mod 8]`, `agent_emoji(id)` is `AGENT_EMOJI[id mod 15]`. Nothing is stored, so the colour survives GUI restarts and is the same on every client; no protocol or daemon change. A stride of 3 (coprime with 8) gives any eight consecutive ids eight different colours, and neighbouring ids never neighbouring hues. Fleetd's ids are sequential, so a hash would only add collisions between agents started together. A test pins the first few, so changing the derivation is a visible decision.
- **Eight colours**, indigo, purple, magenta, pink, gold, lime, teal and cyan, each with a dark and a light variant. They stay clear of the status colours (accent blue, orange, green, red: ADR 0042) so a swatch is not read as a status. Every variant is a non-text mark at **3:1 or more** (WCAG 1.4.11) on every surface it is drawn on: the window tint bare, hovered, selected and bell-tinted (sidebar rows) and the card (pane header and border), over a white and a black desktop, and for `Tokens::opaque()` (Reduce Transparency). The light variants were tuned against both the current light tint and ADR 0048's lighter one (`#f0f0f2` at 0.94). The colours are never used for text, so the 4.5:1 text bar does not apply.
- **Where it is drawn** (`app::agent_mark`): a 9 px rounded square at the start of the sidebar row, before the status dot (square, and a little larger than the round dot, so the two are not confused); the terminal icon in the pane header, focused or not, so every pane names its agent; and the focused pane's 1 px ring (the accent only for an empty pane). Shells get a colour too. No motion is added.
- **Emoji, optional:** `[gui] agent_emoji = true` (default off) puts the agent's emoji in place of the swatch and of the header icon. The fifteen emoji are single code points (no joiners), so each draws as one glyph through font fallback; fifteen is coprime with eight, so a colour and emoji pair repeats only every 120 ids. Until the Settings window shows it, it is set in `config.toml`.

## Consequences

- Eight colours repeat from the ninth agent; the emoji then tells agents apart. A user-picked colour or emoji per agent would need storage (a label or GUI state) and is not done here.
- The focus ring no longer uses the system accent when a pane shows an agent; the focused pane is still the only one with a coloured ring.
- If the dark or light surfaces change (a later theme issue), the contrast test in `core::identity` checks the colours against the new tokens.
