pub trait FactLog {
    fn append_run(&self, fact: crate::fact::run::Fact);
}
