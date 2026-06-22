use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use reqwest::{Client, header};
use serde_json::Value;

use crate::config::{
    AbacusPaths, Credentials, PermissionMode, ProviderProfile, ProviderProtocol, Settings,
};
use crate::web::SearchBackend;

struct Preset {
    id: &'static str,
    name: &'static str,
    base_url: &'static str,
    env_key: Option<&'static str>,
    fallback_model: &'static str,
    protocol: ProviderProtocol,
}

const PRESETS: &[Preset] = &[
    Preset {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        env_key: Some("OPENAI_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::Responses,
    },
    Preset {
        id: "xai",
        name: "xAI",
        base_url: "https://api.x.ai/v1",
        env_key: Some("XAI_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::Responses,
    },
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        env_key: Some("OPENROUTER_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
    },
    Preset {
        id: "ollama",
        name: "Ollama / local server",
        base_url: "http://localhost:11434/v1",
        env_key: None,
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
    },
];

pub async fn run(paths: &AbacusPaths, force: bool) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "setup needs an interactive terminal; configure {} directly",
            paths.config_file.display()
        );
    }

    let mut settings = Settings::load(paths)?;
    let mut credentials = Credentials::load(paths)?;
    if settings.is_configured() && !force {
        let replace = confirm("A default provider already exists. Reconfigure it?", false)?;
        if !replace {
            println!("Kept the existing configuration.");
            return Ok(());
        }
    }

    println!();
    println!("  ┌─ ABACUS ─────────────────────────────────────────────┐");
    println!("  │  Fast, focused coding from the terminal.             │");
    println!("  └───────────────────────────────────────────────────────┘");
    println!();
    println!("  1/3  Choose a provider");
    println!("       Credentials stay on this machine.\n");
    for (index, preset) in PRESETS.iter().enumerate() {
        println!("       {:>2}  {}", index + 1, preset.name);
    }
    println!(
        "       {:>2}  Custom OpenAI-compatible endpoint",
        PRESETS.len() + 1
    );

    let selection = prompt_usize("\nProvider", 1, PRESETS.len() + 1)?;
    let (profile_id, display_name, base_url, env_key, fallback_model, protocol) =
        if let Some(preset) = PRESETS.get(selection - 1) {
            (
                preset.id.to_owned(),
                preset.name.to_owned(),
                preset.base_url.to_owned(),
                preset.env_key.map(str::to_owned),
                preset.fallback_model.to_owned(),
                preset.protocol,
            )
        } else {
            let name = prompt("Profile name", Some("custom"))?;
            let base = prompt("API base URL", Some("http://localhost:8000/v1"))?;
            let env = prompt("API key environment variable (blank for none)", None)?;
            let protocol =
                match prompt("Protocol: 1) chat-completions  2) responses", Some("1"))?.as_str() {
                    "2" | "responses" => ProviderProtocol::Responses,
                    _ => ProviderProtocol::ChatCompletions,
                };
            (
                {
                    let slug = slug(&name);
                    if slug.is_empty() {
                        "custom".to_owned()
                    } else {
                        slug
                    }
                },
                name,
                base,
                (!env.is_empty()).then_some(env),
                String::new(),
                protocol,
            )
        };

    let mut key = env_key
        .as_deref()
        .and_then(|name| std::env::var(name).ok())
        .or_else(|| credentials.keys.get(&profile_id).cloned());
    if env_key.is_some() && key.is_none() {
        let env_name = env_key.as_deref().unwrap_or("API_KEY");
        println!("\n  API credential");
        println!("  {env_name} is not set. Stored keys use a private local file.");
        let pasted = rpassword::prompt_password(
            "Paste an API key to store in ~/.abacus/credentials.toml (blank to skip): ",
        )?;
        if !pasted.trim().is_empty() {
            key = Some(pasted.trim().to_owned());
            credentials
                .keys
                .insert(profile_id.clone(), pasted.trim().to_owned());
        }
    }

    println!("\n  2/3  Connect and choose a model");
    print!("       Checking the provider and discovering models… ");
    io::stdout().flush()?;
    let discovered = discover_models(&base_url, key.as_deref()).await;
    let model = match discovered {
        Ok(models) if !models.is_empty() => {
            println!("found {}", models.len());
            choose_model(&models, &fallback_model)?
        }
        Ok(_) => {
            println!("no models returned");
            prompt_model(&fallback_model)?
        }
        Err(error) => {
            println!("unavailable");
            println!("       {error}");
            println!("       You can save now and repair connectivity from /config later.");
            prompt_model(&fallback_model)?
        }
    };

    settings.profiles.insert(
        profile_id.clone(),
        ProviderProfile {
            name: display_name,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model: model.clone(),
            protocol,
            api_key_env: env_key,
        },
    );
    settings.default_profile = profile_id.clone();
    println!("\n  3/3  Choose working defaults");
    let approve_automatically = confirm(
        "Allow file edits and commands without asking each time?",
        settings.ui.permission_mode == PermissionMode::AlwaysApprove,
    )?;
    settings.ui.permission_mode = if approve_automatically {
        PermissionMode::AlwaysApprove
    } else {
        PermissionMode::Ask
    };
    settings.ui.vim_mode = confirm(
        "Enable Vim keybindings in the composer?",
        settings.ui.vim_mode,
    )?;
    settings.ui.show_tooltips = confirm(
        "Show quick-start guidance on new sessions?",
        settings.ui.show_tooltips,
    )?;
    settings.search.enabled = confirm(
        "Enable web search (web_search / read_page tools)?",
        settings.search.enabled,
    )?;
    if settings.search.enabled {
        println!("       Search backend:");
        println!("         1  DuckDuckGo  (no API key)");
        println!("         2  Brave       (needs BRAVE_API_KEY)");
        println!("         3  Tavily      (needs TAVILY_API_KEY)");
        let default = match settings.search.backend {
            SearchBackend::Brave => "2",
            SearchBackend::Tavily => "3",
            SearchBackend::Duckduckgo => "1",
        };
        let (backend, env_var) = match prompt("       Backend", Some(default))?.trim() {
            "2" => (SearchBackend::Brave, Some("BRAVE_API_KEY")),
            "3" => (SearchBackend::Tavily, Some("TAVILY_API_KEY")),
            _ => (SearchBackend::Duckduckgo, None),
        };
        settings.search.backend = backend;
        if let Some(env_var) = env_var
            && std::env::var(env_var)
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
        {
            println!(
                "       Note: set {env_var} in your environment to use this backend (or name another with `[search] api_key_env`)."
            );
        }
    }
    settings.save(paths)?;
    if credentials.keys.contains_key(&profile_id) {
        credentials.save(paths)?;
    }

    println!("\n  ✓ Ready · {profile_id}/{model}");
    println!("    Settings  {}", paths.config_file.display());
    println!("    Start     cd your-project && abacus");
    println!("    Verify    abacus doctor\n");
    Ok(())
}

pub async fn discover_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut request = client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .header(header::ACCEPT, "application/json");
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.context("could not reach provider")?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        bail!("provider returned {status}: {}", one_line(&detail, 240));
    }
    let value: Value = response
        .json()
        .await
        .context("provider returned invalid JSON")?;
    let mut models = value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    models.sort_by_key(|value| value.to_ascii_lowercase());
    models.dedup();
    Ok(models)
}

fn choose_model(models: &[String], preferred: &str) -> Result<String> {
    let mut shown = models.to_vec();
    if shown.len() > 40 {
        shown.truncate(40);
    }
    println!();
    for (index, model) in shown.iter().enumerate() {
        let marker = if model == preferred { " *" } else { "" };
        println!("  {:>2}. {model}{marker}", index + 1);
    }
    println!("   0. Enter a model ID manually");

    let default = shown
        .iter()
        .position(|model| model == preferred)
        .map(|index| index + 1)
        .unwrap_or(1);
    let selected =
        prompt_usize("\nModel", 0, shown.len()).or_else(|_| Ok::<usize, anyhow::Error>(default))?;
    if selected == 0 {
        prompt_model(preferred)
    } else {
        Ok(shown[selected - 1].clone())
    }
}

fn prompt_model(fallback: &str) -> Result<String> {
    let model = prompt(
        "Model ID",
        if fallback.is_empty() {
            None
        } else {
            Some(fallback)
        },
    )?;
    if model.is_empty() {
        bail!("a model ID is required");
    }
    Ok(model)
}

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(default) => print!("{label} [{default}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim();
    Ok(if value.is_empty() {
        default.unwrap_or_default().to_owned()
    } else {
        value.to_owned()
    })
}

fn prompt_usize(label: &str, min: usize, max: usize) -> Result<usize> {
    loop {
        let raw = prompt(label, None)?;
        match raw.parse::<usize>() {
            Ok(value) if (min..=max).contains(&value) => return Ok(value),
            _ => println!("Enter a number from {min} to {max}."),
        }
    }
}

fn confirm(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    let value = prompt(&format!("{label} [{suffix}]"), None)?;
    if value.is_empty() {
        Ok(default)
    } else {
        Ok(matches!(value.to_ascii_lowercase().as_str(), "y" | "yes"))
    }
}

fn slug(value: &str) -> String {
    let value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    value.trim_matches('-').to_owned()
}

fn one_line(value: &str, max: usize) -> String {
    let value = value.replace(['\n', '\r'], " ");
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_slug_is_stable() {
        assert_eq!(slug("My Local/API"), "my-local-api");
    }
}
