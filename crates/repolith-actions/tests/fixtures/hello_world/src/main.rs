fn main() {
    #[cfg(feature = "loud")]
    println!("HELLO LOUD WORLD");
    #[cfg(not(feature = "loud"))]
    println!("hello world");
}
