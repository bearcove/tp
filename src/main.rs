use std::collections::HashSet;
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

#[derive(Facet, Debug)]
struct Package {
    name: String,
    id: String,
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

fn get_publishable_crates() -> Result<Vec<String>> {
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
        .iter()
        .filter(|pkg| {
            if !workspace_member_ids.contains(pkg.id.as_str()) {
                return false;
            }
            match &pkg.publish {
                Some(registries) if registries.is_empty() => false,
                _ => true,
            }
        })
        .map(|pkg| pkg.name.clone())
        .collect();

    Ok(publishable)
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

    let crates = get_publishable_crates()?;
    println!("Found {} publishable crates\n", crates.len());

    if crates.is_empty() {
        println!("No publishable crates found.");
        return Ok(());
    }

    let client = Client::new();

    println!("Checking which crates exist on crates.io...");
    let mut unpublished = Vec::new();
    for name in &crates {
        if !crate_exists(&client, name).await? {
            unpublished.push(name.as_str());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !unpublished.is_empty() {
        eprintln!("\nThe following crates have never been published to crates.io:");
        for name in &unpublished {
            eprintln!("  - {}", name);
        }
        eprintln!(
            "\nYou must publish these crates manually first before setting up trusted publishing."
        );
        std::process::exit(1);
    }

    println!("All crates exist on crates.io.\n");

    for crate_name in &crates {
        print!("Configuring trusted publishing for {}... ", crate_name);

        if args.dry_run {
            println!("(dry run)");
            continue;
        }

        let config = GithubConfigRequest {
            github_config: GithubConfigInner {
                crate_name: crate_name.clone(),
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
