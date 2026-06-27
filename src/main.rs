fn main() {
    match dropbox_dev::cli::run_from_env() {
        Ok(output) => print!("{output}"),
        Err(error) => {
            eprintln!("{}: {error}", error.code());
            std::process::exit(1);
        }
    }
}
