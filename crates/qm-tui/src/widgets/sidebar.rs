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
