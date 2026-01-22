use std::collections::HashSet;
use std::io::{Write, stdin, stdout};
use std::process::Command;
use std::time::Duration;

use color_eyre::eyre::{bail, eyre, Result};
use facet::Facet;
use facet_args as args;
use facet_json::{from_str, to_string};
use reqwest::Client;

const BASE_URL: &str = "https://crates.io";
const USER_AGENT: &str = "tp-trusted-publishing-setup (contact: amos@bearcove.eu)";

#[derive(Facet, Debug)]
struct Args {
    /// GitHub repository owner (e.g., "facet-rs")
    #[facet(args::positional)]
    owner: String,

    /// GitHub repository name (e.g., "facet")
    #[facet(args::positional)]
    repo: String,

    /// Workflow filename (e.g., "release-plz.yml")
    #[facet(default = "release-plz.yml".to_string(), args::named, args::short = 'w')]
    workflow: String,

    /// Environment variable name containing the crates.io token
    #[facet(default = "CRATES_IO_TOKEN".to_string(), args::named, args::short = 'e')]
    token_env: String,

    /// Dry run - don't actually configure trusted publishing
    #[facet(args::named, args::short = 'n')]
    dry_run: bool,
}

#[derive(Facet, Debug)]
struct CargoMetadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Facet, Debug, Clone)]
struct Package {
    name: String,
    id: String,
    version: String,
    description: Option<String>,
    license: Option<String>,
    repository: Option<String>,
    publish: Option<Vec<String>>,
}

#[derive(Facet, Debug)]
struct GithubConfigRequest {
    github_config: GithubConfigInner,
}

#[derive(Facet, Debug)]
struct GithubConfigInner {
    #[facet(rename = "crate")]
    crate_name: String,
    repository_owner: String,
    repository_name: String,
    workflow_filename: String,
}

fn get_publishable_crates() -> Result<Vec<Package>> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()?;

    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8(output.stdout)?;
    let metadata: CargoMetadata = from_str(&stdout)?;

    let workspace_member_ids: HashSet<&str> =
        metadata.workspace_members.iter().map(|s| s.as_str()).collect();

    let publishable = metadata
        .packages
        .into_iter()
        .filter(|pkg| {
            if !workspace_member_ids.contains(pkg.id.as_str()) {
                return false;
            }
            match &pkg.publish {
                Some(registries) if registries.is_empty() => false,
                _ => true,
            }
        })
        .collect();

    Ok(publishable)
}

fn publish_skeleton(pkg: &Package, token: &str) -> Result<()> {
    let tmp_dir = std::env::temp_dir().join(format!("tp-skeleton-{}", pkg.name));

    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    std::fs::create_dir_all(&tmp_dir)?;

    let description = pkg.description.as_deref().unwrap_or("Placeholder for trusted publishing setup");
    let license = pkg.license.as_deref().unwrap_or("MIT OR Apache-2.0");

    let mut cargo_toml = format!(
        r#"[package]
name = "{}"
version = "0.0.0"
edition = "2024"
description = "{}"
license = "{}"
"#,
        pkg.name,
        description.replace('"', r#"\""#),
        license
    );

    if let Some(repo) = &pkg.repository {
        cargo_toml.push_str(&format!("repository = \"{}\"\n", repo));
    }

    std::fs::write(tmp_dir.join("Cargo.toml"), cargo_toml)?;

    let src_dir = tmp_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("lib.rs"), "//! Placeholder crate for trusted publishing setup.\n")?;

    let status = Command::new("cargo")
        .args(["publish", "--allow-dirty"])
        .env("CARGO_REGISTRY_TOKEN", token)
        .current_dir(&tmp_dir)
        .status()?;

    std::fs::remove_dir_all(&tmp_dir)?;

    if !status.success() {
        bail!("cargo publish failed for {}", pkg.name);
    }

    Ok(())
}

fn ask_yes_no(prompt: &str) -> bool {
    print!("{} [Y/n] ", prompt);
    stdout().flush().unwrap();

    let mut input = String::new();
    stdin().read_line(&mut input).unwrap();
    let input = input.trim().to_lowercase();

    input.is_empty() || input == "y" || input == "yes"
}

async fn crate_exists(client: &Client, name: &str) -> Result<bool> {
    let url = format!("{}/api/v1/crates/{}", BASE_URL, name);
    let res = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await?;
    Ok(res.status().is_success())
}

async fn create_trustpub_github_config(
    client: &Client,
    token: &str,
    config: &GithubConfigRequest,
) -> Result<()> {
    let url = format!("{}/api/v1/trusted_publishing/github_configs", BASE_URL);
    let body = to_string(config)?;

    let res = client
        .post(&url)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/json")
        .header("Authorization", token)
        .body(body)
        .send()
        .await?;

    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await?;
        bail!("{}: {}", status, text);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let args: Args = facet_args::from_std_args().map_err(|e| eyre!("{}", e))?;

    let token = std::env::var(&args.token_env)
        .map_err(|_| eyre!("Set {} environment variable", args.token_env))?;

    let packages = get_publishable_crates()?;
    println!("Found {} publishable crates\n", packages.len());

    if packages.is_empty() {
        println!("No publishable crates found.");
        return Ok(());
    }

    let client = Client::new();

    println!("Checking which crates exist on crates.io...");
    let mut unpublished: Vec<&Package> = Vec::new();
    for pkg in &packages {
        if !crate_exists(&client, &pkg.name).await? {
            unpublished.push(pkg);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !unpublished.is_empty() {
        println!("\nThe following crates have never been published to crates.io:");
        for pkg in &unpublished {
            println!("  - {}", pkg.name);
        }

        if args.dry_run {
            println!("\n(dry run) Would publish skeleton crates to reserve names");
        } else if ask_yes_no("\nPublish skeleton crates to reserve these names?") {
            println!();
            for pkg in &unpublished {
                print!("Publishing skeleton for {}... ", pkg.name);
                stdout().flush().unwrap();
                match publish_skeleton(pkg, &token) {
                    Ok(()) => println!("✓"),
                    Err(e) => {
                        println!("✗ {}", e);
                        bail!("Failed to publish skeleton for {}", pkg.name);
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            println!();
        } else {
            println!("\nAborting. Publish the crates manually or run again to create skeletons.");
            std::process::exit(1);
        }
    } else {
        println!("All crates exist on crates.io.\n");
    }

    for pkg in &packages {
        print!("Configuring trusted publishing for {}... ", pkg.name);

        if args.dry_run {
            println!("(dry run)");
            continue;
        }

        let config = GithubConfigRequest {
            github_config: GithubConfigInner {
                crate_name: pkg.name.clone(),
                repository_owner: args.owner.clone(),
                repository_name: args.repo.clone(),
                workflow_filename: args.workflow.clone(),
            },
        };

        match create_trustpub_github_config(&client, &token, &config).await {
            Ok(()) => println!("✓"),
            Err(e) => println!("✗ {}", e),
        }

        tokio::time::sleep(Duration::from_millis(1100)).await;
    }

    println!("\nDone! Trusted publishing configured for all crates.");
    Ok(())
}
