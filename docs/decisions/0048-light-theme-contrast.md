# 0048 — Light theme: WCAG AA text on every surface

**Status:** accepted · 2026-11-16 (M2d, #47)

**Amends:** [0031](0031-native-look.md) (area 2, material, and area 3, the colour tokens in `core::theme`).

## Context

ADR 0031 tuned the window in dark first; light followed with its own tokens but was never reviewed. Its contrast tests asked for 4.5:1 only for primary text on the window tint and 3:1 for secondary text, and nothing for tertiary text, the accent, the terminal's ANSI colours or the running dot. Measured over the darkest desktop the blur can show (black: a dark wallpaper behind a light window is common), the light tokens fell well short of WCAG AA:

| light token, worst surface | before | after |
|---|---|---|
| text | 8.67 | 11.14 |
| secondary (paths, second lines) | 3.26 | 6.01 |
| tertiary (timestamps, hints, exited rows, placeholders) | 1.68 | 4.54 |
| accent as text (the bar's buttons), system blue | 2.07 | 4.50 |
| white label on the accent (primary button) | 4.02 | 6.80 |
| running dot (non-text, 3:1) | 2.26 | 3.02 |
| danger text on a sheet | 5.34 | 5.34 |

"Worst surface" is the lowest contrast over a white and a black desktop on the window tint (bare, hovered and selected, as sidebar rows are), the cards, the sheets and their fields. Pressed fills are left out: they last only while the button is held.

## Decision

- **Every light text token meets AA (4.5:1) on every light surface**, tertiary included: ours is real text (the sidebar's timestamps and exited rows, hints, placeholders), not only disabled labels. Meaningful marks (the running dot, the danger outline of the toast) meet 3:1. `core::theme` tests this for the light tokens and for `opaque()` (Reduce Transparency).
- **The light window tint is lighter and more opaque**, `#f0f0f2` at 0.94 (was `#eeeeee` at 0.84). Over a white desktop it looks as before (`#f1f1f3`); over a black one it no longer sinks to mid-grey (`#e2e2e3`, was `#c8c8c8`), which is what made AA reachable without merging secondary and tertiary into one grey. It stays translucent.
- **Text greys:** secondary `#48484e`, tertiary `#5a5a5e`, each kept a visible step (by luminance) from the next level.
- **The accent is made legible in light** (`Tokens::legible_accent`, applied by `Tokens::with_accent` and so to the user's `controlAccentColor`): scaled toward black, keeping its hue, just enough to reach 4.5:1 on every light surface. The system blue becomes `#0058b8`, yellow `#715700`; an accent already dark enough is kept. Dark mode keeps the accent as it is (there the tension runs both ways: text on a dark surface wants it lighter, a white button label wants it darker). The selection tint follows the accent at 0.22 alpha (was 0.28), so selected terminal text stays at least 7:1 and selected ANSI colours at least 3:1.
- **The light ANSI palette** keeps every colour but white (7) and bright white (15) at 4.5:1 on the card; green, yellow, cyan and the bright variants were darkened. White and bright white stay pale: on a light terminal they serve as backgrounds, as in Terminal.app's light profiles.
- The running dot is a little darker green (`#23873c`). Borders, shadows, fills, the field well and the marked-text colour are unchanged: they already read, and the focus ring takes the now darker accent.

## Consequences

- Light looks slightly crisper and its accent deeper than the system's own controls; a pale system accent (yellow, orange) turns noticeably darker in our window. That is the cost of AA text in the accent.
- The dark tokens are unchanged. Dark tertiary text is below AA on the cards (about 3.2:1); that is a dark-theme question for a later issue.
- Contrast is checked against flat backdrops; the real blur of a busy wallpaper sits between the white and black cases tested.
