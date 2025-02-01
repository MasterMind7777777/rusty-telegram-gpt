// src/bin/webhook_creator.rs

use dotenv::dotenv;
use reqwest::blocking::Client;
use std::env;

fn main() {
    dotenv().ok();
    let telegram_token =
        env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set in .env");
    let domain = env::var("WEBHOOK_DOMAIN").expect("WEBHOOK_DOMAIN not set in .env");
    let secret_token =
        env::var("TELEGRAM_SECRET_TOKEN").unwrap_or_else(|_| "default_secret".to_string());
    let url = format!("https://api.telegram.org/bot{}/setWebhook", telegram_token);
    let webhook_url = format!("{}/webhook", domain);

    let client = Client::new();
    // Specify allowed_updates to receive only "message" updates.
    let params = [
        ("url", webhook_url.as_str()),
        ("secret_token", secret_token.as_str()),
        ("allowed_updates", "[\"message\"]"),
    ];

    match client.post(&url).form(&params).send() {
        Ok(response) => {
            let text = response.text().unwrap_or_default();
            println!("Webhook set response: {}", text);
        }
        Err(err) => {
            eprintln!("Error setting webhook: {}", err);
        }
    }
}
