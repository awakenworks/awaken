//! In-memory row projection of the canonical dispatch transition phases.

use awaken_run_ingress_contract::DispatchPhase;

use crate::dispatch::DispatchState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowState {
    Reserved,
    ReservationLeased,
    Pending,
    Leased,
    Awaiting,
    DeadLetter,
    Superseded,
}

impl RowState {
    pub(super) fn public(self) -> DispatchState {
        match self {
            Self::Reserved => DispatchState::Reserved,
            Self::ReservationLeased => DispatchState::ReservationLeased,
            Self::Pending => DispatchState::Pending,
            Self::Leased => DispatchState::Leased,
            Self::Awaiting => DispatchState::Awaiting,
            Self::DeadLetter => DispatchState::DeadLetter,
            Self::Superseded => DispatchState::Superseded,
        }
    }

    pub(super) fn transition_phase(self) -> DispatchPhase {
        match self {
            Self::Reserved => DispatchPhase::Reserved,
            Self::ReservationLeased => DispatchPhase::ReservationLeased,
            Self::Pending => DispatchPhase::Pending,
            Self::Leased => DispatchPhase::Leased,
            Self::Awaiting => DispatchPhase::Awaiting,
            Self::DeadLetter => DispatchPhase::DeadLetter,
            Self::Superseded => DispatchPhase::Superseded,
        }
    }

    pub(super) fn from_transition_phase(phase: DispatchPhase) -> Self {
        match phase {
            DispatchPhase::Reserved => Self::Reserved,
            DispatchPhase::ReservationLeased => Self::ReservationLeased,
            DispatchPhase::Pending => Self::Pending,
            DispatchPhase::Leased => Self::Leased,
            DispatchPhase::Awaiting => Self::Awaiting,
            DispatchPhase::DeadLetter => Self::DeadLetter,
            DispatchPhase::Superseded => Self::Superseded,
        }
    }
}
