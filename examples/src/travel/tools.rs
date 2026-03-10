use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tirea_agentos::contracts::runtime::tool_call::{
    Tool, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult,
};
use tirea_agentos::contracts::AnyStateAction;
use tirea_agentos::contracts::ToolCallContext;

use super::state::{Place, SearchProgress, TravelAction, TravelState, Trip};

/// Add one or more trips to the travel plan.
///
/// Corresponds to CopilotKit's `add_trips` tool.
pub struct AddTripTool;

#[async_trait]
impl Tool for AddTripTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("add_trips", "Add Trips", "Add new trips to the travel plan")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "trips": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id":          { "type": "string" },
                                "name":        { "type": "string" },
                                "destination": { "type": "string" },
                                "places":      { "type": "array", "items": { "type": "object" }, "default": [] }
                            },
                            "required": ["id", "name", "destination"]
                        },
                        "description": "Array of trips to add"
                    }
                },
                "required": ["trips"]
            }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let raw_trips = args["trips"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trips' array".into()))?;

        let mut current = ctx.snapshot_of::<TravelState>().unwrap_or_default().trips;

        for raw in raw_trips {
            let trip = Trip {
                id: raw["id"].as_str().unwrap_or_default().to_string(),
                name: raw["name"].as_str().unwrap_or_default().to_string(),
                destination: raw["destination"].as_str().unwrap_or_default().to_string(),
                places: parse_places(raw["places"].as_array()),
            };
            current.push(trip);
        }

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "add_trips",
            json!({ "added": raw_trips.len(), "total": current.len() }),
        ))
        .with_action(AnyStateAction::new::<TravelState>(TravelAction::SetTrips(
            current,
        ))))
    }
}

/// Update existing trips in the travel plan.
pub struct UpdateTripTool;

#[async_trait]
impl Tool for UpdateTripTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "update_trips",
            "Update Trips",
            "Update existing trips in the travel plan",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "trips": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id":          { "type": "string" },
                            "name":        { "type": "string" },
                            "destination": { "type": "string" },
                            "places":      { "type": "array", "items": { "type": "object" } }
                        },
                        "required": ["id"]
                    }
                }
            },
            "required": ["trips"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let updates = args["trips"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trips' array".into()))?;

        let mut current = ctx.snapshot_of::<TravelState>().unwrap_or_default().trips;
        let mut updated_count = 0usize;

        for upd in updates {
            let id = upd["id"].as_str().unwrap_or_default();
            if let Some(trip) = current.iter_mut().find(|t| t.id == id) {
                if let Some(n) = upd["name"].as_str() {
                    trip.name = n.to_string();
                }
                if let Some(d) = upd["destination"].as_str() {
                    trip.destination = d.to_string();
                }
                if let Some(arr) = upd["places"].as_array() {
                    trip.places = parse_places(Some(arr));
                }
                updated_count += 1;
            }
        }

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "update_trips",
            json!({ "updated": updated_count }),
        ))
        .with_action(AnyStateAction::new::<TravelState>(TravelAction::SetTrips(
            current,
        ))))
    }
}

/// Delete trips from the travel plan.
pub struct DeleteTripTool;

#[async_trait]
impl Tool for DeleteTripTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "delete_trips",
            "Delete Trips",
            "Delete trips from the travel plan",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "trip_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "IDs of trips to delete"
                }
            },
            "required": ["trip_ids"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let ids: Vec<&str> = args["trip_ids"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trip_ids' array".into()))?
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        let state = ctx.snapshot_of::<TravelState>().unwrap_or_default();
        let mut current = state.trips;
        let before = current.len();
        current.retain(|t| !ids.contains(&t.id.as_str()));
        let deleted = before - current.len();

        let next_selected = state
            .selected_trip_id
            .filter(|sel| !ids.contains(&sel.as_str()));

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "delete_trips",
            json!({ "deleted": deleted }),
        ))
        .with_action(AnyStateAction::new::<TravelState>(TravelAction::SetTrips(
            current,
        )))
        .with_action(AnyStateAction::new::<TravelState>(
            TravelAction::SetSelectedTripId(next_selected),
        )))
    }
}

/// Arguments for [`SelectTripTool`].
#[derive(Deserialize, JsonSchema)]
pub struct SelectTripArgs {
    /// ID of the trip to select.
    trip_id: String,
}

/// Select a trip as the currently active trip.
pub struct SelectTripTool;

#[async_trait]
impl Tool for SelectTripTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "select_trip",
            "Select Trip",
            "Select a trip as the currently active trip",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "trip_id": {
                    "type": "string",
                    "description": "ID of the trip to select"
                }
            },
            "required": ["trip_id"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let args: SelectTripArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        Ok(ToolExecutionEffect::new(ToolResult::success(
            "select_trip",
            json!({ "selected": args.trip_id }),
        ))
        .with_action(AnyStateAction::new::<TravelState>(
            TravelAction::SetSelectedTripId(Some(args.trip_id)),
        )))
    }
}

/// Search for places of interest at a destination.
///
/// Returns mock data (can be replaced with a real API like Google Places).
pub struct SearchPlacesTool;

#[async_trait]
impl Tool for SearchPlacesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "search_for_places",
            "Search Places",
            "Search for places of interest at a destination",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "destination": {
                    "type": "string",
                    "description": "The destination city or area to search"
                },
                "category": {
                    "type": "string",
                    "enum": ["restaurant", "hotel", "attraction", "shopping", "nightlife"],
                    "description": "Category of places to search for"
                },
                "trip_id": {
                    "type": "string",
                    "description": "Trip ID to add found places to"
                }
            },
            "required": ["destination", "trip_id"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(<Self as Tool>::execute_effect(self, args, ctx)
            .await?
            .result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let destination = args["destination"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'destination'".into()))?;
        let trip_id = args["trip_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trip_id'".into()))?;
        let category = args["category"].as_str().unwrap_or("attraction");

        let state = ctx.snapshot_of::<TravelState>().unwrap_or_default();
        let query = format!("{} {}", category, destination);

        let places = mock_places(destination, category);

        let mut trips = state.trips;
        if let Some(trip) = trips.iter_mut().find(|t| t.id == trip_id) {
            trip.places.extend(places.clone());
        }

        Ok(ToolExecutionEffect::new(ToolResult::success(
            "search_for_places",
            json!({
                "destination": destination,
                "category": category,
                "places_found": places.len(),
                "places": places,
            }),
        ))
        .with_action(AnyStateAction::new::<TravelState>(TravelAction::SetTrips(
            trips,
        )))
        .with_action(AnyStateAction::new::<TravelState>(
            TravelAction::SetSearchProgress(vec![SearchProgress {
                query,
                status: "complete".into(),
                results_count: places.len(),
            }]),
        )))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_places(arr: Option<&Vec<Value>>) -> Vec<Place> {
    arr.map(|items| {
        items
            .iter()
            .map(|v| Place {
                id: v["id"].as_str().unwrap_or_default().to_string(),
                name: v["name"].as_str().unwrap_or_default().to_string(),
                address: v["address"].as_str().unwrap_or_default().to_string(),
                lat: v["lat"].as_f64().unwrap_or_default(),
                lng: v["lng"].as_f64().unwrap_or_default(),
                description: v["description"].as_str().unwrap_or_default().to_string(),
                category: v["category"].as_str().unwrap_or_default().to_string(),
            })
            .collect()
    })
    .unwrap_or_default()
}

fn mock_places(destination: &str, category: &str) -> Vec<Place> {
    let dest_lower = destination.to_lowercase();
    let prefix = uuid::Uuid::new_v4().to_string()[..8].to_string();

    if dest_lower.contains("paris") {
        vec![
            Place {
                id: format!("{prefix}-1"),
                name: "Eiffel Tower".into(),
                address: "Champ de Mars, Paris".into(),
                lat: 48.8584,
                lng: 2.2945,
                description: "Iconic iron lattice tower".into(),
                category: category.into(),
            },
            Place {
                id: format!("{prefix}-2"),
                name: "Louvre Museum".into(),
                address: "Rue de Rivoli, Paris".into(),
                lat: 48.8606,
                lng: 2.3376,
                description: "World's largest art museum".into(),
                category: category.into(),
            },
            Place {
                id: format!("{prefix}-3"),
                name: "Notre-Dame Cathedral".into(),
                address: "6 Parvis Notre-Dame, Paris".into(),
                lat: 48.8530,
                lng: 2.3499,
                description: "Medieval Catholic cathedral".into(),
                category: category.into(),
            },
        ]
    } else if dest_lower.contains("tokyo") {
        vec![
            Place {
                id: format!("{prefix}-1"),
                name: "Senso-ji Temple".into(),
                address: "2-3-1 Asakusa, Taito, Tokyo".into(),
                lat: 35.7148,
                lng: 139.7967,
                description: "Ancient Buddhist temple".into(),
                category: category.into(),
            },
            Place {
                id: format!("{prefix}-2"),
                name: "Shibuya Crossing".into(),
                address: "Shibuya, Tokyo".into(),
                lat: 35.6595,
                lng: 139.7004,
                description: "World's busiest pedestrian crossing".into(),
                category: category.into(),
            },
        ]
    } else {
        vec![Place {
            id: format!("{prefix}-1"),
            name: format!("Central {}", destination),
            address: format!("Main Street, {}", destination),
            lat: 0.0,
            lng: 0.0,
            description: format!("A popular {} spot in {}", category, destination),
            category: category.into(),
        }]
    }
}
