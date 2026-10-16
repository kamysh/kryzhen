use std::io::ErrorKind;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn subcommand_dry_run_is_rejected_before_connecting_or_reading_files() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let commands: &[&[&str]] = &[
        &["verify"],
        &["hack", "add", "example", "--schema", "public"],
        &["hack", "delete", "example", "--schema", "public"],
        &["hack", "fix-checksum", "example"],
        &["hack", "migrate-from", "sqlx", "convert"],
        &["hack", "migrate-from", "sqlx", "import"],
        &["hack", "migrate-from", "sqlx", "all"],
    ];
    for subcommand in commands {
        for flag_first in [true, false] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_kryzhen"));
            command.args([
                "--host",
                "127.0.0.1",
                "--port",
                &port,
                "--database",
                "isolated",
                "--root",
                "/nonexistent/kryzhen-argument-regression",
            ]);
            if flag_first {
                command.arg("--dry-run");
            }
            command.args(*subcommand);
            if !flag_first {
                command.arg("--dry-run");
            }
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("{subcommand:?} did not reject promptly: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(!output.status.success(), "{subcommand:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("--dry-run is supported only for the default migration command"),
                "{subcommand:?}: {stderr}"
            );
            assert!(output.stdout.is_empty(), "{subcommand:?}: {output:?}");
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                ErrorKind::WouldBlock,
                "{subcommand:?} opened a database connection"
            );
        }
    }
}
