//! `accounts`: the accounts and helper programs downloads use (see `hyperfetch_core::accounts`):
//! where each stands, signing in and out, what an account can download, installing a helper.

use std::io::{BufRead, IsTerminal};

use hyperfetch_core::accounts::{self, Service, Status, Step};
use hyperfetch_core::helpers;
use indicatif::HumanBytes;

use crate::cli::AccountAction;
use crate::download::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

const SERVICES: &str = "cloud, epic, gog, steam or streams";

/// Runs `accounts ACTION [SERVICE] [ANSWER]` under the proxy setting `proxy`.
pub async fn run(action: AccountAction, service: Option<&str>, answer: Option<&str>, json: bool, proxy: helpers::Proxy<'_>) -> i32 {
    let service = match service.map(|name| Service::parse(name).ok_or(name)).transpose() {
        Ok(service) => service,
        Err(name) => {
            eprintln!("error: {name:?} is no service: {SERVICES}");
            return EXIT_USAGE;
        }
    };
    let done = match (action, service) {
        (AccountAction::List, service) => list(service, json).await,
        (_, None) => {
            eprintln!("error: name the service: {SERVICES}");
            return EXIT_USAGE;
        }
        (AccountAction::SignIn, Some(service)) => sign_in(service, answer, json, proxy).await,
        (AccountAction::SignOut, Some(service)) => {
            accounts::sign_out(service, answer, proxy).await.map(|done| println!("{done}"))
        }
        (AccountAction::Library, Some(service)) => library(service, json, proxy).await,
        (AccountAction::Install, Some(service)) => install(service, proxy).await,
    };
    match done {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("error: {e}");
            EXIT_FAILED
        }
    }
}

/// One line about a source's helper: its version and where it is, or what would be installed.
fn tool_line(status: &Status) -> String {
    let Some(tool) = &status.tool else { return "the app's own code".to_string() };
    match (&tool.path, tool.installs) {
        (Some(path), _) => format!("{} {} ({}; {})", tool.program, tool.version.as_deref().unwrap_or("?"), path.display(), tool.licence),
        (None, Some((version, size))) => format!("{} not installed: {version} ({}, {}) is fetched when first needed", tool.program, HumanBytes(size), tool.licence),
        (None, None) => format!("{} not found: install it yourself, on PATH ({})", tool.program, tool.home),
    }
}

async fn list(service: Option<Service>, json: bool) -> Result<(), String> {
    let mut all = Vec::new();
    for service in service.map_or_else(|| Service::ALL.to_vec(), |s| vec![s]) {
        all.push(accounts::status(service).await);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?);
        return Ok(());
    }
    for status in &all {
        println!("{:<13} {}", status.service.name(), tool_line(status));
        if let Some(account) = &status.account {
            println!("{:<13} signed in: {account}", "");
        }
        if let Some(note) = &status.note {
            println!("{:<13} {note}", "");
        }
    }
    Ok(())
}

/// The service's sign-in, its steps printed, what it asks for read from the terminal unless given.
async fn sign_in(service: Service, answer: Option<&str>, json: bool, proxy: helpers::Proxy<'_>) -> Result<(), String> {
    let mut answer = answer.map(str::to_string);
    for step in accounts::steps(service) {
        match step {
            Step::Open { url, why } => println!("Open {url}\n  {why}"),
            Step::Wait { why } => println!("{why}"),
            Step::Show { text } => println!("{text}"),
            Step::Paste { what } if answer.is_none() => {
                if !std::io::stdin().is_terminal() {
                    return Err(format!("give it after the service: {what}"));
                }
                eprint!("{what}: ");
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
                answer = Some(line.trim().to_string());
            }
            Step::Paste { .. } => {}
        }
    }
    let show = |step: Step| match step {
        Step::Open { url, why } => println!("{why}: {url}"),
        Step::Show { text } => println!("{text}"),
        Step::Wait { why } | Step::Paste { what: why } => println!("{why}"),
    };
    let who = accounts::sign_in(service, answer.as_deref().unwrap_or_default(), proxy, show).await?;
    match json {
        true => println!("{}", serde_json::json!({"service": service.id(), "account": who})),
        false => println!("Signed in to {}: {who}", service.name()),
    }
    Ok(())
}

async fn library(service: Service, json: bool, proxy: helpers::Proxy<'_>) -> Result<(), String> {
    let items = accounts::library(service, proxy).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&items).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if items.is_empty() {
        println!(
            "{}",
            match service {
                Service::Steam => "Steam libraries are not listed: download steam://app/ID (the number in the game's store page address)",
                Service::Streams => "Live streams have no library",
                _ => "Nothing to download",
            }
        );
    }
    for item in &items {
        let size = item.size.map(|s| HumanBytes(s).to_string());
        let installed = item.installed.as_ref().map(|v| format!("downloaded: {v}"));
        let about: Vec<String> = [item.kind.clone(), installed, size].into_iter().flatten().collect();
        let about = if about.is_empty() { String::new() } else { format!(" ({})", about.join(", ")) };
        println!("{}  {}{about}", item.link, item.title);
    }
    Ok(())
}

async fn install(service: Service, proxy: helpers::Proxy<'_>) -> Result<(), String> {
    let tool = service.tool().ok_or_else(|| format!("{} needs no helper", service.name()))?;
    let path = helpers::ensure(tool, proxy).await?;
    println!("{} is at {}", tool.program, path.display());
    Ok(())
}
