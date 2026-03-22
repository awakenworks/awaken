use awaken_contract::state::{KeyScope, MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};

/// A single trip in the travel planner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trip {
    pub id: String,
    pub name: String,
    pub destination: String,
    pub places: Vec<Place>,
}

/// A place of interest within a trip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Place {
    pub id: String,
    pub name: String,
    pub address: String,
    pub lat: f64,
    pub lng: f64,
    pub description: String,
    pub category: String,
}

/// Search progress indicator sent during place searches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchProgress {
    pub query: String,
    pub status: String,
    pub results_count: usize,
}

/// Root state for the travel example.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TravelStateValue {
    #[serde(default)]
    pub selected_trip_id: Option<String>,
    #[serde(default)]
    pub trips: Vec<Trip>,
    #[serde(default)]
    pub search_progress: Vec<SearchProgress>,
}

/// State update actions for the travel state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TravelAction {
    SetSelectedTripId(Option<String>),
    SetTrips(Vec<Trip>),
    SetSearchProgress(Vec<SearchProgress>),
}

/// State key binding for the travel example.
pub struct TravelState;

impl StateKey for TravelState {
    const KEY: &'static str = "travel";
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = TravelStateValue;
    type Update = TravelAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            TravelAction::SetSelectedTripId(id) => value.selected_trip_id = id,
            TravelAction::SetTrips(trips) => value.trips = trips,
            TravelAction::SetSearchProgress(progress) => value.search_progress = progress,
        }
    }
}
