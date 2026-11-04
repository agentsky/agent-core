//! Persistence and encryption at rest for agent-core.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "store");
    }
}
