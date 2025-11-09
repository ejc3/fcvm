mod common;

use clap::Parser;
use fcvm::cli::{Cli, Commands};

#[test]
fn test_parse_run_command() {
    let args = vec![
        "fcvm",
        "run",
        "nginx:latest",
        "--cpu", "2",
        "--mem", "512",
        "--publish", "8080:80",
    ];

    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Run(run_args) => {
            assert_eq!(run_args.image, "nginx:latest");
            assert_eq!(run_args.cpu, 2);
            assert_eq!(run_args.mem, 512);
            assert_eq!(run_args.publish.len(), 1);
            assert_eq!(run_args.publish[0], "8080:80");
        }
        _ => panic!("Expected Run command"),
    }
}

#[test]
fn test_parse_run_with_name() {
    let args = vec![
        "fcvm",
        "run",
        "redis:alpine",
        "--name", "my-redis",
    ];

    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Run(run_args) => {
            assert_eq!(run_args.image, "redis:alpine");
            assert_eq!(run_args.name, Some("my-redis".to_string()));
        }
        _ => panic!("Expected Run command"),
    }
}

#[test]
fn test_parse_run_with_env() {
    let args = vec![
        "fcvm",
        "run",
        "postgres:latest",
        "--env", "POSTGRES_PASSWORD=secret",
        "--env", "POSTGRES_USER=admin",
    ];

    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Run(run_args) => {
            assert_eq!(run_args.env.len(), 2);
            assert!(run_args.env.contains(&"POSTGRES_PASSWORD=secret".to_string()));
            assert!(run_args.env.contains(&"POSTGRES_USER=admin".to_string()));
        }
        _ => panic!("Expected Run command"),
    }
}

#[test]
fn test_parse_ls_command() {
    let args = vec!["fcvm", "ls"];
    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Ls => {}
        _ => panic!("Expected Ls command"),
    }
}

#[test]
fn test_parse_stop_command() {
    let args = vec!["fcvm", "stop", "--name", "my-vm"];
    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Stop(stop_args) => {
            assert_eq!(stop_args.name, "my-vm");
        }
        _ => panic!("Expected Stop command"),
    }
}

#[test]
fn test_parse_clone_command() {
    let args = vec!["fcvm", "clone", "--name", "new-vm", "--snapshot", "snap-123"];
    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Clone(clone_args) => {
            assert_eq!(clone_args.name, "new-vm");
            assert_eq!(clone_args.snapshot, "snap-123");
        }
        _ => panic!("Expected Clone command"),
    }
}

#[test]
fn test_parse_setup_kernel_command() {
    let args = vec![
        "fcvm",
        "setup",
        "kernel",
        "--output", "/var/lib/fcvm/kernel",
    ];

    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Setup(setup_args) => {
            match setup_args.cmd {
                fcvm::cli::SetupCommands::Kernel { output, download } => {
                    assert_eq!(output, "/var/lib/fcvm/kernel");
                    assert!(!download);
                }
                _ => panic!("Expected Kernel setup command"),
            }
        }
        _ => panic!("Expected Setup command"),
    }
}

#[test]
fn test_parse_setup_rootfs_command() {
    let args = vec![
        "fcvm",
        "setup",
        "rootfs",
        "--output", "/var/lib/fcvm/rootfs",
        "--suite", "bookworm",
        "--size-mb", "2048",
    ];

    let cli = Cli::try_parse_from(args).unwrap();

    match cli.cmd {
        Commands::Setup(setup_args) => {
            match setup_args.cmd {
                fcvm::cli::SetupCommands::Rootfs { output, suite, size_mb } => {
                    assert_eq!(output, "/var/lib/fcvm/rootfs");
                    assert_eq!(suite, "bookworm");
                    assert_eq!(size_mb, 2048);
                }
                _ => panic!("Expected Rootfs setup command"),
            }
        }
        _ => panic!("Expected Setup command"),
    }
}
