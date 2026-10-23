//! The left column's three boxes: Projects, Crew and Actions, stacked top to bottom.

/// Heights of Projects, Crew and Actions for a left column `h` rows tall, given each pane's
/// number of content rows. Projects and Crew are as tall as their rows within their limits;
/// Actions takes the rest, so it reaches the bottom of the column.
pub(crate) fn heights(h: u16, projects: u16, crew: u16, actions: u16) -> (u16, u16, u16) {
    let projects = projects.saturating_add(2).min((h / 4).max(3)).min(h);
    let rest = h - projects;
    let floor = if actions == 0 {
        3
    } else {
        actions.saturating_add(2).min((rest / 2).max(3))
    };
    let mut crew = crew.saturating_add(2).min(rest.saturating_sub(floor));
    if rest >= 6 {
        crew = crew.max(3);
    }
    let crew = crew.min(rest);
    (projects, crew, rest - crew)
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
    fn every_row_is_given_out_even_when_tiny() {
        for h in 0..60 {
            for (p, c, a) in [(0, 0, 0), (2, 6, 3), (40, 40, 40)] {
                let (x, y, z) = heights(h, p, c, a);
                assert_eq!(x + y + z, h, "h {h}");
            }
        }
        // A long crew never squeezes Actions out of a usable box.
        let (_, crew, actions) = heights(14, 2, 40, 5);
        assert!(crew >= 3 && actions >= 3);
    }
}
