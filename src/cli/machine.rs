use crate::api::schema::SshHostInfo;

pub(super) fn run_machine_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") if args.len() == 1 => super::runtime::machine_list(),
        Some("add") => machine_add(&args[1..]),
        Some("import") => machine_import(&args[1..]),
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

fn machine_add(args: &[String]) -> std::io::Result<i32> {
    let mut name = None;
    let mut target = None;
    let mut cwd = None;
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if !matches!(option, "--name" | "--target" | "--cwd") {
            eprintln!("unknown option: {option}");
            return Ok(2);
        }
        let Some(value) = args.get(index + 1) else {
            eprintln!("missing value for {option}");
            return Ok(2);
        };
        if option == "--name" {
            name = Some(value.clone());
        } else if option == "--target" {
            target = Some(value.clone());
        } else {
            cwd = Some(value.clone());
        }
        index += 2;
    }

    let Some(name) = name else {
        eprintln!("missing value for --name");
        return Ok(2);
    };
    let Some(target) = target else {
        eprintln!("missing value for --target");
        return Ok(2);
    };

    super::runtime::machine_add(crate::api::schema::MachineAddParams { name, target, cwd })
}

fn machine_import(args: &[String]) -> std::io::Result<i32> {
    let mut aliases = Vec::new();
    let mut all = false;
    for arg in args {
        if arg == "--all" {
            if all || !aliases.is_empty() {
                eprintln!("usage: herdr machine import [OPTIONS] [ALIAS]...");
                return Ok(2);
            }
            all = true;
        } else if arg.starts_with('-') {
            eprintln!("unknown option: {arg}");
            return Ok(2);
        } else {
            if all {
                eprintln!("usage: herdr machine import [OPTIONS] [ALIAS]...");
                return Ok(2);
            }
            aliases.push(arg.clone());
        }
    }

    if !all && aliases.is_empty() {
        eprintln!("usage: herdr machine import [OPTIONS] [ALIAS]...");
        return Ok(2);
    }

    if all {
        let response = super::runtime::machine_ssh_hosts()?;
        if response.get("error").is_some() {
            return super::print_response(&response);
        }
        let hosts = response
            .get("result")
            .and_then(|result| result.get("hosts"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                std::io::Error::other("machine.ssh_hosts response did not include hosts")
            })?;
        aliases = hosts
            .iter()
            .map(|host| {
                host.get("alias")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        std::io::Error::other(
                            "machine.ssh_hosts response included a host without an alias",
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
    }

    super::runtime::machine_import(aliases)
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
    eprintln!("  herdr machine add --name NAME --target TARGET [--cwd PATH]");
    eprintln!("  herdr machine import [OPTIONS] [ALIAS]...");
    eprintln!("  herdr machine ssh-hosts [--json]");
}
