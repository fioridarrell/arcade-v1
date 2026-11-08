fn main() { for n in std::env::args().skip(1) { println!("{n}: {:?}", arcade_chain::dns::resolve_a(&n)); } }
