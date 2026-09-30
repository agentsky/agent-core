//! Test doubles and fakes for agent-core.

pub mod rocketchat;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "testkit");
    }
}
