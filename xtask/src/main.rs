fn main() {
    if let Err(err) = xtask::run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
