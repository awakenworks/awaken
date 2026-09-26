//! Operator-authored limits for node image preparation; no Session authority.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct K8sImagePullJobs {
    pub parallelism: u32,
    pub max_images: usize,
    pub node_selector: BTreeMap<String, String>,
}

impl Default for K8sImagePullJobs {
    fn default() -> Self {
        Self {
            parallelism: 2,
            max_images: 16,
            node_selector: BTreeMap::new(),
        }
    }
}

impl K8sImagePullJobs {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=32).contains(&self.parallelism) || !(1..=256).contains(&self.max_images) {
            return Err("image pull jobs require parallelism 1..=32 and max_images 1..=256".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_image_preparation_policy() {
        // Causes C1 valid/zero/excess parallelism; C2 valid/zero/excess image
        // budget. E1 both valid => admitted; E2 either invalid => rejected.
        // Decision table covers boundary classes and their Cartesian product.
        for parallelism in [0, 1, 2, 32, 33] {
            for max_images in [0, 1, 16, 256, 257] {
                let policy = K8sImagePullJobs {
                    parallelism,
                    max_images,
                    ..Default::default()
                };
                assert_eq!(
                    policy.validate().is_ok(),
                    (1..=32).contains(&parallelism) && (1..=256).contains(&max_images)
                );
            }
        }
    }
}
