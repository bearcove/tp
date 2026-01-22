use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("install") => install(),
        Some(cmd) => {
            eprintln!("Unknown command: {}", cmd);
            eprintln!("Available commands: install");
            std::process::exit(1);
        }
        None => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("Available commands: install");
            std::process::exit(1);
        }
    }
}

fn install() {
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "tp"])
        .status()
        .expect("Failed to run cargo build");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    let src = "target/release/tp";

    let home = std::env::var("HOME").expect("HOME not set");
    let dst = format!("{}/.cargo/bin/tp", home);

    std::fs::copy(src, &dst).expect("Failed to copy binary");
    println!("Copied tp to {}", dst);

    #[cfg(target_os = "macos")]
    {
        println!("Signing installed binary...");
        let status = Command::new("codesign")
            .args(["--sign", "-", "--force", &dst])
            .status()
            .expect("Failed to run codesign");

        if !status.success() {
            eprintln!("Warning: codesign failed, continuing anyway");
        }
    }

    println!("Installed tp to {}", dst);
}
