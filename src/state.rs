//! Bar state machine and per-workspace persistence.
//!
//! `BarState` is the discrete steady state. Mid-drag interpolation lives in
//! `drag::Visual`, not here — once a drag settles, state collapses to one of
//! these three values.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId {
    pub x: i32,
    pub y: i32,
}

impl WorkspaceId {
    pub const ZERO: Self = Self { x: 0, y: 0 };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BarState {
    Closed,
    Hidden,
    Detailed,
}

impl BarState {
    pub fn as_str(self) -> &'static str {
        match self {
            BarState::Closed => "closed",
            BarState::Hidden => "hidden",
            BarState::Detailed => "detailed",
        }
    }
}
