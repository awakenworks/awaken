pub trait EventLog {
    fn append(&self, record: crate::event::record::Record);
}
