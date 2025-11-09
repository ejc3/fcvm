// Focused CLI parsing tests (tests command-line parsing only, not business logic)

use clap::Parser;
use fcvm::cli::{Cli, Commands};

#[test]
fn test_all_commands_parse() {
    // Test that all commands parse correctly
    let test_cases = vec![
        vec!["fcvm", "run", "nginx:latest"],
        vec!["fcvm", "ls"],
        vec!["fcvm", "stop", "--name", "vm-1"],
        vec!["fcvm", "clone", "--name", "vm-2", "--snapshot", "snap-1"],
        vec!["fcvm", "inspect", "--name", "vm-3"],
        vec!["fcvm", "logs", "--name", "vm-4"],
        vec!["fcvm", "top"],
        vec!["fcvm", "setup", "kernel", "--output", "/tmp/kernel"],
        vec!["fcvm", "setup", "rootfs", "--output", "/tmp/rootfs", "--suite", "bookworm", "--size-mb", "2048"],
        vec!["fcvm", "setup", "preflight"],
    ];

    for args in test_cases {
        Cli::try_parse_from(&args).expect(&format!("Failed to parse: {:?}", args));
    }
}

#[test]
fn test_run_with_all_options() {
    let args = vec![
        "fcvm", "run", "nginx:latest",
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
        Commands::Run(r) => {
            assert_eq!(r.image, "nginx:latest");
            assert_eq!(r.name, Some("web-server".to_string()));
            assert_eq!(r.cpu, 4);
            assert_eq!(r.mem, 1024);
            assert_eq!(r.publish.len(), 2);
            assert_eq!(r.env.len(), 1);
            assert_eq!(r.map.len(), 1);
        }
        _ => panic!("Expected Run command"),
    }
}
