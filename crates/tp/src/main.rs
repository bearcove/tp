use std::collections::HashSet;
use std::io::{Write, stdout};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use color_eyre::eyre::{bail, eyre, Result};
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use facet::Facet;
use figue::{self as args, FigueBuiltins};
use facet_json::{from_str, to_string};
use futures::{StreamExt, stream};
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use reqwest::Client;

const BASE_URL: &str = "https://crates.io";
const USER_AGENT: &str = "tp-trusted-publishing-setup (contact: amos@bearcove.eu)";

#[derive(Facet, Debug)]
struct Args {
    /// GitHub repository owner (e.g., "facet-rs"). Detected from git remote if not provided.
    #[facet(args::positional)]
    owner: Option<String>,

    /// GitHub repository name (e.g., "facet"). Detected from git remote if not provided.
    #[facet(args::positional)]
    repo: Option<String>,

    /// Workflow filename (e.g., "release-plz.yml"). Auto-detected from .github/workflows/ if not provided.
    #[facet(args::named, args::short = 'w')]
    workflow: Option<String>,

    /// Environment variable to override the crates.io token (default: read from ~/.cargo/credentials.toml)
    #[facet(args::named, args::short = 'e')]
    token_env: Option<String>,

    /// Dry run - don't actually configure trusted publishing
    #[facet(args::named, args::short = 'n', default)]
    dry_run: bool,

    /// Standard CLI options (--help, --version, --completions)
    #[facet(flatten)]
    builtins: FigueBuiltins,
}

fn detect_github_repo() -> Result<(String, String)> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()?;

    if !output.status.success() {
        bail!("Could not get git remote URL. Specify owner and repo explicitly.");
    }

    let url = String::from_utf8(output.stdout)?.trim().to_string();

    // Parse SSH format: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        if let Some((owner, repo)) = rest.split_once('/') {
            return Ok((owner.to_string(), repo.to_string()));
        }
    }

    // Parse HTTPS format: https://github.com/owner/repo.git
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        if let Some((owner, repo)) = rest.split_once('/') {
            return Ok((owner.to_string(), repo.to_string()));
        }
    }

    bail!(
        "Could not parse GitHub owner/repo from remote URL: {}\nSpecify owner and repo explicitly.",
        url
    );
}

#[derive(Facet, Debug)]
struct CargoCredentials {
    registry: Option<RegistryCredentials>,
}

#[derive(Facet, Debug)]
struct RegistryCredentials {
    token: Option<String>,
}

fn get_cargo_credentials_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cargo")
        .join("credentials.toml")
}

fn detect_workflow_files() -> Result<Vec<String>> {
    let workflows_dir = PathBuf::from(".github/workflows");
    if !workflows_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&workflows_dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext == "yml" || ext == "yaml" {
                if let Some(name) = path.file_name() {
                    files.push(name.to_string_lossy().to_string());
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

fn select_workflow(files: &[String]) -> Result<String> {
    if files.is_empty() {
        bail!("No workflow files found in .github/workflows/. Specify one with -w.");
    }

    if files.len() == 1 {
        return Ok(files[0].clone());
    }

    // Sort with release-plz.yml first if it exists
    let mut sorted: Vec<_> = files.to_vec();
    if let Some(pos) = sorted.iter().position(|f| f == "release-plz.yml") {
        let release_plz = sorted.remove(pos);
        sorted.insert(0, release_plz);
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select workflow")
        .items(&sorted)
        .default(0)
        .interact()?;

    Ok(sorted[selection].clone())
}

fn get_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("tp")
        .join("configured.json")
}

#[derive(Facet, Debug, Default)]
struct TrustpubCache {
    /// Set of "owner/repo/crate" keys that have been configured
    configured: HashSet<String>,
}

fn cache_key(owner: &str, repo: &str, crate_name: &str) -> String {
    format!("{}/{}/{}", owner, repo, crate_name)
}

fn load_cache() -> TrustpubCache {
    let path = get_cache_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => facet_json::from_str(&contents).unwrap_or_default(),
        Err(_) => TrustpubCache::default(),
    }
}

fn save_cache(cache: &TrustpubCache) -> Result<()> {
    let path = get_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = facet_json::to_string(cache)?;
    std::fs::write(&path, contents)?;
    Ok(())
}

fn read_token_from_credentials() -> Result<String> {
    let path = get_cargo_credentials_path();
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| eyre!("Could not read {}: {}", path.display(), e))?;

    let creds: CargoCredentials = facet_toml::from_str(&contents)
        .map_err(|e| eyre!("Could not parse {}: {}", path.display(), e))?;

    creds
        .registry
        .and_then(|r| r.token)
        .ok_or_else(|| eyre!("No token found in {}", path.display()))
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

#[derive(Facet, Debug)]
struct GithubConfigListResponse {
    github_configs: Vec<GithubConfig>,
}

#[derive(Facet, Debug)]
struct GithubConfig {
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
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(true)
        .interact()
        .unwrap_or(false)
}

fn sparse_index_path(name: &str) -> String {
    let name = name.to_lowercase();
    match name.len() {
        1 => format!("1/{}", name),
        2 => format!("2/{}", name),
        3 => format!("3/{}/{}", &name[..1], name),
        _ => format!("{}/{}/{}", &name[..2], &name[2..4], name),
    }
}

fn check_local_sparse_index(name: &str) -> bool {
    let cargo_home = dirs::home_dir()
        .unwrap_or_default()
        .join(".cargo")
        .join("registry")
        .join("index");

    // Find any index.crates.io-* directory
    if let Ok(entries) = std::fs::read_dir(&cargo_home) {
        for entry in entries.flatten() {
            let dir_name = entry.file_name();
            if dir_name.to_string_lossy().starts_with("index.crates.io-") {
                let cache_path = entry.path().join(sparse_index_path(name));
                if cache_path.exists() {
                    return true;
                }
            }
        }
    }
    false
}

async fn crate_exists(client: &Client, name: &str) -> Result<bool> {
    // Check local cache first
    if check_local_sparse_index(name) {
        return Ok(true);
    }

    // Fall back to network request
    let url = format!("https://index.crates.io/{}", sparse_index_path(name));
    let res = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "text/plain")
        .send()
        .await?;
    Ok(res.status().is_success())
}

async fn list_trustpub_github_configs(
    client: &Client,
    token: &str,
    crates: &[Package],
) -> Result<Vec<GithubConfig>> {
    // Query configs for each crate in parallel
    let results: Vec<_> = stream::iter(crates.iter().map(|pkg| {
        let client = client;
        let crate_name = &pkg.name;
        async move {
            let url = format!(
                "{}/api/v1/trusted_publishing/github_configs?crate={}",
                BASE_URL,
                crate_name
            );

            let res = client
                .get(&url)
                .header("User-Agent", USER_AGENT)
                .header("Authorization", token)
                .send()
                .await?;

            if !res.status().is_success() {
                let status = res.status();
                let text = res.text().await?;
                bail!("Failed to list configurations for {}: {}: {}", crate_name, status, text);
            }

            let body = res.text().await?;
            let response: GithubConfigListResponse = from_str(&body)?;
            Ok::<_, color_eyre::eyre::Error>(response.github_configs)
        }
    }))
    .buffer_unordered(20)
    .collect()
    .await;

    // Flatten all configs into a single vector
    let mut all_configs = Vec::new();
    for result in results {
        all_configs.extend(result?);
    }
    Ok(all_configs)
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

    let args: Args = figue::from_std_args().unwrap();

    // Print cache location upfront
    println!("{} {}\n", "📁 Cache:".dimmed(), get_cache_path().display().dimmed());

    let (owner, repo) = match (&args.owner, &args.repo) {
        (Some(o), Some(r)) => (o.clone(), r.clone()),
        (None, None) => {
            let (o, r) = detect_github_repo()?;
            println!("{} {}/{}", "🔍 Detected repo:".cyan(), o.green(), r.green());
            (o, r)
        }
        (Some(_), None) => bail!("If you specify owner, you must also specify repo"),
        (None, Some(_)) => bail!("If you specify repo, you must also specify owner"),
    };

    let workflow = match &args.workflow {
        Some(w) => {
            println!("{} {}", "⚙️  Workflow:".cyan(), w.yellow());
            w.clone()
        }
        None => {
            let files = detect_workflow_files()?;
            let w = select_workflow(&files)?;
            println!("{} {}", "⚙️  Workflow:".cyan(), w.yellow());
            w
        }
    };
    println!();

    let token = if let Some(env_var) = &args.token_env {
        std::env::var(env_var).map_err(|_| eyre!("Set {} environment variable", env_var))?
    } else {
        read_token_from_credentials()?
    };

    let packages = get_publishable_crates()?;
    println!(
        "📦 Found {} publishable crate{}\n",
        packages.len().to_string().bright_white().bold(),
        if packages.len() == 1 { "" } else { "s" }
    );

    if packages.is_empty() {
        println!("{}", "No publishable crates found.".yellow());
        return Ok(());
    }

    let client = Client::new();

    let pb = ProgressBar::new(packages.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg} [{bar:30}] {pos}/{len}")
            .unwrap()
            .progress_chars("=> "),
    );
    pb.set_message("Checking crates.io");

    // Check crate existence in parallel (up to 20 concurrent requests)
    let results: Vec<_> = stream::iter(packages.iter().map(|pkg| {
        let client = &client;
        let pb = &pb;
        async move {
            let exists = crate_exists(client, &pkg.name).await.unwrap_or(false);
            pb.inc(1);
            (pkg, exists)
        }
    }))
    .buffer_unordered(20)
    .collect()
    .await;

    let unpublished: Vec<_> = results
        .into_iter()
        .filter(|(_, exists)| !exists)
        .map(|(pkg, _)| pkg)
        .collect();
    pb.finish_and_clear();

    if !unpublished.is_empty() {
        println!("\n{}", "⚠️  The following crates have never been published to crates.io:".yellow());
        for pkg in &unpublished {
            println!("  {} {}", "•".dimmed(), pkg.name.bright_white());
        }

        if args.dry_run {
            println!("\n{}", "(dry run) Would publish skeleton crates to reserve names".dimmed());
        } else if ask_yes_no("Publish skeleton crates to reserve these names?") {
            println!();
            for pkg in &unpublished {
                print!("  Publishing {}... ", pkg.name.cyan());
                stdout().flush().unwrap();
                match publish_skeleton(pkg, &token) {
                    Ok(()) => println!("{}", "✓".green()),
                    Err(e) => {
                        println!("{} {}", "✗".red(), e.to_string().red());
                        bail!("Failed to publish skeleton for {}", pkg.name);
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            println!();
        } else {
            println!("\n{}", "Aborted.".yellow());
            std::process::exit(1);
        }
    } else {
        println!("{}", "✓ All crates exist on crates.io.".green());
    }

    // List existing configurations from crates.io
    println!("\n{}", "🔍 Checking existing configurations...".cyan());
    let existing_configs = list_trustpub_github_configs(&client, &token, &packages).await?;

    // Build a set of already-configured (owner, repo, crate) tuples
    let already_configured: HashSet<(String, String, String)> = existing_configs
        .into_iter()
        .map(|cfg| (cfg.repository_owner, cfg.repository_name, cfg.crate_name))
        .collect();

    let mut cache = load_cache();

    // Update cache based on actual configurations from crates.io
    for pkg in &packages {
        if already_configured.contains(&(owner.clone(), repo.clone(), pkg.name.clone())) {
            cache.configured.insert(cache_key(&owner, &repo, &pkg.name));
        }
    }

    // Filter out already-configured crates
    let to_configure: Vec<_> = packages
        .iter()
        .filter(|pkg| !already_configured.contains(&(owner.clone(), repo.clone(), pkg.name.clone())))
        .collect();

    if to_configure.is_empty() {
        println!(
            "\n{} All {} crates already have trusted publishing configured.",
            "✓".green(),
            packages.len()
        );
        // Save updated cache
        if let Err(e) = save_cache(&cache) {
            eprintln!("{} could not save cache: {}", "⚠️  Warning:".yellow(), e);
        }
        return Ok(());
    }

    println!(
        "\n🔐 Will configure trusted publishing for {} crate{}:",
        to_configure.len().to_string().bright_white().bold(),
        if to_configure.len() == 1 { "" } else { "s" }
    );
    println!("   {} {}/{}", "Repository:".dimmed(), owner.green(), repo.green());
    println!("   {} {}", "Workflow:".dimmed(), workflow.yellow());
    println!("   {}", "Crates:".dimmed());
    for pkg in &to_configure {
        println!("     {} {}", "•".dimmed(), pkg.name.cyan());
    }
    if packages.len() > to_configure.len() {
        println!(
            "   {}",
            format!("({} crates already configured, skipped)", packages.len() - to_configure.len()).dimmed()
        );
    }
    println!();

    if !args.dry_run && !ask_yes_no("Proceed with trusted publishing setup?") {
        println!("{}", "Aborted.".yellow());
        return Ok(());
    }

    let pb = ProgressBar::new(to_configure.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg} [{bar:30}] {pos}/{len}")
            .unwrap()
            .progress_chars("=> "),
    );

    let mut errors = Vec::new();
    for pkg in &to_configure {
        pb.set_message(format!("Configuring {}", pkg.name));

        if !args.dry_run {
            let config = GithubConfigRequest {
                github_config: GithubConfigInner {
                    crate_name: pkg.name.clone(),
                    repository_owner: owner.clone(),
                    repository_name: repo.clone(),
                    workflow_filename: workflow.clone(),
                },
            };

            if let Err(e) = create_trustpub_github_config(&client, &token, &config).await {
                errors.push((pkg.name.clone(), e.to_string()));
            } else {
                cache.configured.insert(cache_key(&owner, &repo, &pkg.name));
            }

            tokio::time::sleep(Duration::from_millis(1100)).await;
        } else {
            // In dry-run, don't cache but still count as "would configure"
        }

        pb.inc(1);
    }
    pb.finish_and_clear();

    if !args.dry_run {
        if let Err(e) = save_cache(&cache) {
            eprintln!("{} could not save cache: {}", "⚠️  Warning:".yellow(), e);
        }
    }

    if !errors.is_empty() {
        println!("\n{}", "❌ Errors configuring trusted publishing:".red());
        for (name, err) in &errors {
            println!("   {} {} {}", name.cyan(), "✗".red(), err.dimmed());
        }
    }

    let success_count = to_configure.len() - errors.len();
    if args.dry_run {
        println!(
            "\n{} Would configure trusted publishing for {} crate{}.",
            "(dry run)".dimmed(),
            to_configure.len().to_string().bright_white(),
            if to_configure.len() == 1 { "" } else { "s" }
        );
    } else if errors.is_empty() {
        println!(
            "\n{} Configured trusted publishing for {} crate{}.",
            "✅".green(),
            success_count.to_string().bright_white().bold(),
            if to_configure.len() == 1 { "" } else { "s" }
        );
    } else {
        println!(
            "\n{} Configured trusted publishing for {}/{} crate{}.",
            "⚠️".yellow(),
            success_count.to_string().green(),
            to_configure.len(),
            if to_configure.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}
