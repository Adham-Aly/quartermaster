//! The busy indicators: one spinning glyph per kind of work, shown wherever an agent is thinking
//! or working, and a nautical verb beside it in the conversations.
//!
//! - The Quartermaster thinks under the Lookout, an eye opening wide (`о ө ӫ Ӫ`).
//! - A secondmate thinks under the Insignia, an officer's star turning.
//! - A crewmate works under the Helm, the ship's wheel.
//! - A context being compacted shows the Asterism, stars folding together.
//! - A subscription login waiting on the browser shows the Compass Card.
//!
//! The same agent shows the same glyph everywhere (its conversation, its row of the crew tree),
//! so a busy row says who is busy.
//!
//! It costs nothing while none shows: drawing one marks its kind in the frame (`App::spinning`),
//! and only then does `App::tick` advance that kind, each at its own pace. Each step changes a
//! single cell, so the terminal gets a few bytes a step. The glyph is the good-news green and the
//! label dim, both from the terminal's own palette, so it reads in light and dark themes alike.

use crate::app::App;
use crate::theme;
use jiff::Timestamp;
use qm_types::api::AgentRef;
use ratatui::text::{Line, Span};
use std::time::Duration;

/// A kind of busy indicator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spinner {
    Lookout,
    Insignia,
    Helm,
    Asterism,
    CompassCard,
}

impl Spinner {
    /// Every kind, in `App::spin`'s order.
    pub(crate) const ALL: [Self; 5] = [
        Self::Lookout,
        Self::Insignia,
        Self::Helm,
        Self::Asterism,
        Self::CompassCard,
    ];

    /// The glyphs, in order. Every one is a single cell; some fonts draw a few of them a little
    /// wide, so a glyph is always followed by a space.
    pub(crate) const fn frames(self) -> &'static [&'static str] {
        match self {
            // An eye opening, then wide, held a beat. Plain Cyrillic letters, which coding fonts
            // carry (the old-Cyrillic eyes `ꙩ ꙮ` are missing from most, so they draw as boxes).
            Self::Lookout => &["о", "ө", "ӫ", "Ӫ", "Ӫ"],
            Self::Insignia => &["✩", "✫", "✬", "✭", "✮", "✯", "✰", "✪"],
            Self::Helm => &["⎈", "✣", "⎈", "✥"],
            Self::Asterism => &["⁎", "⁑", "⁂", "※", "⁂", "⁑"],
            // The plain circled plus and times: `⨁ ⨂` draw wider than a cell.
            Self::CompassCard => &["⊕", "⊗"],
        }
    }

    /// How long each glyph shows.
    pub(crate) const fn interval(self) -> Duration {
        Duration::from_millis(match self {
            Self::Lookout => 220,
            Self::Insignia => 140,
            Self::Helm => 180,
            Self::Asterism => 170,
            Self::CompassCard => 240,
        })
    }

    /// The kind an agent thinks or works under.
    pub(crate) const fn of(agent: AgentRef) -> Self {
        match agent {
            AgentRef::Quartermaster => Self::Lookout,
            AgentRef::Secondmate { .. } => Self::Insignia,
            AgentRef::Crewmate { .. } => Self::Helm,
        }
    }

    /// Its place in `App::spin`.
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// Its bit in `App::spinning`.
    pub(crate) const fn bit(self) -> u8 {
        1 << self.index()
    }

    /// The frame to show `elapsed` after the clock it counts from.
    pub(crate) fn frame_at(self, elapsed: Duration) -> usize {
        let step = elapsed.as_millis() / self.interval().as_millis();
        // The remainder is below the frame count, so it always fits.
        usize::try_from(step % self.frames().len() as u128).unwrap_or_default()
    }
}

/// What an orchestrator (the Quartermaster, a secondmate) is doing while it thinks.
const THINKING: [&str; 6] = [
    "Charting a course",
    "Taking bearings",
    "Plotting waypoints",
    "Reading the log",
    "Mustering the crew",
    "Keeping watch",
];

/// What a crewmate is doing while it works.
const WORKING: [&str; 4] = [
    "Trimming the sails",
    "Sounding the depths",
    "Provisioning",
    "Squaring away",
];

/// The verb for `spinner`'s turn, with its ellipsis: `Charting a course…`. `since` is when the
/// turn's last prompt arrived, so the verb holds for the whole turn and changes with the next.
pub(crate) fn verb(spinner: Spinner, since: Option<Timestamp>) -> String {
    let verbs: &[&str] = if spinner == Spinner::Helm {
        &WORKING
    } else {
        &THINKING
    };
    let seed = since.map_or(0, |at| at.as_second().unsigned_abs());
    // The remainder is below the verb count, so it always fits.
    let i = usize::try_from(seed % verbs.len() as u64).unwrap_or_default();
    format!("{}…", verbs[i])
}

/// The spinning glyph, in the good-news green; drawing it keeps its animation going.
pub(crate) fn glyph(app: &App, spinner: Spinner) -> Span<'static> {
    app.spinning.set(app.spinning.get() | spinner.bit());
    let frames = spinner.frames();
    Span::styled(
        frames[app.spin[spinner.index()] % frames.len()],
        theme::ok(),
    )
}

/// The glyph, a space and `label` dim: `ө Charting a course…`.
pub(crate) fn line(app: &App, spinner: Spinner, label: &str) -> Line<'static> {
    Line::from(vec![
        glyph(app, spinner),
        Span::raw(" "),
        Span::styled(label.to_owned(), theme::dim()),
    ])
}

#[cfg(test)]
mod tests {
    use super::{Spinner, verb};
    use std::time::Duration;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_glyph_is_one_cell() {
        for spinner in Spinner::ALL {
            for frame in spinner.frames() {
                assert_eq!(frame.width(), 1, "{spinner:?} {frame}");
            }
        }
    }

    #[test]
    fn kinds_match_their_places() {
        for (i, spinner) in Spinner::ALL.into_iter().enumerate() {
            assert_eq!(spinner.index(), i);
        }
    }

    #[test]
    fn steps_at_each_kinds_own_pace() {
        assert_eq!(Spinner::Lookout.frame_at(Duration::from_millis(219)), 0);
        assert_eq!(Spinner::Lookout.frame_at(Duration::from_millis(220)), 1);
        assert_eq!(Spinner::Lookout.frame_at(Duration::from_millis(5 * 220)), 0);
        assert_eq!(Spinner::Insignia.frame_at(Duration::from_millis(140)), 1);
    }

    #[test]
    fn a_turn_keeps_one_verb() {
        let at = jiff::Timestamp::from_second(1_000).unwrap();
        assert_eq!(verb(Spinner::Lookout, Some(at)), "Mustering the crew…");
        assert_eq!(verb(Spinner::Helm, Some(at)), "Trimming the sails…");
        assert_eq!(verb(Spinner::Insignia, None), "Charting a course…");
    }
}
