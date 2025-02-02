use dotenv::dotenv;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs::{read_dir, read_to_string};
use std::process::exit;

#[derive(Debug, Deserialize, Serialize, Clone)]
struct BotConfig {
    bot_name: String,
    telegram_bot_token: String,
    telegram_secret_token: String,
    openai_assistant_id: String,
    vector_store_id: String,
}

/// Loads all bot configuration files from the "bot_config" directory.
fn load_bot_configs() -> Result<HashMap<String, BotConfig>, Box<dyn Error>> {
    let mut configs = HashMap::new();
    let config_dir = "bot_config";
    for entry in read_dir(config_dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
            let contents = read_to_string(entry.path())?;
            let config: BotConfig = serde_json::from_str(&contents)?;
            configs.insert(config.bot_name.clone(), config);
        }
    }
    Ok(configs)
}

fn main() -> Result<(), Box<dyn Error>> {
    dotenv().ok();

    // Load webhook domain from .env
    let webhook_domain = env::var("WEBHOOK_DOMAIN").expect("WEBHOOK_DOMAIN not set in .env");

    // Load all bot configurations.
    let bot_configs = load_bot_configs()?;
    if bot_configs.is_empty() {
        eprintln!("No bot configurations found in bot_config/");
        exit(1);
    }

    let client = Client::new();

    // Process each bot.
    for (bot_name, config) in bot_configs {
        println!("===================================");
        println!("Processing bot: {}", bot_name);
        let token = &config.telegram_bot_token;

        // 1. Get current webhook info.
        let get_url = format!("https://api.telegram.org/bot{}/getWebhookInfo", token);
        println!("Fetching current webhook info for {}...", bot_name);
        match client.get(&get_url).send() {
            Ok(response) => {
                let text = response.text().unwrap_or_default();
                println!("Current webhook info for {}:\n{}", bot_name, text);
            }
            Err(e) => println!("Error fetching webhook info for {}: {}", bot_name, e),
        }

        // 2. Delete existing webhook.
        let delete_url = format!("https://api.telegram.org/bot{}/deleteWebhook", token);
        println!("Deleting webhook for {}...", bot_name);
        match client.post(&delete_url).send() {
            Ok(response) => {
                let text = response.text().unwrap_or_default();
                println!("Delete webhook response for {}:\n{}", bot_name, text);
            }
            Err(e) => println!("Error deleting webhook for {}: {}", bot_name, e),
        }

        // 3. Set new webhook.
        // Construct the new webhook URL as: "{WEBHOOK_DOMAIN}/{bot_name}/webhook"
        let new_webhook_url = format!("{}/{}/webhook", webhook_domain, bot_name);
        println!(
            "Setting webhook for {} to URL: {}",
            bot_name, new_webhook_url
        );
        // Allowed updates is set to only "message"
        let params = [
            ("url", new_webhook_url.as_str()),
            ("secret_token", config.telegram_secret_token.as_str()),
            ("allowed_updates", "[\"message\"]"),
        ];
        let set_url = format!("https://api.telegram.org/bot{}/setWebhook", token);
        match client.post(&set_url).form(&params).send() {
            Ok(response) => {
                let text = response.text().unwrap_or_default();
                println!("Set webhook response for {}:\n{}", bot_name, text);
            }
            Err(e) => println!("Error setting webhook for {}: {}", bot_name, e),
        }
        println!("===================================\n");
    }

    Ok(())
}
