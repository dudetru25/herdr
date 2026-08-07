use crate::api::schema::SshHostInfo;

pub(super) fn run_machine_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") if args.len() == 1 => super::runtime::machine_list(),
        Some("ssh-hosts") => machine_ssh_hosts(&args[1..]),
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

fn machine_ssh_hosts(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr machine ssh-hosts [--json]");
            return Ok(2);
        }
    };
    let response = super::runtime::machine_ssh_hosts()?;
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    let hosts = response
        .get("result")
        .and_then(|result| result.get("hosts"))
        .cloned()
        .ok_or_else(|| std::io::Error::other("machine.ssh_hosts response did not include hosts"))?;
    let hosts: Vec<SshHostInfo> = serde_json::from_value(hosts).map_err(|error| {
        std::io::Error::other(format!("invalid machine.ssh_hosts response: {error}"))
    })?;
    println!("{:<24} {:<48} configured", "alias", "target");
    for host in hosts {
        println!(
            "{:<24} {:<48} {}",
            host.alias,
            host.target,
            if host.already_configured { "yes" } else { "no" }
        );
    }
    Ok(0)
}

fn print_machine_help() {
    eprintln!("herdr machine commands:");
    eprintln!("  herdr machine list");
    eprintln!("  herdr machine ssh-hosts [--json]");
}
