use async_trait::async_trait;
use serde_json::{Value, json};

use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};

use super::state::{Place, TravelState};

/// Add one or more trips to the travel plan.
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

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let raw_trips = args["trips"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trips' array".into()))?;

        let current = ctx.state::<TravelState>();
        let existing_count = current.map_or(0, |s| s.trips.len());
        let added = raw_trips.len();

        Ok(ToolResult::success(
            "add_trips",
            json!({ "added": added, "total": existing_count + added }),
        ))
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

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let updates = args["trips"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trips' array".into()))?;

        let mut updated_count = 0usize;
        if let Some(state) = ctx.state::<TravelState>() {
            for upd in updates {
                let id = upd["id"].as_str().unwrap_or_default();
                if state.trips.iter().any(|t| t.id == id) {
                    updated_count += 1;
                }
            }
        }

        Ok(ToolResult::success(
            "update_trips",
            json!({ "updated": updated_count }),
        ))
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

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let ids: Vec<&str> = args["trip_ids"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trip_ids' array".into()))?
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        let deleted = ctx.state::<TravelState>().map_or(0, |s| {
            s.trips
                .iter()
                .filter(|t| ids.contains(&t.id.as_str()))
                .count()
        });

        Ok(ToolResult::success(
            "delete_trips",
            json!({ "deleted": deleted }),
        ))
    }
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

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let trip_id = args["trip_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'trip_id'".into()))?;

        Ok(ToolResult::success(
            "select_trip",
            json!({ "selected": trip_id }),
        ))
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

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolResult, ToolError> {
        let destination = args["destination"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'destination'".into()))?;
        let category = args["category"].as_str().unwrap_or("attraction");

        let places = mock_places(destination, category);

        Ok(ToolResult::success(
            "search_for_places",
            json!({
                "destination": destination,
                "category": category,
                "places_found": places.len(),
                "places": places,
            }),
        ))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
            name: format!("Central {destination}"),
            address: format!("Main Street, {destination}"),
            lat: 0.0,
            lng: 0.0,
            description: format!("A popular {category} spot in {destination}"),
            category: category.into(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_places_paris_returns_three() {
        let places = mock_places("Paris", "attraction");
        assert_eq!(places.len(), 3);
        assert_eq!(places[0].name, "Eiffel Tower");
    }

    #[test]
    fn mock_places_tokyo_returns_two() {
        let places = mock_places("Tokyo", "restaurant");
        assert_eq!(places.len(), 2);
    }

    #[test]
    fn mock_places_unknown_returns_one() {
        let places = mock_places("Timbuktu", "hotel");
        assert_eq!(places.len(), 1);
        assert!(places[0].name.contains("Timbuktu"));
    }

    #[tokio::test]
    async fn add_trip_tool_descriptor() {
        let tool = AddTripTool;
        let desc = tool.descriptor();
        assert_eq!(desc.id, "add_trips");
    }

    #[tokio::test]
    async fn search_places_tool_returns_results() {
        let tool = SearchPlacesTool;
        let ctx = ToolCallContext::test_default();
        let result = tool
            .execute(json!({"destination": "Paris", "trip_id": "t1"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["places_found"], 3);
    }

    #[tokio::test]
    async fn select_trip_tool_returns_selected_id() {
        let tool = SelectTripTool;
        let ctx = ToolCallContext::test_default();
        let result = tool.execute(json!({"trip_id": "abc"}), &ctx).await.unwrap();
        assert!(result.is_success());
        assert_eq!(result.data["selected"], "abc");
    }
}
