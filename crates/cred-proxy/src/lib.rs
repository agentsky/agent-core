//! Credential-swapping reverse proxy and egress allowlist for agent-core sandboxes.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "cred-proxy");
    }
}
