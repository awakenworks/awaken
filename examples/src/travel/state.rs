use serde::{Deserialize, Serialize};
use tirea_state_derive::State;

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
///
/// Corresponds to CopilotKit's `CopilotKitState` with travel-specific fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(action = "TravelAction")]
pub struct TravelState {
    #[serde(default)]
    pub selected_trip_id: Option<String>,
    #[serde(default)]
    pub trips: Vec<Trip>,
    #[serde(default)]
    pub search_progress: Vec<SearchProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TravelAction {
    SetSelectedTripId(Option<String>),
    SetTrips(Vec<Trip>),
    SetSearchProgress(Vec<SearchProgress>),
}

impl TravelState {
    pub fn reduce(&mut self, action: TravelAction) {
        match action {
            TravelAction::SetSelectedTripId(selected_trip_id) => {
                self.selected_trip_id = selected_trip_id;
            }
            TravelAction::SetTrips(trips) => self.trips = trips,
            TravelAction::SetSearchProgress(search_progress) => {
                self.search_progress = search_progress;
            }
        }
    }
}
