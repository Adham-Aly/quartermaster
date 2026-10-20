//! Each task's block of ports.
//!
//! `41000–48999` in blocks of [`PortBlock::SIZE`]: 800 blocks. A new task takes the lowest block
//! no open task holds and on which nothing listens now. The `tasks_port_open` index refuses a
//! duplicate as a backstop; a closed task's block is free again.

use qm_engine::supervisor::PortListener;
use qm_types::error::Refusal;
use qm_types::ids::PortBlock;
use std::ops::RangeInclusive;

/// Every port a task block may use.
pub const PORT_RANGE: RangeInclusive<u16> = 41_000..=48_999;

/// The lowest free block, given the blocks open tasks hold and what listens in the range now.
pub fn choose(in_use: &[u16], listeners: &[PortListener]) -> Result<PortBlock, Refusal> {
    let mut base = *PORT_RANGE.start();
    while base.saturating_add(PortBlock::SIZE - 1) <= *PORT_RANGE.end() {
        let block = PortBlock { base };
        let held = in_use.contains(&base);
        let busy = listeners.iter().any(|l| block.ports().contains(&l.port));
        if !held && !busy {
            return Ok(block);
        }
        base += PortBlock::SIZE;
    }
    Err(Refusal::new(format!(
        "No free block of ports in {}–{}; close a finished task",
        PORT_RANGE.start(),
        PORT_RANGE.end()
    )))
}

/// A block as the brief and the startup note show it: `41420–41429`.
pub fn label(block: PortBlock) -> String {
    format!("{}–{}", block.ports().start(), block.ports().end())
}
