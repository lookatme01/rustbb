// Rebuild when migrations are added or changed (sqlx::migrate! embeds them at compile time).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
