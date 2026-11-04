//! Per-session sandbox containers for agent-core.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "sandbox");
    }
}
