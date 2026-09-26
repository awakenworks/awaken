//! Operator-authored limits for node image preparation; no Session authority.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct K8sImagePrewarm {
    pub max_images: usize,
    pub node_selector: BTreeMap<String, String>,
}

impl Default for K8sImagePrewarm {
    fn default() -> Self {
        Self {
            max_images: 16,
            node_selector: BTreeMap::new(),
        }
    }
}

impl K8sImagePrewarm {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=256).contains(&self.max_images) {
            return Err("image prewarm requires max_images 1..=256".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_image_preparation_policy() {
        // Decision table P1 max_images at valid boundaries => admit;
        // P2 zero/above maximum => reject before Kubernetes effects.
        for max_images in [0, 1, 16, 256, 257] {
            let policy = K8sImagePrewarm {
                max_images,
                ..Default::default()
            };
            assert_eq!(policy.validate().is_ok(), (1..=256).contains(&max_images));
        }
    }
}
