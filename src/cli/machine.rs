pub(super) fn run_machine_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") if args.len() == 1 => super::runtime::machine_list(),
        Some("help" | "--help" | "-h") => {
            print_machine_help();
            Ok(0)
        }
        _ => {
            print_machine_help();
            Ok(2)
        }
    }
}

fn print_machine_help() {
    eprintln!("herdr machine commands:");
    eprintln!("  herdr machine list");
}
