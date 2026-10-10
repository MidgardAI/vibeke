//! Terminal properties supplied by the browser shell.
use std::cell::Cell;
thread_local! { static CELL: Cell<(u16,u16)> = const { Cell::new((9,18)) }; }
pub fn size() -> (u16, u16) {
    (80, 24)
}
pub fn cell_px() -> Option<(u16, u16)> {
    Some(CELL.with(Cell::get))
}
pub fn set_cell_px(w: u16, h: u16) {
    CELL.with(|v| v.set((w.max(1), h.max(1))));
}
pub fn sgr_pixels(_: bool) {}
pub fn title_pushed() {}
