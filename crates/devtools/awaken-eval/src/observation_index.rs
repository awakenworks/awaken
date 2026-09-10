//! Canonical one-observation-per-case indexing for offline eval scorers.

use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct ObservationIndex<'a, T> {
    pub(crate) by_id: BTreeMap<&'a str, Vec<&'a T>>,
    pub(crate) duplicate_ids: Vec<String>,
    pub(crate) unexpected_ids: Vec<String>,
}

impl<'a, T> ObservationIndex<'a, T> {
    pub(crate) fn unique(&self, id: &str) -> Option<&'a T> {
        let observations = self.by_id.get(id)?;
        (observations.len() == 1).then_some(observations[0])
    }
}

pub(crate) fn index_observations<'expected, 'observation, T>(
    expected: impl IntoIterator<Item = &'expected str>,
    observations: &'observation [T],
    id: impl for<'a> Fn(&'a T) -> &'a str,
) -> ObservationIndex<'observation, T> {
    let expected = expected.into_iter().collect::<BTreeSet<_>>();
    let mut by_id: BTreeMap<&str, Vec<&T>> = BTreeMap::new();
    let mut unexpected_ids = BTreeSet::new();
    for observation in observations {
        let observation_id = id(observation);
        if expected.contains(observation_id) {
            by_id.entry(observation_id).or_default().push(observation);
        } else {
            unexpected_ids.insert(observation_id.to_owned());
        }
    }
    let duplicate_ids = by_id
        .iter()
        .filter(|(_, values)| values.len() > 1)
        .map(|(id, _)| (*id).to_owned())
        .collect();
    ObservationIndex {
        by_id,
        duplicate_ids,
        unexpected_ids: unexpected_ids.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct Observation {
        id: String,
        value: u8,
    }

    #[test]
    fn one_index_owns_unique_duplicate_and_unexpected_classification() {
        // Cause/effect decision table for the shared index:
        // R1 expected count=0 -> no entry/unique value; R2 count=1 -> exact
        // unique reference; R3 count=2 -> duplicate and no unique value;
        // R4 unexpected count>=1 -> sorted/deduplicated diagnostic only.
        let observations = [
            Observation {
                id: "one".into(),
                value: 1,
            },
            Observation {
                id: "two".into(),
                value: 2,
            },
            Observation {
                id: "two".into(),
                value: 3,
            },
            Observation {
                id: "unknown".into(),
                value: 4,
            },
            Observation {
                id: "unknown".into(),
                value: 5,
            },
        ];
        let index = index_observations(["missing", "one", "two"], &observations, |value| {
            value.id.as_str()
        });

        assert!(index.unique("missing").is_none(), "R1");
        assert_eq!(index.unique("one"), Some(&observations[0]), "R2");
        assert!(index.unique("two").is_none(), "R3");
        assert_eq!(index.duplicate_ids, ["two"], "R3");
        assert_eq!(index.unexpected_ids, ["unknown"], "R4");
    }
}
