//! The left column's three sections: Projects, Crew and Actions, stacked top to bottom. Each is a
//! heading, its rows, then a blank row before the next (no boxes: the column reads as one list).

/// Cells of the column's width that aren't rows: a cell of padding either side.
pub(crate) const INSET: u16 = 2;

/// Heights of Projects, Crew and Actions for a left column `h` rows tall, given each pane's
/// number of content rows (each section adds two: its heading and the blank row under it).
/// Projects starts at no more than a quarter of the column and Crew as tall as its rows, leaving
/// Actions a usable section; Actions takes the rest, so it reaches the bottom of the column.
/// Projects then grows into any rows that Actions doesn't need, so a project tree is shown whole
/// whenever Crew and Actions are.
pub(crate) fn heights(h: u16, projects: u16, crew: u16, actions: u16) -> (u16, u16, u16) {
    let projects_want = projects.saturating_add(2);
    let projects = projects_want.min((h / 4).max(3)).min(h);
    let rest = h - projects;
    let actions_want = actions.saturating_add(2).max(3);
    let floor = actions_want.min((rest / 2).max(3));
    let mut crew = crew.saturating_add(2).min(rest.saturating_sub(floor));
    if rest >= 6 {
        crew = crew.max(3);
    }
    let crew = crew.min(rest);
    let actions = rest - crew;
    let grow = actions
        .saturating_sub(actions_want)
        .min(projects_want - projects);
    (projects + grow, crew, actions - grow)
}

#[cfg(test)]
mod tests {
    use super::heights;

    #[test]
    fn the_column_is_shared_as_the_plan_says() {
        // Projects with 2 rows, Crew with 6.
        let cases = [
            (14, [(3, 8, 3), (3, 6, 5), (3, 6, 5)]),
            (20, [(4, 8, 8), (4, 8, 8), (4, 8, 8)]),
            (30, [(4, 8, 18), (4, 8, 18), (4, 8, 18)]),
            (43, [(4, 8, 31), (4, 8, 31), (4, 8, 31)]),
        ];
        for (h, expected) in cases {
            for (actions, want) in [0, 3, 30].into_iter().zip(expected) {
                assert_eq!(heights(h, 2, 6, actions), want, "h {h}, {actions} actions");
            }
        }
    }

    #[test]
    fn projects_grow_into_the_rows_actions_does_not_need() {
        // Projects with 9 rows, Crew with 13, as on the sample Bridge.
        let cases = [
            // Actions needs 5 of its 18 rows: Projects takes one more and is whole.
            (43, 3, (11, 15, 17)),
            // Actions needs all but one of its rows: Projects takes that one.
            (43, 15, (11, 15, 17)),
            (43, 16, (10, 15, 18)),
            // A shorter column: Projects grows as far as Actions can spare.
            (30, 3, (10, 15, 5)),
            // Nothing spare: Projects keeps its quarter and scrolls.
            (30, 30, (7, 12, 11)),
        ];
        for (h, actions, want) in cases {
            assert_eq!(heights(h, 9, 13, actions), want, "h {h}, {actions} actions");
        }
        // An empty inbox still keeps its one-line box.
        assert_eq!(heights(43, 30, 6, 0), (32, 8, 3));
    }

    #[test]
    fn every_row_is_given_out_even_when_tiny() {
        for h in 0..60 {
            for (p, c, a) in [(0, 0, 0), (2, 6, 3), (9, 13, 3), (40, 40, 40), (40, 2, 0)] {
                let (x, y, z) = heights(h, p, c, a);
                assert_eq!(x + y + z, h, "h {h}");
                if h >= 9 && p > 0 {
                    assert!(x >= 3 && y >= 3 && z >= 3, "h {h}: {x} {y} {z}");
                }
            }
        }
        // A long crew never squeezes Actions out of a usable box.
        let (_, crew, actions) = heights(14, 2, 40, 5);
        assert!(crew >= 3 && actions >= 3);
    }
}
