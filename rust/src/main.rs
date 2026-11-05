fn main() {
    std::process::exit(sitefile::cli::main(std::env::args().skip(1).collect()));
}
