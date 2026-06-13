//! JSONL merge/dedupe tool. See SPEC.md for the contract.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: dedup FILE1.jsonl [FILE2.jsonl ...]");
        std::process::exit(2);
    }
    // TODO: implement per dedup/SPEC.md
    eprintln!("dedup: not implemented");
    std::process::exit(2);
}
