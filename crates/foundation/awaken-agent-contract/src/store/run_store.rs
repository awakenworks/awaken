pub trait RunStore {
    fn get(&self, id: &crate::agent::run::Id) -> Option<crate::agent::run::Record>;
}
