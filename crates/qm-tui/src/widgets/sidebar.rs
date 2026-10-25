//! The left column's three sections: Projects, Needs you and Crew, stacked top to bottom. Each is a
//! heading, its rows, then a blank row before the next (no boxes: the column reads as one list).

/// Cells of the column's width that aren't rows: a cell of padding either side.
pub(crate) const INSET: u16 = 2;

/// Heights of Projects, Needs you and Crew for a left column `h` rows tall, given each section's
/// number of content rows (Projects and Needs you add two each, their heading and the blank row
/// under them; Crew adds one, its heading, and reaches the column's foot).
///
/// Projects starts at no more than a quarter of the column. Needs you is as tall as its rows, up
/// to half of what is left, so what waits on you is always in sight. Crew takes the rest. Projects
/// then grows into any rows Crew doesn't need, so every project shows whenever the other two
/// do.
pub(crate) fn heights(h: u16, projects: u16, needs: u16, crew: u16) -> (u16, u16, u16) {
    let projects_want = projects.saturating_add(2);
    let projects = projects_want.min((h / 4).max(3)).min(h);
    let rest = h - projects;
    let needs_want = needs.saturating_add(2).max(3);
    let needs = needs_want.min((rest / 2).max(3)).min(rest);
    let crew_rows = rest - needs;
    let crew_want = crew.saturating_add(1).max(2);
    let grow = crew_rows
        .saturating_sub(crew_want)
        .min(projects_want - projects);
    (projects + grow, needs, crew_rows - grow)
}

#[cfg(test)]
mod tests {
    use super::heights;

    #[test]
    fn needs_you_is_as_tall_as_its_items_up_to_half_and_crew_takes_the_rest() {
        // Projects with 2 rows; Needs you with 1 (empty), 6 or 40; Crew with 12.
        let cases = [
            (20, [(4, 3, 13), (4, 8, 8), (4, 8, 8)]),
            (30, [(4, 3, 23), (4, 8, 18), (4, 13, 13)]),
            (43, [(4, 3, 36), (4, 8, 31), (4, 19, 20)]),
        ];
        for (h, expected) in cases {
            for (needs, want) in [1, 6, 40].into_iter().zip(expected) {
                assert_eq!(heights(h, 2, needs, 12), want, "h {h}, {needs} rows");
            }
        }
    }

    #[test]
    fn projects_grow_into_the_rows_crew_does_not_need() {
        // Projects with 9 rows, as on the sample Bridge; a short crew.
        assert_eq!(heights(43, 9, 6, 5), (11, 8, 24));
        assert_eq!(heights(43, 9, 6, 30), (10, 8, 25));
        // A crew of five leaves room: Projects is whole.
        assert_eq!(heights(30, 9, 6, 5), (11, 8, 11));
    }

    #[test]
    fn every_row_is_given_out_even_when_tiny() {
        for h in 0..60 {
            for (p, n, c) in [(0, 0, 0), (2, 6, 3), (9, 13, 3), (40, 40, 40), (40, 2, 0)] {
                let (x, y, z) = heights(h, p, n, c);
                assert_eq!(x + y + z, h, "h {h}");
                if h >= 9 && p > 0 {
                    // Crew's heading and at least the Quartermaster's row.
                    assert!(x >= 3 && y >= 3 && z >= 2, "h {h}: {x} {y} {z}");
                }
            }
        }
    }
}
