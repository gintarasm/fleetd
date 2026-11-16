//! An agent's identity at a glance (ADR 0064): a colour, and optionally an
//! emoji, derived from its id alone, so an agent keeps them across GUI
//! restarts and on every client. The shell draws the colour as a swatch on
//! the sidebar row, on the pane header's icon and as the focused pane's
//! ring; the emoji replaces the swatch and the icon when `[gui] agent_emoji`
//! is on.

use super::theme::{Appearance, Rgba, rgb};

/// One of the agent colours, in its dark and light variant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentColour {
    pub name: &'static str,
    pub dark: Rgba,
    pub light: Rgba,
}

impl AgentColour {
    /// The variant drawn in `appearance`.
    pub fn for_appearance(self, appearance: Appearance) -> Rgba {
        match appearance {
            Appearance::Dark => self.dark,
            Appearance::Light => self.light,
        }
    }
}

/// The agent colours, in hue order. They stay clear of the status colours
/// (the accent blue, orange, green and red of ADR 0042) so a swatch is not
/// read as a status. Each variant is a non-text mark at 3:1 or more on every
/// surface it is drawn on (WCAG 1.4.11), tested below.
pub const AGENT_COLOURS: [AgentColour; 8] = [
    AgentColour {
        name: "indigo",
        dark: rgb(0x9b9bf1),
        light: rgb(0x4242f0),
    },
    AgentColour {
        name: "purple",
        dark: rgb(0xc489ee),
        light: rgb(0x8a12e0),
    },
    AgentColour {
        name: "magenta",
        dark: rgb(0xeb75e1),
        light: rgb(0xaa0e9d),
    },
    AgentColour {
        name: "pink",
        dark: rgb(0xed7ea6),
        light: rgb(0xba0f4e),
    },
    AgentColour {
        name: "gold",
        dark: rgb(0xc49d1c),
        light: rgb(0x745b09),
    },
    AgentColour {
        name: "lime",
        dark: rgb(0x73b319),
        light: rgb(0x406808),
    },
    AgentColour {
        name: "teal",
        dark: rgb(0x1ab5a0),
        light: rgb(0x096a5d),
    },
    AgentColour {
        name: "cyan",
        dark: rgb(0x1eb0d4),
        light: rgb(0x0a667d),
    },
];

/// Steps through [`AGENT_COLOURS`] three at a time: coprime with eight, so
/// any eight consecutive ids get eight different colours, and neighbours
/// are never neighbouring hues.
const COLOUR_STRIDE: u64 = 3;

/// Single code points (no joiners or modifiers), so each draws as one
/// glyph in any font fallback. Fifteen, coprime with the eight colours, so
/// a colour and emoji pair repeats only every 120 ids.
pub const AGENT_EMOJI: [&str; 15] = [
    "🦊", "🐙", "🦉", "🐢", "🐝", "🦋", "🐳", "🦀", "🐌", "🦔", "🐧", "🦜", "🐸", "🦄", "🐞",
];

/// Agent `id`'s colour.
pub fn agent_colour(id: u64) -> AgentColour {
    // 2^64 is a multiple of eight, so wrapping keeps the cycle unbroken.
    AGENT_COLOURS[(id.wrapping_mul(COLOUR_STRIDE) % AGENT_COLOURS.len() as u64) as usize]
}

/// Agent `id`'s emoji.
pub fn agent_emoji(id: u64) -> &'static str {
    AGENT_EMOJI[(id % AGENT_EMOJI.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{AGENT_COLOURS, AGENT_EMOJI, agent_colour, agent_emoji};
    use crate::core::theme::{Rgba, Tokens, contrast, rgb};

    /// Every opaque colour a swatch, the header icon or the focus ring can
    /// sit on in `t`: the window tint (bare, hovered, selected: sidebar
    /// rows) and the cards (the pane header and its border), over a white
    /// and a black desktop.
    fn surfaces(t: &Tokens) -> Vec<Rgba> {
        let mut out = Vec::new();
        for desktop in [rgb(0xffffff), rgb(0x000000)] {
            let window = t.window.over(desktop);
            out.push(window);
            out.push(t.hover.over(window));
            out.push(t.selected.over(window));
            // A row whose agent rang the bell.
            out.push(
                Rgba {
                    a: 0.14,
                    ..t.warning
                }
                .over(window),
            );
            out.push(t.card.over(window));
        }
        out
    }

    #[test]
    fn every_colour_reads_on_every_surface_in_dark_and_light() {
        for t in [Tokens::dark(), Tokens::light()] {
            for tokens in [t, t.opaque()] {
                for c in AGENT_COLOURS {
                    let colour = c.for_appearance(tokens.appearance);
                    for bg in surfaces(&tokens) {
                        let ratio = contrast(colour, bg);
                        assert!(
                            ratio >= 3.0,
                            "{:?} {}: {ratio:.2} on {bg:?}",
                            tokens.appearance,
                            c.name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn colours_are_distinct_and_opaque() {
        let mut seen = HashSet::new();
        for c in AGENT_COLOURS {
            for v in [c.dark, c.light] {
                assert_eq!(v.a, 1.0, "{}", c.name);
                assert!(seen.insert((v.r, v.g, v.b)), "{} repeats", c.name);
            }
        }
    }

    #[test]
    fn colour_and_emoji_depend_only_on_the_id() {
        for id in [0, 1, 7, 42, u64::MAX] {
            assert_eq!(agent_colour(id), agent_colour(id));
            assert_eq!(agent_emoji(id), agent_emoji(id));
        }
        // Pinned, so a change to the derivation is a visible decision:
        // an agent's colour must not change under the user.
        assert_eq!(agent_colour(1).name, "pink");
        assert_eq!(agent_colour(2).name, "teal");
        assert_eq!(agent_colour(3).name, "purple");
        assert_eq!(agent_emoji(1), "🐙");
        assert_eq!(agent_emoji(16), "🐙");
    }

    #[test]
    fn eight_consecutive_agents_get_eight_colours() {
        for start in [0, 1, 5, 1000, u64::MAX - 7] {
            let names: HashSet<_> = (0..8)
                .map(|i| agent_colour(start.wrapping_add(i)).name)
                .collect();
            assert_eq!(names.len(), 8, "from {start}");
        }
    }

    #[test]
    fn neighbouring_ids_do_not_get_neighbouring_hues() {
        let index = |id: u64| {
            let name = agent_colour(id).name;
            AGENT_COLOURS.iter().position(|c| c.name == name).unwrap()
        };
        for id in 0..64u64 {
            let (a, b) = (index(id), index(id + 1));
            let gap = a.abs_diff(b).min(8 - a.abs_diff(b));
            assert!(gap >= 2, "{id} and {}: {a} and {b}", id + 1);
        }
    }

    #[test]
    fn emoji_are_single_code_points_and_cycle() {
        let set: HashSet<_> = AGENT_EMOJI.iter().collect();
        assert_eq!(set.len(), AGENT_EMOJI.len());
        for e in AGENT_EMOJI {
            assert_eq!(e.chars().count(), 1, "{e}");
        }
        let firsts: HashSet<_> = (0..15).map(agent_emoji).collect();
        assert_eq!(firsts.len(), 15);
    }
}
