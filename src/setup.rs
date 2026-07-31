use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use reqwest::{Client, header};
use serde_json::Value;

use crate::config::{
    AbacusPaths, Credentials, PermissionMode, ProviderProfile, ProviderProtocol, Settings,
};
use crate::console;
use crate::web::SearchBackend;

/// A ready-made provider. Every entry is an OpenAI-compatible endpoint, which
/// is the only wire protocol Abacus speaks — vendors with their own schema are
/// deliberately absent rather than half-working.
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    pub env_key: Option<&'static str>,
    pub fallback_model: &'static str,
    pub protocol: ProviderProtocol,
    /// One-line orientation shown beside the name.
    pub hint: &'static str,
}

/// Shared with the TUI so `/config` can offer the same providers the wizard
/// does, rather than keeping a second list that drifts.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        env_key: Some("OPENAI_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::Responses,
        hint: "GPT and o-series",
    },
    Preset {
        id: "xai",
        name: "xAI",
        base_url: "https://api.x.ai/v1",
        env_key: Some("XAI_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::Responses,
        hint: "Grok",
    },
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        env_key: Some("OPENROUTER_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "one key, most models",
    },
    Preset {
        id: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        env_key: Some("GROQ_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "fast open-weight inference",
    },
    Preset {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        env_key: Some("DEEPSEEK_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "DeepSeek V3 and R1",
    },
    Preset {
        id: "mistral",
        name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        env_key: Some("MISTRAL_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "Mistral and Codestral",
    },
    Preset {
        id: "together",
        name: "Together",
        base_url: "https://api.together.xyz/v1",
        env_key: Some("TOGETHER_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "hosted open-weight models",
    },
    Preset {
        id: "fireworks",
        name: "Fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        env_key: Some("FIREWORKS_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "hosted open-weight models",
    },
    Preset {
        id: "cerebras",
        name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        env_key: Some("CEREBRAS_API_KEY"),
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "very high throughput",
    },
    Preset {
        id: "ollama",
        name: "Ollama",
        base_url: "http://localhost:11434/v1",
        env_key: None,
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "local models, no key",
    },
    Preset {
        id: "local",
        name: "llama.cpp / vLLM",
        base_url: "http://localhost:8000/v1",
        env_key: None,
        fallback_model: "",
        protocol: ProviderProtocol::ChatCompletions,
        hint: "local OpenAI-compatible server",
    },
];

/// Whether a preset's key is already exported, so the list can say so before
/// the user picks and finds out.
pub fn key_in_env(preset: &Preset) -> bool {
    preset
        .env_key
        .and_then(|name| std::env::var(name).ok())
        .is_some_and(|value| !value.trim().is_empty())
}

/// Print the provider table: one row each, columns padded by display width so
/// the hint and key columns line up regardless of name length.
fn print_presets() {
    let marks = console::marks();
    let name_width = PRESETS
        .iter()
        .map(|preset| preset.name.len())
        .max()
        .unwrap_or(10);
    let hint_width = PRESETS
        .iter()
        .map(|preset| preset.hint.len())
        .max()
        .unwrap_or(20);
    for (index, preset) in PRESETS.iter().enumerate() {
        // A key already in the environment is the single most useful thing to
        // know here — it decides whether the next step will just work.
        let status = match preset.env_key {
            Some(name) if key_in_env(preset) => console::ok(&format!("{} {name}", marks.pass)),
            Some(name) => console::dim(&format!("  {name}")),
            None => console::dim("  no key needed"),
        };
        println!(
            "    {}  {}  {}  {}",
            console::accent(&format!("{:>2}", index + 1)),
            console::pad(preset.name, name_width),
            console::dim(&console::pad(preset.hint, hint_width)),
            status,
        );
    }
    println!(
        "    {}  Custom OpenAI-compatible endpoint",
        console::accent(&format!("{:>2}", PRESETS.len() + 1)),
    );
}

/// Normalise what someone types into a base URL: add a scheme when it is
/// missing, drop a trailing slash, and reject anything that clearly is not a
/// URL before it becomes a confusing connection error later.
fn normalise_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("a base URL is required");
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else if trimmed.starts_with("localhost") || trimmed.starts_with("127.0.0.1") {
        format!("http://{trimmed}")
    } else {
        format!("https://{trimmed}")
    };
    if with_scheme.contains(' ') {
        bail!("a base URL cannot contain spaces");
    }
    Ok(with_scheme)
}

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
        console::banner("Fast, focused coding from the terminal.");
        console::blank();
        console::field("profile", &settings.default_profile);
        if let Some(profile) = settings.profiles.get(&settings.default_profile) {
            console::field("model", &profile.model);
            console::field("endpoint", &profile.base_url);
        }
        console::blank();
        if !console::confirm("A provider is already configured. Reconfigure it?", false)? {
            println!("  {}", console::dim("Kept the existing configuration."));
            return Ok(());
        }
    } else {
        console::banner("Fast, focused coding from the terminal.");
    }

    // ---- 1/3 provider -----------------------------------------------------
    console::step(1, 3, "Choose a provider");
    console::note(
        "Credentials stay on this machine. Every option is an OpenAI-compatible endpoint.",
    );
    console::blank();
    print_presets();
    console::blank();

    let selection = console::prompt_index("Provider", 1, PRESETS.len() + 1)?;
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
            console::blank();
            let name = console::prompt("Profile name", Some("custom"))?;
            let base = loop {
                let raw = console::prompt("API base URL", Some("http://localhost:8000/v1"))?;
                match normalise_base_url(&raw) {
                    Ok(url) => break url,
                    Err(error) => println!("      {}", console::err(&format!("{error}"))),
                }
            };
            let env = console::prompt("API key environment variable (blank for none)", None)?;
            let protocol =
                match console::prompt("Protocol — 1 chat-completions, 2 responses", Some("1"))?
                    .as_str()
                {
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
        .filter(|value| !value.trim().is_empty())
        .or_else(|| credentials.keys.get(&profile_id).cloned());
    let key_from_env = env_key
        .as_deref()
        .is_some_and(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()));
    if key_from_env {
        console::blank();
        println!(
            "  {} {}",
            console::ok(console::marks().pass),
            console::dim(&format!(
                "Using {} from your environment.",
                env_key.as_deref().unwrap_or("the API key")
            ))
        );
    } else if env_key.is_some() && key.is_none() {
        let env_name = env_key.as_deref().unwrap_or("API_KEY");
        console::blank();
        console::note(&format!(
            "{env_name} is not set. A pasted key is stored in {} with owner-only permissions.",
            paths.credentials_file.display()
        ));
        let pasted = rpassword::prompt_password(format!(
            "  {} Paste an API key (blank to skip): ",
            console::accent(console::marks().arrow)
        ))?;
        if !pasted.trim().is_empty() {
            key = Some(pasted.trim().to_owned());
            credentials
                .keys
                .insert(profile_id.clone(), pasted.trim().to_owned());
        }
    }

    // ---- 2/3 model --------------------------------------------------------
    console::step(2, 3, "Connect and choose a model");
    print!(
        "       {}",
        console::dim("Reaching the provider and discovering models… ")
    );
    io::stdout().flush()?;
    let discovered = discover_models(&base_url, key.as_deref()).await;
    let model = match discovered {
        Ok(models) if !models.is_empty() => {
            println!("{}", console::ok(&format!("{} found", models.len())));
            choose_model(&models, &fallback_model)?
        }
        Ok(_) => {
            println!("{}", console::warn("none returned"));
            console::note("The endpoint answered but listed no models; enter one by hand.");
            prompt_model(&fallback_model)?
        }
        Err(error) => {
            println!("{}", console::err("unavailable"));
            console::note(&one_line(&format!("{error:#}"), 200));
            console::note(
                "You can save now and fix connectivity later with /config or `abacus doctor`.",
            );
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
            api_key_env: env_key.clone(),
        },
    );
    settings.default_profile = profile_id.clone();

    // ---- 3/3 defaults -----------------------------------------------------
    console::step(3, 3, "Choose working defaults");
    console::note("All of these are changeable later with /config.");
    console::blank();
    let approve_automatically = console::confirm(
        "Allow file edits and commands without asking each time?",
        settings.ui.permission_mode == PermissionMode::AlwaysApprove,
    )?;
    settings.ui.permission_mode = if approve_automatically {
        PermissionMode::AlwaysApprove
    } else {
        PermissionMode::Ask
    };
    settings.ui.vim_mode = console::confirm(
        "Enable Vim keybindings in the composer?",
        settings.ui.vim_mode,
    )?;
    settings.ui.show_tooltips = console::confirm(
        "Show quick-start guidance on new sessions?",
        settings.ui.show_tooltips,
    )?;
    settings.search.enabled = console::confirm(
        "Enable web search (web_search / read_page tools)?",
        settings.search.enabled,
    )?;
    if settings.search.enabled {
        console::blank();
        for (index, (name, requirement)) in [
            ("DuckDuckGo", "no API key"),
            ("Brave", "needs BRAVE_API_KEY"),
            ("Tavily", "needs TAVILY_API_KEY"),
        ]
        .iter()
        .enumerate()
        {
            println!(
                "    {}  {}  {}",
                console::accent(&format!("{:>2}", index + 1)),
                console::pad(name, 12),
                console::dim(requirement)
            );
        }
        let default = match settings.search.backend {
            SearchBackend::Brave => "2",
            SearchBackend::Tavily => "3",
            SearchBackend::Duckduckgo => "1",
        };
        let (backend, env_var) = match console::prompt("Search backend", Some(default))?.trim() {
            "2" => (SearchBackend::Brave, Some("BRAVE_API_KEY")),
            "3" => (SearchBackend::Tavily, Some("TAVILY_API_KEY")),
            _ => (SearchBackend::Duckduckgo, None),
        };
        settings.search.backend = backend;
        if let Some(env_var) = env_var
            && std::env::var(env_var)
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
        {
            println!(
                "      {}",
                console::warn(&format!(
                    "{env_var} is not set — export it, or name another with `[search] api_key_env`."
                ))
            );
        }
    }

    settings.save(paths)?;
    let stored_key = credentials.keys.contains_key(&profile_id);
    if stored_key {
        credentials.save(paths)?;
    }

    // ---- summary ----------------------------------------------------------
    console::blank();
    println!(
        "  {}",
        console::dim(&console::marks().rule.repeat(console::WIDTH))
    );
    println!(
        "  {} {}",
        console::ok(console::marks().pass),
        console::bold("Ready")
    );
    console::blank();
    console::field("profile", &profile_id);
    console::field("model", &model);
    console::field("endpoint", &base_url);
    console::field(
        "auth",
        &if key_from_env {
            format!("{} (environment)", env_key.as_deref().unwrap_or("api key"))
        } else if stored_key {
            format!("stored in {}", paths.credentials_file.display())
        } else {
            "none".to_owned()
        },
    );
    console::field("settings", &paths.config_file.display().to_string());
    console::blank();
    println!(
        "  {}  {}",
        console::dim("start"),
        console::accent("cd your-project && abacus")
    );
    println!(
        "  {}  {}",
        console::dim("check"),
        console::accent("abacus doctor")
    );
    console::blank();
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

/// Present the discovered models. A provider can list hundreds, so the list is
/// filterable: typing a fragment narrows it rather than forcing a scroll
/// through everything, and an empty filter shows a bounded first page.
fn choose_model(models: &[String], preferred: &str) -> Result<String> {
    const PAGE: usize = 24;
    let mut filter = String::new();
    loop {
        let matched = models
            .iter()
            .filter(|model| {
                filter.is_empty()
                    || model
                        .to_ascii_lowercase()
                        .contains(&filter.to_ascii_lowercase())
            })
            .collect::<Vec<_>>();
        console::blank();
        if matched.is_empty() {
            println!("      {}", console::warn("Nothing matches that filter."));
        }
        for (index, model) in matched.iter().take(PAGE).enumerate() {
            let marker = if *model == preferred {
                console::ok(" (current)")
            } else {
                String::new()
            };
            println!(
                "    {}  {}{marker}",
                console::accent(&format!("{:>2}", index + 1)),
                model
            );
        }
        let hidden = matched.len().saturating_sub(PAGE);
        if hidden > 0 {
            println!(
                "        {}",
                console::dim(&format!("+{hidden} more — type a fragment to filter"))
            );
        }
        console::blank();
        println!(
            "        {}",
            console::dim("number to pick  ·  text to filter  ·  blank to type an ID")
        );
        let answer = console::prompt("Model", None)?;
        if answer.is_empty() {
            return prompt_model(preferred);
        }
        if let Ok(index) = answer.parse::<usize>() {
            if index >= 1 && index <= matched.len().min(PAGE) {
                return Ok(matched[index - 1].clone());
            }
            println!("      {}", console::err("That number is not on the list."));
            continue;
        }
        filter = answer;
    }
}

/// Ask for a model ID, re-asking on an empty answer. Bailing here would throw
/// away everything already chosen, which is a harsh penalty for pressing enter.
fn prompt_model(fallback: &str) -> Result<String> {
    loop {
        let model = console::prompt(
            "Model ID",
            if fallback.is_empty() {
                None
            } else {
                Some(fallback)
            },
        )?;
        if !model.trim().is_empty() {
            return Ok(model.trim().to_owned());
        }
        println!(
            "      {}",
            console::err("A model ID is required — for example gpt-5-codex or grok-4.")
        );
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

    #[test]
    fn base_urls_gain_a_scheme_and_lose_a_trailing_slash() {
        assert_eq!(
            normalise_base_url("api.example.com/v1/").unwrap(),
            "https://api.example.com/v1"
        );
        // A local address defaults to http — https on localhost is the rarer
        // case and produces a confusing TLS error rather than a clear one.
        assert_eq!(
            normalise_base_url("localhost:11434/v1").unwrap(),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalise_base_url("http://127.0.0.1:8000/v1").unwrap(),
            "http://127.0.0.1:8000/v1"
        );
        assert!(normalise_base_url("  ").is_err());
        assert!(normalise_base_url("not a url").is_err());
    }

    #[test]
    fn every_preset_is_well_formed() {
        let mut ids = std::collections::HashSet::new();
        for preset in PRESETS {
            assert!(ids.insert(preset.id), "duplicate preset id {}", preset.id);
            assert_eq!(
                normalise_base_url(preset.base_url).unwrap(),
                preset.base_url,
                "{} base URL is not already normalised",
                preset.id
            );
            assert!(!preset.hint.is_empty(), "{} has no hint", preset.id);
            // A hosted endpoint needs a key; a local one must not claim to.
            let local = preset.base_url.contains("localhost");
            assert_eq!(
                preset.env_key.is_none(),
                local,
                "{} disagrees about needing a key",
                preset.id
            );
        }
    }
}
