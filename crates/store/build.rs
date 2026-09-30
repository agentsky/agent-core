//! Rebuilds the crate when a migration changes. `sqlx::migrate!` embeds the
//! `migrations` directory at compile time, but on stable Rust it can't tell
//! Cargo to watch it.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
