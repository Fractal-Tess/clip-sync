//! Where the picker's cursor is, and how the keys move it.
//!
//! The picker draws two zones: the history grid and the pinned column beside
//! it. Each zone keeps its own cursor, so moving into the pins and back
//! returns to the same history card, and every arrow key moves the way the
//! cards are drawn.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    History,
    Pins,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub zone: Zone,
    /// Position in the history grid, row-major.
    pub history: usize,
    /// Position in the pinned column, top to bottom.
    pub pins: usize,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            zone: Zone::History,
            history: 0,
            pins: 0,
        }
    }
}

impl Selection {
    /// Keeps both cursors inside their lists and moves to the other zone
    /// when the focused one has become empty.
    pub fn clamp(&mut self, history_len: usize, pins_len: usize) {
        self.history = self.history.min(history_len.saturating_sub(1));
        self.pins = self.pins.min(pins_len.saturating_sub(1));
        self.zone = match self.zone {
            Zone::History if history_len == 0 && pins_len > 0 => Zone::Pins,
            Zone::Pins if pins_len == 0 => Zone::History,
            zone => zone,
        };
    }

    /// Focuses the other zone, if it has anything in it.
    pub fn toggle_zone(&mut self, history_len: usize, pins_len: usize) {
        self.zone = match self.zone {
            Zone::History if pins_len > 0 => Zone::Pins,
            Zone::Pins if history_len > 0 => Zone::History,
            zone => zone,
        };
    }

    /// Applies one navigation key. `columns` and `rows` describe the grid as
    /// last drawn, so paging moves one screen.
    pub fn apply(
        &mut self,
        movement: Move,
        history_len: usize,
        pins_len: usize,
        columns: usize,
        rows: usize,
    ) {
        let columns = columns.max(1);
        match self.zone {
            Zone::History => {
                if history_len == 0 {
                    return;
                }
                let last = history_len - 1;
                let at_row_end = self.history % columns == columns - 1 || self.history == last;
                self.history = match movement {
                    // The pins sit to the right of the grid, so stepping off
                    // the end of a row lands in them.
                    Move::Right if at_row_end && pins_len > 0 => {
                        self.zone = Zone::Pins;
                        self.history
                    }
                    Move::Right => (self.history + 1).min(last),
                    Move::Left => self.history.saturating_sub(1),
                    Move::Down if self.history + columns <= last => self.history + columns,
                    // On the last full row, Down lands on the final card rather
                    // than doing nothing, so the end is always reachable.
                    Move::Down => {
                        if self.history / columns < last / columns {
                            last
                        } else {
                            self.history
                        }
                    }
                    Move::Up => self.history.checked_sub(columns).unwrap_or(self.history),
                    Move::PageDown => (self.history + columns * rows.max(1)).min(last),
                    Move::PageUp => self.history.saturating_sub(columns * rows.max(1)),
                    Move::First => 0,
                    Move::Last => last,
                };
            }
            Zone::Pins => {
                if pins_len == 0 {
                    return;
                }
                let last = pins_len - 1;
                self.pins = match movement {
                    Move::Left if history_len > 0 => {
                        self.zone = Zone::History;
                        self.pins
                    }
                    Move::Left | Move::Right => self.pins,
                    Move::Down => (self.pins + 1).min(last),
                    Move::Up => self.pins.saturating_sub(1),
                    Move::PageDown => (self.pins + rows.max(1)).min(last),
                    Move::PageUp => self.pins.saturating_sub(rows.max(1)),
                    Move::First => 0,
                    Move::Last => last,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(zone: Zone, history: usize, pins: usize) -> Selection {
        Selection {
            zone,
            history,
            pins,
        }
    }

    #[test]
    fn right_from_the_end_of_a_row_enters_the_pins_and_left_returns() {
        // Four columns, ten history cards, three pins.
        let mut selection = at(Zone::History, 7, 1);
        selection.apply(Move::Right, 10, 3, 4, 3);
        assert_eq!(selection, at(Zone::Pins, 7, 1));

        selection.apply(Move::Down, 10, 3, 4, 3);
        assert_eq!(selection, at(Zone::Pins, 7, 2));

        selection.apply(Move::Left, 10, 3, 4, 3);
        assert_eq!(selection, at(Zone::History, 7, 2), "history position kept");
    }

    #[test]
    fn right_inside_a_row_stays_in_the_grid() {
        let mut selection = at(Zone::History, 5, 0);
        selection.apply(Move::Right, 10, 3, 4, 3);
        assert_eq!(selection, at(Zone::History, 6, 0));
    }

    #[test]
    fn right_at_a_row_end_without_pins_continues_to_the_next_card() {
        let mut selection = at(Zone::History, 3, 0);
        selection.apply(Move::Right, 10, 0, 4, 3);
        assert_eq!(selection, at(Zone::History, 4, 0));
    }

    #[test]
    fn vertical_moves_follow_the_grid() {
        let mut selection = at(Zone::History, 1, 0);
        selection.apply(Move::Down, 10, 0, 4, 3);
        assert_eq!(selection.history, 5);
        // Row 2 has only cards 8 and 9; Down from 5 lands on 9, the last.
        selection.apply(Move::Down, 10, 0, 4, 3);
        assert_eq!(selection.history, 9);
        selection.apply(Move::Down, 10, 0, 4, 3);
        assert_eq!(selection.history, 9);
        selection.apply(Move::Up, 10, 0, 4, 3);
        assert_eq!(selection.history, 5);
        selection.apply(Move::Up, 10, 0, 4, 3);
        selection.apply(Move::Up, 10, 0, 4, 3);
        assert_eq!(selection.history, 1, "the top row stays put");
    }

    #[test]
    fn pins_move_as_a_column() {
        let mut selection = at(Zone::Pins, 0, 0);
        selection.apply(Move::Up, 10, 3, 4, 3);
        assert_eq!(selection.pins, 0);
        selection.apply(Move::Last, 10, 3, 4, 3);
        assert_eq!(selection.pins, 2);
        selection.apply(Move::Right, 10, 3, 4, 3);
        assert_eq!(selection, at(Zone::Pins, 0, 2));
    }

    #[test]
    fn tab_switches_zones_only_when_the_other_has_items() {
        let mut selection = Selection::default();
        selection.toggle_zone(10, 0);
        assert_eq!(selection.zone, Zone::History);
        selection.toggle_zone(10, 2);
        assert_eq!(selection.zone, Zone::Pins);
        selection.toggle_zone(10, 2);
        assert_eq!(selection.zone, Zone::History);
    }

    #[test]
    fn filtering_everything_out_of_a_zone_moves_focus() {
        let mut selection = at(Zone::History, 8, 1);
        selection.clamp(0, 2);
        assert_eq!(selection, at(Zone::Pins, 0, 1));
        selection.clamp(4, 0);
        assert_eq!(selection, at(Zone::History, 0, 0));
    }
}
