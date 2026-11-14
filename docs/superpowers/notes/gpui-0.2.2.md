# gpui 0.2.2 API findings (verified against source + compiled spike)

Source root: `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-0.2.2/` (paths below relative to `src/`).
Spike crate: `.../scratchpad/gpui-spike` (src/main.rs = ~110-line proof; also contains a passing headless test).

## 0. Build
- Compiles on `rustup run 1.99.1 cargo build`. Clean dev build: **57 s** (451 crates, M-series, 8+ cores). Adding `test-support` dev-feature: +~25 s.
- The project uses gpui **default features** (precompiled Metal shaders); `runtime_shaders` is never used (it compiles shaders at every launch and threatens the cold-start budget). `build.rs` shells out to `xcrun metal`, so building needs the Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`); without it the build fails with `cannot execute tool 'metal' due to missing Metal Toolchain`. It is installed on this Mac (also the perf-wall runner). Cargo.toml: `gpui = "=0.2.2"`.
- Only one other warning: future-incompat notes for `block 0.1.6`, `proc-macro-error2`.
- Default features also pull wayland/x11 deps' crates (cfg'd out on mac) -> big dependency tree (blade-graphics etc. appear in `cargo tree`).

## 1. Bootstrap
- `Application::new() -> Self` (app.rs:132), `Application::headless()`, `.with_assets(impl AssetSource)`, `.run<F: 'static + FnOnce(&mut App)>(self, F)` (app.rs:174). Also `Application::on_open_urls`, `on_reopen(FnMut(&mut App))` (Dock click with no windows; app.rs:198).
- `App::open_window<V: 'static + Render>(&mut self, options: WindowOptions, build_root_view: impl FnOnce(&mut Window, &mut App) -> Entity<V>) -> anyhow::Result<WindowHandle<V>>` (app.rs:943).
- `WindowOptions` (platform.rs:1089, has Default): `window_bounds: Option<WindowBounds>`, `titlebar: Option<TitlebarOptions>`, `focus: bool`, `show: bool`, `kind: WindowKind`(Normal/PopUp/Floating), `is_movable`, `is_resizable`, `is_minimizable`, `display_id`, `window_background: WindowBackgroundAppearance`(Opaque/Transparent/Blurred..), `app_id`, `window_min_size: Option<Size<Pixels>>`, `window_decorations`, `tabbing_identifier: Option<String>`.
- `TitlebarOptions { title: Option<SharedString>, appears_transparent: bool, traffic_light_position: Option<Point<Pixels>> }` (platform.rs:1248).
- `WindowBounds::{Windowed(Bounds<Pixels>), Maximized(..), Fullscreen(..)}` (platform.rs:1188). `Bounds::centered(display_id: Option<DisplayId>, size: Size<Pixels>, cx: &App)` (geometry.rs:771), `Bounds::maximized`, `Bounds::new(origin, size)`.
- Window fns: `window.set_window_title(&str)`, `window.window_bounds()`, `window.bounds()`, `window.viewport_size()`, `window.scale_factor()`.

## 2. Quit, activation policy, tray, menus
- **No automatic quit when last window closes and no `quit_mode` setting exists.** Closing just removes the window and fires `App::on_window_closed(&self, FnMut(&mut App)) -> Subscription` (app.rs:1806; fired at app.rs:1378 after removal). The mac NSApplicationDelegate does not implement applicationShouldTerminateAfterLastWindowClosed (AppKit default: NO). So by default **the app keeps running with zero windows** (Dock icon stays). To quit on last close: `cx.on_window_closed(|cx| if cx.windows().is_empty() { cx.quit() }).detach();` (spike does this). `App::quit(&self)` app.rs:749, `shutdown()`, `on_app_quit` (100 ms budget, SHUTDOWN_TIMEOUT).
- Reopen window on Dock click: `Application::on_reopen` (called only when `!has_visible_windows`, platform/mac/platform.rs:1409).
- **Activation policy: not configurable through gpui.** `did_finish_launching` hard-codes `setActivationPolicy_(NSApplicationActivationPolicyRegular)` (platform/mac/platform.rs:1390) and then immediately invokes the `run` callback (line ~1402). So you can override it from inside the `run` closure via objc2/cocoa: `[NSApp setActivationPolicy: Accessory]` (not run-tested; ordering is from source). `App::activate(ignoring_other_apps: bool)`, `hide()`, `hide_other_apps()` exist.
- **Status item / tray: none.** `platform/mac/status_item.rs` exists in the tarball but is NOT in any `mod` list (platform/mac.rs) - dead legacy file written against an old API (uses `crate::geometry::rect::RectF`); it does not compile into the crate. No public tray API on any platform.
- Menus: `App::set_menus(&self, Vec<Menu>)` (app.rs:1840), `get_menus`, `App::set_dock_menu(&self, Vec<MenuItem>)` (app.rs:1850), `add_recent_document`-style fns nearby. `Menu { name: SharedString, items: Vec<MenuItem> }` (platform/app_menu.rs:5). `enum MenuItem { Separator, Submenu(Menu), SystemMenu(OsMenu), Action{name, action: Box<dyn Action>, os_action: Option<OsAction>} }`; ctors `MenuItem::separator()`, `::submenu(Menu)`, `::os_submenu(name, SystemMenuType)`, `::action(name, impl Action)`, `::os_action(name, action, OsAction)` (Cut/Copy/Paste/SelectAll/Undo/Redo map to NSResponder selectors).

## 3. Custom Element + text
- `trait Element: 'static + IntoElement` (element.rs:51):
  - `type RequestLayoutState: 'static; type PrepaintState: 'static;`
  - `fn id(&self) -> Option<ElementId>; fn source_location(&self) -> Option<&'static std::panic::Location<'static>>;` (both required)
  - `fn request_layout(&mut self, id: Option<&GlobalElementId>, inspector_id: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, Self::RequestLayoutState)`
  - `fn prepaint(&mut self, id, inspector_id, bounds: Bounds<Pixels>, request_layout: &mut RequestLayoutState, window: &mut Window, cx: &mut App) -> PrepaintState`
  - `fn paint(&mut self, id, inspector_id, bounds, request_layout: &mut _, prepaint: &mut _, window: &mut Window, cx: &mut App)`
  - Need also `impl IntoElement { type Element = Self; fn into_element(self) -> Self }`.
  - Layout: `window.request_layout(style: Style, children: impl IntoIterator<Item=LayoutId>, cx: &mut App) -> LayoutId` (window.rs:3217); `style.size.width = relative(1.).into()`.
- Quads: `window.paint_quad(PaintQuad)` (window.rs:2839, paint phase only); `fill(bounds: impl Into<Bounds<Pixels>>, background: impl Into<Background>) -> PaintQuad` (window.rs:5078); `outline(bounds, border_color, BorderStyle)`; `PaintQuad{bounds, corner_radii, background, border_widths, border_color, border_style}`. Colors: `hsla(h,s,l,a)`, `rgb(0xRRGGBB)`, `white()`, `black()`; `Hsla`.
- `window.text_system() -> &Arc<WindowTextSystem>` (window.rs:1435; derefs to `TextSystem`).
- `WindowTextSystem::shape_line(&self, text: SharedString, font_size: Pixels, runs: &[TextRun], force_width: Option<Pixels>) -> ShapedLine` (text_system.rs:365). Text must contain no '\n' (debug_assert). `force_width` = force every glyph advance to that width (use cell width for a terminal grid). Also `shape_text(text, font_size, runs, wrap_width: Option<Pixels>, line_clamp: Option<usize>) -> Result<SmallVec<[WrappedLine;1]>>`, `layout_line(&str, font_size, &[TextRun], force_width) -> Arc<LineLayout>`.
- `TextRun { len: usize /*bytes*/, font: Font, color: Hsla, background_color: Option<Hsla>, underline: Option<UnderlineStyle>, strikethrough: Option<StrikethroughStyle> }` (text_system.rs:733). Runs' lens must sum to text.len().
- `ShapedLine::paint(&self, origin: Point<Pixels>, line_height: Pixels, window: &mut Window, cx: &mut App) -> Result<()>` (text_system/line.rs:63); `paint_background(...)` same args (line 86) paints run background_colors. `WrappedLine::paint(origin, line_height, align: TextAlign, bounds: Option<Bounds<Pixels>>, window, cx)`. ShapedLine has `x_for_index`/`index_for_x` helpers (via Deref to LineLayout).
- `Font { family: SharedString, features: FontFeatures, fallbacks: Option<FontFallbacks>, weight: FontWeight, style: FontStyle }`; ctor `font(family: impl Into<SharedString>) -> Font` (text_system.rs:807), `.bold()`, etc. System font fallback stack exists; `all_font_names()`. Family "Menlo" resolved fine at compile level (not run).
- Metrics: `TextSystem::resolve_font(&self, &Font) -> FontId` (pub; `font_id` is private - use resolve_font; panics if neither font nor fallbacks resolve), `advance(&self, FontId, font_size: Pixels, ch: char) -> Result<Size<Pixels>>` (use `.width` for monospace cell width), `em_advance(font_id, size) -> Result<Pixels>`, `em_width` (typographic bounds of 'm', not advance), `ch_width`, `ascent`, `descent`, `bounding_box`. `window.line_height()` is from the *text style* (rem-based), not font metrics; for a terminal compute your own line height (ascent+descent, or size*factor).

## 4. Text input / IME
- `trait EntityInputHandler: 'static + Sized` (input.rs), all required:
  - `text_for_range(&mut self, range: Range<usize> /*utf16*/, adjusted_range: &mut Option<Range<usize>>, window, cx: &mut Context<Self>) -> Option<String>`
  - `selected_text_range(&mut self, ignore_disabled_input: bool, window, cx) -> Option<UTF16Selection>` (`UTF16Selection{range, reversed}`)
  - `marked_text_range(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>>`
  - `unmark_text(&mut self, window, cx)`
  - `replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, window, cx)`  <- committed text (also plain typed chars)
  - `replace_and_mark_text_in_range(&mut self, range: Option<Range<usize>>, new_text: &str, new_selected_range: Option<Range<usize>>, window, cx)`  <- IME marked/preedit text
  - `bounds_for_range(&mut self, range_utf16, element_bounds: Bounds<Pixels>, window, cx) -> Option<Bounds<Pixels>>` (candidate-window position)
  - `character_index_for_point(&mut self, Point<Pixels>, window, cx) -> Option<usize>`
- `ElementInputHandler::new(element_bounds: Bounds<Pixels>, view: Entity<V>)`; `V: EntityInputHandler` gives `InputHandler`. (Raw `InputHandler` trait in platform.rs ~1040 has the same methods on `&mut App`, plus defaulted `apple_press_and_hold_enabled() -> bool` returning true.)
- Register in **paint phase**: `window.handle_input(&focus_handle, ElementInputHandler::new(bounds, entity), cx)` (window.rs:3400). Only registers if `focus_handle.is_focused(window)`. Must be called every frame.
- **Key routing gotcha** (platform/mac/window.rs:1683-1765): KeyDown first goes to gpui dispatch (key bindings + `on_key_down`). If any handler stops propagation (`cx.stop_propagation()`/action handled) the IME/input handler never sees it. If not handled and not key_equivalent, the event goes to NSTextInputContext -> `replace_text_in_range` (printable chars, incl. plain ASCII) or `do_command_by_selector`. While marked text is active (`marked_text_range()` is Some) keys go to the IME FIRST. Keys with key_char None and no ctrl/fn (arrows, escape, backspace) are also offered to the IME before gpui. Terminal strategy: handle keys in `on_key_down`, `stop_propagation()` for what you encode yourself, let printable non-composing keys flow to `replace_text_in_range` (or handle both and stop propagation, but then IME composition for CJK will not start unless you let it through).
- Verified in headless test: an `on_key_down` that does NOT stop propagation still results in `replace_text_in_range("a")` being called for plain keys.

## 5. Keyboard
- `KeyDownEvent { keystroke: Keystroke, is_held: bool }`, `KeyUpEvent{keystroke}`, `ModifiersChangedEvent{modifiers, capslock}` (interactive.rs).
- `Keystroke { modifiers: Modifiers, key: String, key_char: Option<String> }` (platform/keystroke.rs:18).
- `Modifiers { control: bool, alt: bool, shift: bool, platform: bool /*cmd*/, function: bool }` (keystroke.rs:447).
- **Option-modified keys deliver the base key**: doc + mac code (platform/mac/events.rs:300-445): option-s -> `key: "s"`, `modifiers.alt = true`, `key_char: Some("ß")` (key_char = layout-translated char with shift/option; None when ctrl/cmd/fn held). Verified headless: `alt-s` -> key="s", char=None (simulated; simulation does not produce the real "ß"). Caveats: shifted non-letter keys come as the shifted char with `shift=false` (shift-1 -> key "!"); cmd+shift on letters keep shift=true; special names: "space","tab","enter","escape","backspace","up",... ; no option-as-meta setting in gpui (you implement it via `alt` + `key`).
- Actions: `actions!(namespace, [A, B])` macro (action.rs:24) -> unit structs; `#[derive(Action)]` for data actions. Namespace may be omitted.
- `KeyBinding::new<A: Action>(keystrokes: &str, action: A, context: Option<&str>) -> KeyBinding` (keymap/binding.rs:33; panics on parse error; strings like "cmd-c", "ctrl-shift-a", "alt-s").
- `App::bind_keys(&mut self, impl IntoIterator<Item=KeyBinding>)` (app.rs:1677). Global handler: `App::on_action<A>(&mut self, Fn(&A,&mut App))` (app.rs:1696). Element-level: `.on_action(cx.listener(|this, a: &A, window, cx| ..))` (div.rs:349).
- Key context: `.key_context("Terminal")` on any `InteractiveElement` (div.rs:658), `Window::set_key_context(KeyContext)` in paint (window.rs:3333); binding context predicate e.g. `Some("Terminal && !Editing")`. Element must have `.track_focus(&focus_handle)` (focusable) to receive key events.
- Raw key listeners: `.on_key_down(listener)` on div (div.rs:387; `Fn(&KeyDownEvent,&mut Window,&mut App)`), `window.on_key_event::<KeyDownEvent>(|ev, phase: DispatchPhase, window, cx| ..)` (window.rs:3444, paint phase), `cx.observe_keystrokes`.

## 6. Mouse
- `MouseDownEvent { button: MouseButton, position: Point<Pixels>, modifiers, click_count: usize, first_mouse: bool }`; `MouseUpEvent{button, position, modifiers, click_count}`; `MouseMoveEvent { position, pressed_button: Option<MouseButton>, modifiers }`; `ScrollWheelEvent { position, delta: ScrollDelta, modifiers, touch_phase: TouchPhase }`; `MouseExitEvent` (interactive.rs).
- `enum ScrollDelta { Pixels(Point<Pixels>), Lines(Point<f32>) }`; `.precise()`, `.pixel_delta(line_height) -> Point<Pixels>`, `.coalesce(other)`. Trackpads -> Pixels, wheel mice -> Lines. ctrl+left-click is converted to Right click on mac (window.rs ~1790).
- Element-level listener (paint phase): `window.on_mouse_event::<E: MouseEvent>(impl FnMut(&E, DispatchPhase, &mut Window, &mut App) + 'static)` (window.rs:3421). You must filter yourself: `if phase == DispatchPhase::Bubble && bounds.contains(&ev.position)`. Div helpers do hit-testing for you: `.on_mouse_down(MouseButton, Fn(&MouseDownEvent,&mut Window,&mut App))`, `.on_mouse_move(..)`, `.on_scroll_wheel(..)`, `.on_mouse_up`.
- Mouse positions are in window coordinates (logical pixels), y-down.

## 7. Clipboard
- `App::write_to_clipboard(&self, item: ClipboardItem)` (app.rs:1041); `App::read_from_clipboard(&self) -> Option<ClipboardItem>` (app.rs:1053). `ClipboardItem::new_string(String)` (platform.rs:1519), `new_string_with_metadata`, `new_image`; read: `item.text() -> Option<String>`, `entries()`. Also `write_to_primary/read_from_primary` are Linux-only.

## 8. Async / threading
- Foreground (main thread, !Send futures): `App::spawn<AsyncFn, R>(&self, f: AsyncFn) -> Task<R> where AsyncFn: AsyncFnOnce(&mut AsyncApp) -> R + 'static` (app.rs:1417) - async closure, so write `cx.spawn(async move |cx| { ... })`. Entity-scoped: `Context<T>::spawn(AsyncFnOnce(WeakEntity<T>, &mut AsyncApp) -> R)` (context.rs:237), `spawn_in(window, ..)` gives `AsyncWindowContext`. Tasks cancel on drop: `.detach()` or store them.
- Background: `cx.background_executor() -> &BackgroundExecutor` (app.rs:1402); `.spawn<R: Send+'static>(impl Future + Send + 'static) -> Task<R>` (executor.rs:145); `.timer(Duration) -> Task<()>` (executor.rs:357).
- Timers: `gpui::Timer` is re-exported `smol::Timer` (gpui.rs:94): `Timer::after(Duration).await` works inside cx.spawn; `cx.background_executor().timer(d).await` also (the latter is deterministic in tests).
- Cross-thread push: AsyncApp/Entity are NOT Send. Pattern: std thread (e.g. PTY reader) -> channel -> foreground task awaiting it. gpui does not re-export a channel; add `smol`/`async-channel`/`futures` dep and use an async receiver (it wakes the foreground executor from any thread): 
  `cx.spawn(async move |this, cx| { while let Ok(chunk) = rx.recv().await { this.update(cx, |t, cx| { t.feed(chunk); cx.notify(); }).ok(); } })` from `Context<T>`.
  `std::sync::mpsc` receiver can't be awaited; either poll with a `Timer`, or use it from a `background_executor().spawn` blocking task (blocks a pool thread).
- Coalescing: `cx.notify()` marks the entity dirty; multiple notifies before the next frame collapse into one redraw (frame is driven by the display link). To limit work per frame: drain the channel in a loop with `try_recv` after the first `recv().await`, then notify once. `window.request_animation_frame()` (window.rs:1654) = `on_next_frame(|_, cx| cx.notify(current_view))` - for continuous animation (cursor blink: prefer a Timer + notify). `window.on_next_frame(FnOnce(&mut Window,&mut App))` (window.rs:1644). `cx.defer(..)`, `window.defer(cx, ..)`.

## 9. Dialogs
- `App::prompt_for_paths(&self, PathPromptOptions { files: bool, directories: bool, multiple: bool, prompt: Option<SharedString> }) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>>` (app.rs:1116; `Ok(None)` = cancelled). `prompt_for_new_path(&Path, Option<&str>)`.
- `Window::prompt<T: Clone + Into<PromptButton>>(&mut self, level: PromptLevel, message: &str, detail: Option<&str>, answers: &[T], cx: &mut App) -> oneshot::Receiver<usize>` (window.rs:4141). `PromptLevel::{Info, Warning, Critical}`; `&str: Into<PromptButton>`; `PromptButton::{ok,cancel,new}`. On macOS uses native NSAlert (platform_window.prompt); returns index of chosen answer. `cx.set_prompt_builder` for custom.
- The oneshot receiver is `futures::channel::oneshot`; await in `cx.spawn`.

## 10. Appearance
- `Window::appearance(&self) -> WindowAppearance` (window.rs:1711); `App::window_appearance(&self)` (app.rs:1029). `enum WindowAppearance { Light, VibrantLight, Dark, VibrantDark }`.
- `Context<T>::observe_window_appearance(&self, window: &mut Window, FnMut(&mut T,&mut Window,&mut Context<T>)) -> Subscription` (context.rs ~470); `Window::observe_window_appearance(&self, FnMut(&mut Window,&mut App)) -> Subscription` (window.rs:1321). Also `observe_window_activation` (focus/active changes).

## 11. Testing
- Yes: Cargo feature `test-support` (Cargo.toml: enables leak-detection, rand, util/test-support, wayland, x11 ... so it pulls extra crates, +~25 s build). Works from crates.io 0.2.2. `#[gpui::test]` macro (re-exported from gpui_macros), `TestAppContext`, `VisualTestContext`.
- Headless: runs on `TestPlatform` (platform/test), no window server, no Metal. Verified: test passed in 0.01 s.
- Working example (in spike):
  ```rust
  use super::Term; use gpui::TestAppContext;   // DON'T `use super::*` with gpui::* glob: infinite macro recursion "recursion limit reached while expanding #[test]"
  #[gpui::test]
  fn keys(cx: &mut TestAppContext) {
      let (view, cx) = cx.add_window_view(|_w, cx| Term { .. });
      cx.update(|w, cx| w.focus(&view.read(cx).focus.clone()));   // must focus first
      cx.simulate_keystrokes("a b alt-s enter");
      assert_eq!(view.read_with(cx, |t, _| t.cursor.0), 4);
  }
  ```
  `VisualTestContext`: `simulate_keystrokes(&str)`, `simulate_input(&str)` (types text via input handler), `simulate_mouse_move/down/up`, `simulate_event`, `draw(origin, available_space, build)`, `run_until_parked()`; `TestAppContext::add_empty_window`, `add_window_view`, `dispatch_keystroke(window, Keystroke)`.
- Caveats: simulated keystrokes use `with_simulated_ime` (key_char filled for plain keys; alt-modified gets None). Real macOS IME/option composition is not simulated. Custom Element paint runs only if you draw (add_window_view draws on update/run_until_parked).

## 12. Resize / bounds
- Resize: `Context<T>::observe_window_bounds(&self, window: &mut Window, FnMut(&mut T,&mut Window,&mut Context<T>)) -> Subscription` (context.rs:429; fires on resize/move). Current: `window.bounds() -> Bounds<Pixels>`, `window.viewport_size()`, `window.window_bounds()`; `window.resize(Size<Pixels>)`.
- Element bounds: passed as `bounds: Bounds<Pixels>` into `Element::prepaint`/`paint` (window coordinates, logical px). For a div child use `canvas(prepaint, paint)` or a custom Element. Convert to cols/rows in `prepaint` (`(bounds.size.width / cell_w).floor()`), then tell the PTY. Scale factor: `window.scale_factor()`; all APIs are logical pixels.

## 13. Raw AppKit access / dependency versions
- Yes (view handle only): `impl HasWindowHandle for Window` (window.rs:4845) -> `window.window_handle()` gives `raw_window_handle::RawWindowHandle::AppKit(AppKitWindowHandle)` whose `ns_view` is the NSView pointer (platform/mac/window.rs:1548; `NSView.window` -> NSWindow). Also `HasDisplayHandle`. `raw-window-handle = "0.6"` (lock: 0.6.2) - add the same dep to call `.as_raw()`. `AsyncWindowContext`/`WindowHandle::update` give access to the `Window`. **No NSApplication/NSWindow accessor and no gpui hook for NSStatusItem**; do it yourself via objc2 on the main thread (e.g. inside the `Application::run` closure, which is the main thread after didFinishLaunching).
- gpui's own mac deps (Cargo.toml:595-650; lock): `cocoa =0.26.0` (also 0.25.0 transitively), `cocoa-foundation =0.2.0`, `core-foundation =0.10.0` (+0.9.4 transitive), `core-foundation-sys 0.8.6`, `core-graphics 0.24` (+0.23.2), `core-text 21`, `core-video 0.4.3`, `metal 0.29`, `foreign-types 0.5`, `block 0.1`, **`objc 0.2` (0.2.7)** is what gpui uses for its AppKit code. `objc2 0.6` (lock 0.6.5) and `objc2-metal 0.3` are optional (macos-blade); in the default lock the tree still contains `objc2 0.6.5`, `objc2-app-kit 0.3.2`, `objc2-foundation 0.3.2`, `objc2-core-foundation 0.3.2`, `objc2-metal 0.3.2`, `objc2-quartz-core 0.3.2` (via blade-graphics). So pin `objc2 = "0.6"`, `objc2-app-kit = "0.3"`, `objc2-foundation = "0.3"` to share versions with no duplication.
