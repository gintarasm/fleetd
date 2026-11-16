# 0049 — Liquid glass behind the GUI (spike)

**Status:** proposed · 2026-11-16 (M2d, issue #31)

**Amends:** [0031](0031-native-look.md) (material: the blur stays the default; glass is an opt-in alternative) and [0044](0044-gui-settings-window.md) (a new `[gui] glass` key and its control). Builds on [0029](0029-gui-core-toolkit-free.md).

## Context

The workspace window's material is gpui's one `Blurred` backdrop (a colourless `NSVisualEffectView`) under the theme's translucent tint (0031). macOS 26 added liquid glass, `NSGlassEffectView`, which gpui 0.2.2 knows nothing about. Issue #31 asked for a time-boxed spike: put glass behind the gpui content, and separately behind the sidebar and the bar, keep it in step with layout and appearance, fall back to the blur on older macOS and under Reduce Transparency, add a Settings toggle, and record what works and what it costs.

## Decision

Ship the glass **behind a setting, off by default**, until the owner has looked at it and the perf gates have been measured.

- **`[gui] glass`**: `"off"` (default; today's blur), `"window"` (one glass pane behind all the gpui content) or `"chrome"` (glass behind the bar and the sidebar; the panes' area keeps the blur and the tint). Settings › Appearance › Liquid glass, three pills (Off, Window, Sidebar and bar). It applies live, without a restart.
- **The model is in the core** (`core::glass`, toolkit-free, unit-tested): `Backdrop::resolve(choice, glass_available, reduce_transparency)` picks `Opaque`, `Blur`, `GlassWindow` or `GlassChrome`; each backdrop says whether the window is blurred, which glass regions it has (`Window`, or `Bar` and `Sidebar`), and where gpui still paints the tint (`Tint { chrome, content }`). The regions' frames come from the fixed chrome metrics (`style::chrome::BAR_HEIGHT`, `SIDEBAR_WIDTH`), with which edges grow on a resize.
- **The shell** (`app::native::set_glass`) adds one `NSGlassEffectView` per region to the window's content view, just under gpui's `GPUIView` (so, for `chrome`, above gpui's blur view), tags each with an identifier (`fleet.glass.*`), and removes the ones no longer wanted. It is stateless and idempotent: called again it reuses and retints the views. Each view's autoresizing mask (the window: width and height; the bar: width, pinned to the top; the sidebar: height, pinned to the left) keeps it in step with window resizes and full screen, so nothing moves it per frame. The layout has no other moving parts today (the sidebar is fixed-width and always shown); if it becomes resizable or collapsible, its frame must be pushed from the layout instead.
- **The window background follows the backdrop**: `Blurred` for `Blur` and `GlassChrome`, `Transparent` for `GlassWindow` (gpui then drops its blur view and the glass is the only material), `Opaque` under Reduce Transparency. A live change calls `Window::set_background_appearance`; the blur radius setting (`[gui] blur`) applies only when the window is blurred.
- **Tint.** Under glass gpui paints no window tint (in `chrome` it paints it only behind the panes); the glass carries the theme's window colour as its `tintColor` at half the window tint's alpha (`GLASS_TINT_SHARE` = 0.5) so the glass keeps its own edges and highlights instead of turning into a flat fill. The tint is re-applied on every appearance or theme change (the same path as the blur, `Workspace::apply_look`); the glass itself follows the window's effective appearance. When glass is off nothing is wrapped and the root keeps its single fill, so the default frame is unchanged.
- **Fallbacks.** Glass is used only where the runtime has the class (`objc_getClass("NSGlassEffectView")`, i.e. macOS 26 and later; asked of the runtime, not the OS version, so the typed objc2 binding is never messaged on an older system). Without it every choice falls back to the blur. **Reduce Transparency** wins over everything (the window opens opaque, as before, and stays so until reopened); the Settings control is then inert with a note, as it is on macOS < 26.

### What the spike verified

- On this Mac (macOS 26.6.2) the views are created, ordered and masked as intended, checked by dumping the content view's subviews from the running app: `window` gives `[NSGlassEffectView, GPUIView]` (both width+height sizable); `chrome` gives `[BlurredView, glass.bar, glass.sidebar, GPUIView]` with the bar 40 pt tall at the top (width-sizable, flexible bottom margin) and the sidebar 240 pt wide below it (height-sizable, flexible right margin).
- gpui keeps drawing over the glass: its view stays on top and first responder; nothing in gpui reorders the content view's subviews.
- The control switches between Off, Window and Sidebar and bar live and writes the key (shell test `glass_choice_applies_live_where_the_system_has_it`).

### What the spike did not verify

- **The look.** The Mac's screen was locked for the whole spike, so no screenshots could be taken and the side-by-side comparison with the blur was not done; the 0.5 tint share is a starting value, not a tuned one. Before deciding, the owner should open the window in each mode, dark and light, over a bright and a busy desktop.
- **Legibility.** The theme's WCAG tests (0031) cover text on the tint over a black and a white desktop. Glass changes its look with what is behind it and cannot be modelled in those tests, so text over glass has no contrast guarantee; `chrome` keeps the tested tint behind the panes, `window` does not.
- **Glass-aware content.** AppKit renders a glass view's own `contentView` with glass-aware vibrancy. gpui's view cannot be moved into it (gpui owns its view, its Metal layer and first-responder status), so gpui's text sits *over* the glass, not *in* it: no vibrancy, no glass highlights on our controls, and pane cards and sheets stay gpui-drawn cards rather than glass.
- **Accessibility settings beyond Reduce Transparency** (Increase Contrast) are not handled; Reduce Transparency is still read only when the window opens (0031).

## Consequences

- **Performance is not measured** (`PERF_CI` is off, [0041](0041-perf-ci-switch.md)). Expected cost: no per-frame work in our code (the views are placed once per look change; render adds three wrapper divs only while glass is on); at window open one runtime class lookup and up to two view allocations. The window server's cost is what matters: with `window`, a transparent window over a glass view (similar in kind to today's blur view); with `chrome`, the blur view *plus* two glass views, composited under a non-opaque Metal layer every frame. **`input_to_render_ns`, `input_to_render_flood50_ns` and `gui_cold_start_ns` must be measured with each mode (`fleet-perf run --suite wall`, `compare`) before glass is turned on by default**; a regression needs the owner's approval (CLAUDE.md).
- Recommendation: keep the code (it is small, isolated in `core::glass` and `native::set_glass`, and off by default). If glass is turned on by default after the look and perf checks, prefer `chrome`: the panes keep the tuned, contrast-tested blur and tint, the glass goes where macOS 26 itself puts it (sidebars and toolbars), and the panes are cards over the backdrop anyway. `window` mostly matters if the owner wants the whole window to read as glass; it drops gpui's tint and with it the contrast guarantee. If the look does not earn it, delete `core::glass`, `set_glass` and the control; nothing else depends on them.
- A gpui upgrade that adds its own glass background would replace `set_glass`; the core model and the setting would carry over.
