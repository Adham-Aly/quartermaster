//! The busy indicators: one spinning glyph per kind of work, shown wherever an agent is thinking
//! or working, and a nautical verb beside it in the conversations.
//!
//! - The Quartermaster thinks under the Lookout, an eye opening wide (`о ө ӫ Ӫ`).
//! - A secondmate thinks under the Insignia, an officer's star turning.
//! - Each crewmate works under one of six of its own ([`Spinner::POOL`]): the Helm, the Porthole,
//!   the Hatch, the Wind Rose, the Astrolabe and the Rosette. Crewmates open at once get different
//!   ones while there are enough to go round ([`for_agent`]).
//! - A context being compacted shows the Asterism, stars folding together.
//! - A subscription login waiting on the browser shows the Compass Card.
//!
//! The same agent shows the same glyph everywhere (its conversation, its entry in the crew list),
//! so a busy row says who is busy.
//!
//! It costs nothing while none shows: drawing one marks its kind in the frame (`App::spinning`),
//! and only then does `App::tick` advance that kind, each at its own pace. Each step changes a
//! single cell, so the terminal gets a few bytes a step. The glyph is the good-news green and the
//! label dim, both from the terminal's own palette, so it reads in light and dark themes alike.

use crate::app::App;
use crate::theme;
use jiff::Timestamp;
use qm_types::api::{AgentRef, CrewRow};
use qm_types::ids::TaskId;
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
    Porthole,
    Hatch,
    WindRose,
    Astrolabe,
    Rosette,
}

impl Spinner {
    /// Every kind, in `App::spin`'s order.
    pub(crate) const ALL: [Self; 10] = [
        Self::Lookout,
        Self::Insignia,
        Self::Helm,
        Self::Asterism,
        Self::CompassCard,
        Self::Porthole,
        Self::Hatch,
        Self::WindRose,
        Self::Astrolabe,
        Self::Rosette,
    ];

    /// The kinds crewmates work under.
    pub(crate) const POOL: [Self; 6] = [
        Self::Helm,
        Self::Porthole,
        Self::Hatch,
        Self::WindRose,
        Self::Astrolabe,
        Self::Rosette,
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
            Self::Porthole => &["⦶", "⊘", "⊖", "⦸"],
            Self::Hatch => &["◫", "⧄", "⊟", "⧅"],
            Self::WindRose => &["✧", "✦", "✶", "✷", "✸", "✹", "✺", "✹", "✸", "✷", "✶", "✦"],
            Self::Astrolabe => &["⊙", "⦾", "⊚", "⦿", "⊛", "⍟", "⊛", "⦿", "⊚", "⦾"],
            Self::Rosette => &["✾", "❀", "❁", "✿", "❃", "✿", "❁", "❀"],
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
            Self::Porthole | Self::Hatch => 130,
            Self::WindRose => 110,
            Self::Astrolabe | Self::Rosette => 150,
        })
    }

    /// Whether it is a crewmate's: its verbs are working ones, not thinking ones.
    pub(crate) fn works(self) -> bool {
        Self::POOL.contains(&self)
    }

    /// Its place in `App::spin`.
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// Its bit in `App::spinning`.
    pub(crate) const fn bit(self) -> u16 {
        1 << self.index()
    }

    /// The frame to show `elapsed` after the clock it counts from.
    pub(crate) fn frame_at(self, elapsed: Duration) -> usize {
        let step = elapsed.as_millis() / self.interval().as_millis();
        // The remainder is below the frame count, so it always fits.
        usize::try_from(step % self.frames().len() as u128).unwrap_or_default()
    }
}

/// The kind `agent` thinks or works under, given the crew the app shows (`crew`, the core's
/// rows in order). A crewmate's kind starts from its number, and moves on past kinds earlier
/// crewmates in the list already have, so crewmates side by side differ while there are no more
/// of them than kinds. A crewmate not in the list keeps the kind its number gives.
pub(crate) fn for_agent(crew: &[CrewRow], agent: AgentRef) -> Spinner {
    match agent {
        AgentRef::Quartermaster => Spinner::Lookout,
        AgentRef::Secondmate { .. } => Spinner::Insignia,
        AgentRef::Crewmate { task } => crewmate(crew, task),
    }
}

fn crewmate(crew: &[CrewRow], wanted: TaskId) -> Spinner {
    let pool = Spinner::POOL.len();
    let start = |task: TaskId| usize::try_from(task.get()).unwrap_or_default() % pool;
    let mut taken: Vec<usize> = Vec::new();
    for row in crew {
        let AgentRef::Crewmate { task } = row.agent else {
            continue;
        };
        if taken.len() == pool {
            taken.clear();
        }
        let mut at = start(task);
        while taken.contains(&at) {
            at = (at + 1) % pool;
        }
        if task == wanted {
            return Spinner::POOL[at];
        }
        taken.push(at);
    }
    Spinner::POOL[start(wanted)]
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
    let verbs: &[&str] = if spinner.works() { &WORKING } else { &THINKING };
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
