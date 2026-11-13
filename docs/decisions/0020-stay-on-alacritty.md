# 0020 — Stay on alacritty_terminal; close snapshot gaps in our own code

**Status:** accepted · 2026-11-13 (user decision)

**Context:** We ran a spike (branch `spike/libghostty`, commit `20b046a`) to see whether Ghostty's `libghostty-vt` could replace `alacritty_terminal` as the node's headless terminal ([0005](0005-terminal-state-on-node.md)). It used `libghostty-vt =0.2.2`, Ghostty commit `a887df42`, Zig 0.15.2. Measured on the same machine and fixtures as the instr benches:

| | alacritty | ghostty |
|---|---:|---:|
| feed 64 KiB (wall median) | 429 µs | 288 µs (−33%) |
| snapshot 120x40 (wall median) | 36 µs | 53 µs (+48%) |
| feed / snapshot instructions (callgrind, linux/arm64) | 6.97 M / 0.84 M | 4.05 M / 1.16 M |
| `fleetd` release binary | 4.1 MB | 5.1 MB |
| cold release build | 25 s | 76 s |
| term unit tests | 23/23 | 18/23 |

- **Snapshot correctness:** Ghostty's VT formatter has two visible bugs: styles bleed into the blank cells after a styled run, and rows holding only background-coloured blanks are dropped. Ghostty's real snapshot encode/restore API exists only on newer Ghostty, which needs Zig 0.17. It is also a binary format, so every client would need libghostty, which breaks [0018](0018-m1-protocol-and-node.md)'s "clients need only a VT parser".
- **Threading:** the handles are `!Send`. With an owner thread per agent, the round trips (about 7 µs each, after every output chunk) erase most of the feed win.
- **Build:** Zig is pinned to Ghostty's major.minor (Homebrew's 0.17.1 is rejected by that commit). Uncached builds fetch a Ghostty clone plus about 470 MB of Zig packages, and the build directory grows by about 300 MB. The Rust wrapper is pre-1.0 and has a known bug: `format_buf` reports `required: 0`, so a retry loop spins forever.

**Decision:**
- Keep `alacritty_terminal`.
- Close the snapshot and attach gaps listed in [0018](0018-m1-protocol-and-node.md) in our own code, using alacritty's public API only:
  - the detach key under the kitty keyboard protocol (CSI-u and modifyOtherKeys forms of Ctrl-\);
  - the saved cursor (DECSC);
  - the current kitty keyboard flags;
  - the primary screen behind an active alt screen, rendered via `Term::swap_alt` round trips.
- Do not patch, fork or vendor alacritty. The scroll region (DECSTBM) stays uncarried, because alacritty exposes no accessor for it.

**Revisit when** Ghostty's VT formatter bugs are fixed, or it ships a stable, text-based (or otherwise client-neutral) snapshot format, and its Zig requirement stabilises; or when we need state alacritty cannot expose.

**Consequences:** No new toolchain. The snapshot covers what full-screen agent UIs need on reattach, except the scroll region, charsets, tab stops and hyperlinks.
