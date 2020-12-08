fn main() {
    lalrpop::Configuration::new()
        .no_std()
        .process_current_dir()
        .unwrap();
}
