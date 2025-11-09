// Focused CLI parsing tests (tests command-line parsing only, not business logic)

use clap::Parser;
use fcvm::cli::{Cli, Commands};

#[test]
fn test_all_commands_parse() {
    // Test that all commands parse correctly
    let test_cases = vec![
        vec!["fcvm", "podman", "run", "nginx:latest", "--name", "web"],
        vec!["fcvm", "snapshot", "create", "web"],
        vec!["fcvm", "snapshot", "serve", "web"],
        vec!["fcvm", "snapshot", "run", "web"],
        vec!["fcvm", "snapshot", "run", "web", "--name", "web-2"],
        vec!["fcvm", "snapshots"],
        vec!["fcvm", "inspect", "--name", "vm-3"],
        vec!["fcvm", "logs", "--name", "vm-4"],
    ];

    for args in test_cases {
        Cli::try_parse_from(&args).expect(&format!("Failed to parse: {:?}", args));
    }
}

#[test]
fn test_podman_run_with_all_options() {
    let args = vec![
        "fcvm", "podman", "run", "nginx:latest",
        "--name", "web-server",
        "--cpu", "4",
        "--mem", "1024",
        "--publish", "8080:80",
        "--publish", "8443:443/tcp",
        "--env", "FOO=bar",
        "--map", "/host/path:/guest/path:rw",
    ];

    let cli = Cli::try_parse_from(args).unwrap();
    match cli.cmd {
        Commands::Podman(p) => {
            match p.cmd {
                fcvm::cli::PodmanCommands::Run(r) => {
                    assert_eq!(r.image, "nginx:latest");
                    assert_eq!(r.name, "web-server".to_string());
                    assert_eq!(r.cpu, 4);
                    assert_eq!(r.mem, 1024);
                    assert_eq!(r.publish.len(), 2);
                    assert_eq!(r.env.len(), 1);
                    assert_eq!(r.map.len(), 1);
                }
            }
        }
        _ => panic!("Expected Podman command"),
    }
}
