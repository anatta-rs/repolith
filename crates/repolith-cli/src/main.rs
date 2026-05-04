//! boilerplate function that print the pkg version
fn main() {
    println!("repolith {}", env!("CARGO_PKG_VERSION"));
}
