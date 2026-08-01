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
    AppGrid,
}

impl BarState {
    pub fn as_str(self) -> &'static str {
        match self {
            BarState::Closed => "closed",
            BarState::Hidden => "hidden",
            BarState::Detailed => "detailed",
            BarState::AppGrid => "app-grid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drag::Visual;

    #[test]
    fn bar_state_as_str_all_variants() {
        assert_eq!(BarState::Closed.as_str(), "closed");
        assert_eq!(BarState::Hidden.as_str(), "hidden");
        assert_eq!(BarState::Detailed.as_str(), "detailed");
        assert_eq!(BarState::AppGrid.as_str(), "app-grid");
    }

    #[test]
    fn visual_from_bar_state() {
        let v = Visual::from(BarState::Closed);
        assert!((v.hidden_amount - 0.0).abs() < 1e-9);
        assert!((v.detailed_amount - 0.0).abs() < 1e-9);
        assert!((v.grid_amount - 0.0).abs() < 1e-9);

        let v = Visual::from(BarState::Hidden);
        assert!((v.hidden_amount - 1.0).abs() < 1e-9);
        assert!((v.detailed_amount - 0.0).abs() < 1e-9);
        assert!((v.grid_amount - 0.0).abs() < 1e-9);

        let v = Visual::from(BarState::Detailed);
        assert!((v.hidden_amount - 0.0).abs() < 1e-9);
        assert!((v.detailed_amount - 1.0).abs() < 1e-9);
        assert!((v.grid_amount - 0.0).abs() < 1e-9);

        let v = Visual::from(BarState::AppGrid);
        assert!((v.hidden_amount - 0.0).abs() < 1e-9);
        assert!((v.detailed_amount - 1.0).abs() < 1e-9);
        assert!((v.grid_amount - 1.0).abs() < 1e-9);
    }

    #[test]
    fn workspace_id_equality() {
        let a = WorkspaceId { x: 0, y: 0 };
        let b = WorkspaceId { x: 0, y: 0 };
        let c = WorkspaceId { x: 1, y: 0 };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(WorkspaceId::ZERO, a);
    }
}
