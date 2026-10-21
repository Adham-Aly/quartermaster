//! Fitting the palette roles to the terminal's own colours.
//!
//! The app still draws with the terminal's 16 colours only; what changes is which of the 16 each
//! role uses. Each role has its colours in order of preference and takes the first that reads on
//! the terminal's background: [`READABLE`] contrast first, then [`FLOOR`]; failing both, the
//! terminal's default text colour. So a theme where today's choices read keeps them, and a light
//! theme whose yellow or bright black fades into the background gets a colour that doesn't.
//!
//! Contrast is the WCAG 2 ratio (1 to 21). When the terminal doesn't say what a palette colour is,
//! a light background assumes macOS Terminal's Basic palette, and a dark one trusts today's
//! choices.

use ratatui::style::Color;

/// Contrast every role aims for: WCAG AA for text.
const READABLE: f64 = 4.5;
/// The least contrast any text is drawn with: WCAG AA for large text and UI parts. Bright black
/// on black sits at about 3.7 in common dark themes, so dim text clears it there.
const FLOOR: f64 = 3.0;
/// How far the selection fill must stand out from the background.
const FILL_VISIBLE: f64 = 1.5;

/// An sRGB colour as the terminal reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    pub(crate) const BLACK: Self = Self::new(0, 0, 0);
    pub(crate) const WHITE: Self = Self::new(255, 255, 255);

    pub(crate) const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Relative luminance, 0 (black) to 1 (white).
    fn luminance(self) -> f64 {
        let linear = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// The contrast ratio between two colours, 1 to 21.
    fn contrast(self, other: Self) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Whether dark text reads better on it than light text.
    fn is_light(self) -> bool {
        self.contrast(Self::BLACK) > self.contrast(Self::WHITE)
    }
}

/// What the terminal said about its colours: `None` where it didn't answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reported {
    pub(crate) fg: Option<Rgb>,
    pub(crate) bg: Option<Rgb>,
    pub(crate) ansi: [Option<Rgb>; 16],
}

/// The 16 colours by palette index.
const ANSI: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// macOS Terminal's Basic profile, assumed for any colour a light terminal doesn't report.
const TYPICAL_LIGHT: [Rgb; 16] = [
    Rgb::new(0, 0, 0),
    Rgb::new(153, 0, 0),
    Rgb::new(0, 166, 0),
    Rgb::new(153, 153, 0),
    Rgb::new(0, 0, 178),
    Rgb::new(178, 0, 178),
    Rgb::new(0, 166, 178),
    Rgb::new(191, 191, 191),
    Rgb::new(102, 102, 102),
    Rgb::new(229, 0, 0),
    Rgb::new(0, 217, 0),
    Rgb::new(229, 229, 0),
    Rgb::new(0, 0, 255),
    Rgb::new(229, 0, 229),
    Rgb::new(0, 229, 229),
    Rgb::new(229, 229, 229),
];

/// The palette index of one of the 16 colours.
fn index(color: Color) -> Option<usize> {
    ANSI.iter().position(|c| *c == color)
}

/// The same hue at the other brightness: red and bright red, and so on.
fn twin(color: Color) -> Option<Color> {
    index(color).map(|i| ANSI[i ^ 8])
}

/// The colour each role draws with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Palette {
    /// Secondary text, idle borders and hints.
    pub(crate) dim: Color,
    /// Focus, approvals, the active input; also warnings (the words tell them apart).
    pub(crate) accent: Color,
    /// Running, ready, passed.
    pub(crate) ok: Color,
    /// Errors.
    pub(crate) error: Color,
    /// A supervisor's marker in a session.
    pub(crate) blue: Color,
    /// A supervisor's name.
    pub(crate) violet: Color,
    /// The background of rest buttons, the current tab and the selected row.
    pub(crate) fill: Color,
    /// Text on the fill.
    pub(crate) on_fill: Color,
    /// Text on the accent: a hovered button.
    pub(crate) on_accent: Color,
    /// Text on [`Self::on_fill`]: a button on the selected row.
    pub(crate) on_inverse_fill: Color,
    /// The terminal's palette, where known, to check a glyph on the fill.
    ansi: [Option<Rgb>; 16],
}

impl Palette {
    /// The palette for a terminal that says nothing about its colours: tuned for a dark
    /// background, as before detection existed.
    pub(crate) const DEFAULT: Self = Self {
        dim: Color::DarkGray,
        accent: Color::Yellow,
        ok: Color::Green,
        error: Color::Red,
        blue: Color::LightBlue,
        violet: Color::Magenta,
        fill: Color::DarkGray,
        on_fill: Color::White,
        on_accent: Color::Black,
        on_inverse_fill: Color::Black,
        ansi: [None; 16],
    };

    /// The palette that reads on the terminal's reported colours.
    pub(crate) fn fit(reported: &Reported) -> Self {
        let Some(bg) = reported.bg.or_else(|| {
            // Only the text colour: the background is the other end.
            reported.fg.map(|fg| {
                if fg.is_light() {
                    Rgb::BLACK
                } else {
                    Rgb::WHITE
                }
            })
        }) else {
            return Self::DEFAULT;
        };
        let light = bg.is_light();
        let ansi: [Option<Rgb>; 16] =
            std::array::from_fn(|i| reported.ansi[i].or(light.then_some(TYPICAL_LIGHT[i])));
        let fit = Fit { bg, ansi };
        let accent = fit.pick(&[
            Color::Yellow,
            Color::LightYellow,
            Color::Blue,
            Color::LightBlue,
        ]);
        let (fill, on_fill) = fit.fill();
        let on_accent = if accent == Color::Reset {
            Color::Black
        } else {
            fit.text_on(
                accent,
                &[Color::Black, Color::White, Color::DarkGray, Color::Gray],
            )
        };
        Self {
            dim: fit.dim(),
            accent,
            ok: fit.pick(&[Color::Green, Color::LightGreen]),
            error: fit.pick(&[Color::Red, Color::LightRed]),
            blue: fit.pick(&[Color::LightBlue, Color::Blue, Color::LightCyan, Color::Cyan]),
            violet: fit.pick(&[Color::Magenta, Color::LightMagenta]),
            fill,
            on_fill,
            on_accent,
            on_inverse_fill: fit.text_on(
                on_fill,
                &[Color::Black, Color::White, Color::DarkGray, Color::Gray],
            ),
            ansi,
        }
    }

    /// A coloured glyph drawn on the fill: its own colour if that reads there, else the same hue
    /// at the other brightness, else the fill's text colour.
    pub(crate) fn on_fill_glyph(&self, color: Color) -> Color {
        let Some(fill) = index(self.fill).and_then(|i| self.ansi[i]) else {
            return color;
        };
        let reads = |c: Color| {
            index(c)
                .and_then(|i| self.ansi[i])
                .is_none_or(|rgb| rgb.contrast(fill) >= FLOOR)
        };
        if reads(color) {
            color
        } else {
            twin(color).filter(|t| reads(*t)).unwrap_or(self.on_fill)
        }
    }
}

/// The terminal's background and palette, with gaps filled as the module says.
struct Fit {
    bg: Rgb,
    ansi: [Option<Rgb>; 16],
}

impl Fit {
    /// A colour's contrast on `on`; a colour the terminal didn't report counts as readable.
    fn contrast(&self, color: Color, on: Rgb) -> f64 {
        index(color)
            .and_then(|i| self.ansi[i])
            .map_or(f64::INFINITY, |rgb| rgb.contrast(on))
    }

    /// The first colour that reaches [`READABLE`] on the background, else the first that
    /// reaches [`FLOOR`], else the default text colour.
    fn pick(&self, preferred: &[Color]) -> Color {
        [READABLE, FLOOR]
            .into_iter()
            .find_map(|min| {
                preferred
                    .iter()
                    .copied()
                    .find(|c| self.contrast(*c, self.bg) >= min)
            })
            .unwrap_or(Color::Reset)
    }

    /// Bright black if it reads; else the faintest grey that does; else the default text colour.
    fn dim(&self) -> Color {
        if self.contrast(Color::DarkGray, self.bg) >= FLOOR {
            return Color::DarkGray;
        }
        [Color::Black, Color::Gray, Color::White]
            .into_iter()
            .map(|c| (c, self.contrast(c, self.bg)))
            .filter(|(_, contrast)| *contrast >= FLOOR)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(Color::Reset, |(c, _)| c)
    }

    /// The fill and its text: the first grey that stands out from the background and carries
    /// readable text, bright black first.
    fn fill(&self) -> (Color, Color) {
        let texts = [Color::White, Color::Black, Color::Gray, Color::DarkGray];
        [Color::DarkGray, Color::Gray, Color::Black, Color::White]
            .into_iter()
            .find_map(|fill| {
                let rgb = index(fill).and_then(|i| self.ansi[i]);
                if rgb.is_some_and(|rgb| rgb.contrast(self.bg) < FILL_VISIBLE) {
                    return None;
                }
                let text = texts.iter().copied().find(|t| {
                    *t != fill && rgb.is_none_or(|rgb| self.contrast(*t, rgb) >= READABLE)
                })?;
                Some((fill, text))
            })
            .unwrap_or((Color::DarkGray, Color::White))
    }

    /// The first of `texts` that reads on `bg` (one of the 16), else the one that reads best.
    fn text_on(&self, bg: Color, texts: &[Color]) -> Color {
        let Some(rgb) = index(bg).and_then(|i| self.ansi[i]) else {
            return texts[0];
        };
        let candidates = texts.iter().copied().filter(|t| *t != bg);
        candidates
            .clone()
            .find(|t| self.contrast(*t, rgb) >= READABLE)
            .or_else(|| {
                candidates.max_by(|a, b| self.contrast(*a, rgb).total_cmp(&self.contrast(*b, rgb)))
            })
            .unwrap_or(texts[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A palette from `#rrggbb` strings: foreground, background, then ANSI 0–15.
    fn reported(fg: &str, bg: &str, ansi: [&str; 16]) -> Reported {
        let rgb = |hex: &str| {
            let v = u32::from_str_radix(&hex[1..], 16).unwrap();
            let [_, r, g, b] = v.to_be_bytes();
            Some(Rgb::new(r, g, b))
        };
        Reported {
            fg: rgb(fg),
            bg: rgb(bg),
            ansi: ansi.map(rgb),
        }
    }

    const ITERM2: [&str; 16] = [
        "#000000", "#c91b00", "#00c200", "#c7c400", "#0225c7", "#ca30c7", "#00c5c7", "#c7c7c7",
        "#686868", "#ff6e67", "#5ffa68", "#fffc67", "#6871ff", "#ff77ff", "#60fdff", "#ffffff",
    ];
    const BASE16_ONE_LIGHT: [&str; 16] = [
        "#fafafa", "#ca1243", "#50a14f", "#c18401", "#4078f2", "#a626a4", "#0184bc", "#383a42",
        "#a0a1a7", "#ca1243", "#50a14f", "#c18401", "#4078f2", "#a626a4", "#0184bc", "#090a0b",
    ];

    #[test]
    fn nothing_reported_keeps_the_dark_palette() {
        assert_eq!(Palette::fit(&Reported::default()), Palette::DEFAULT);
    }

    #[test]
    fn a_dark_background_with_no_palette_keeps_todays_roles() {
        let p = Palette::fit(&Reported {
            bg: Some(Rgb::BLACK),
            ..Reported::default()
        });
        assert_eq!(
            (p.dim, p.accent, p.ok, p.error),
            (Color::DarkGray, Color::Yellow, Color::Green, Color::Red)
        );
        assert_eq!(
            (p.fill, p.on_fill, p.on_accent),
            (Color::DarkGray, Color::White, Color::Black)
        );
    }

    #[test]
    fn iterm2_dark_keeps_its_look_but_brightens_a_faint_red() {
        let p = Palette::fit(&reported("#c7c7c7", "#000000", ITERM2));
        assert_eq!(
            (p.dim, p.accent, p.ok),
            (Color::DarkGray, Color::Yellow, Color::Green)
        );
        assert_eq!((p.blue, p.violet), (Color::LightBlue, Color::Magenta));
        assert_eq!((p.fill, p.on_fill), (Color::DarkGray, Color::White));
        // #c91b00 on black is 3.5:1; the bright red reads.
        assert_eq!(p.error, Color::LightRed);
        // Green on the bright-black fill is 2.3:1; bright green reads there.
        assert_eq!(p.on_fill_glyph(Color::Green), Color::LightGreen);
    }

    #[test]
    fn iterm2_light_swaps_the_faded_yellow_and_green() {
        let p = Palette::fit(&reported("#000000", "#ffffff", ITERM2));
        assert_eq!(p.accent, Color::Blue);
        assert_eq!(p.on_accent, Color::White);
        assert_eq!(p.dim, Color::DarkGray);
        // Neither green reaches 3:1 on white: plain text it is.
        assert_eq!(p.ok, Color::Reset);
        assert_eq!(p.error, Color::Red);
        assert_eq!(p.blue, Color::Blue);
    }

    #[test]
    fn base16_light_with_its_light_bright_black_gets_a_darker_grey() {
        let p = Palette::fit(&reported("#383a42", "#fafafa", BASE16_ONE_LIGHT));
        // Bright black is #a0a1a7 (2.4:1); the only grey that reads is ANSI white, #383a42.
        assert_eq!(p.dim, Color::Gray);
        // The fill stays bright black, with ANSI bright white (#090a0b) on it.
        assert_eq!((p.fill, p.on_fill), (Color::DarkGray, Color::White));
        assert_eq!(p.accent, Color::Yellow);
    }

    #[test]
    fn a_light_background_alone_assumes_terminal_basic() {
        let p = Palette::fit(&Reported {
            bg: Some(Rgb::WHITE),
            ..Reported::default()
        });
        // Terminal's olive yellow is 3.0:1; its blue reads well.
        assert_eq!(p.accent, Color::Blue);
        assert_eq!(p.dim, Color::DarkGray);
        assert_eq!(p.ok, Color::Green);
    }

    #[test]
    fn only_a_dark_text_colour_means_a_light_background() {
        let p = Palette::fit(&Reported {
            fg: Some(Rgb::new(20, 20, 20)),
            ..Reported::default()
        });
        assert_eq!(p.accent, Color::Blue);
    }

    #[test]
    fn contrast_matches_wcag() {
        assert!((Rgb::BLACK.contrast(Rgb::WHITE) - 21.0).abs() < 1e-9);
        assert!((Rgb::new(0x66, 0x66, 0x66).contrast(Rgb::WHITE) - 5.74).abs() < 0.01);
        assert!(Rgb::WHITE.is_light());
        assert!(!Rgb::new(0x30, 0x30, 0x30).is_light());
    }
}
