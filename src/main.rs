use dotenv::dotenv;
use futures_util::StreamExt;
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use log::{error, info};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::fs::{read_dir, read_to_string};
use std::io::Write;
use std::sync::Mutex;
use tokio::time::{timeout, Duration};

// ---------------- Global Bot Configuration and File Mappings ----------------

static BOT_CONFIGS: Lazy<Mutex<HashMap<String, BotConfig>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static FILE_MAPPINGS: Lazy<Mutex<HashMap<String, Vec<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// ---------------- Bot Configuration Types ----------------

#[derive(Debug, Deserialize, Serialize, Clone)]
struct BotConfig {
    bot_name: String,
    telegram_bot_token: String,
    telegram_secret_token: String,
    openai_assistant_id: String,
    vector_store_id: String,
}

// Loads all bot configuration files from the "bot_config" directory.
fn load_bot_configs() -> Result<HashMap<String, BotConfig>, Box<dyn std::error::Error>> {
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

// Loads the friendly file names for a given vector store ID from "{DATA_DIR}/{vector_store_id}_files_details.json".
fn load_file_names(vector_store_id: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let path = format!("{}/{}_files_details.json", data_dir, vector_store_id);
    let contents = read_to_string(&path)?;
    let file_details: Vec<serde_json::Value> = serde_json::from_str(&contents)?;
    let names = file_details
        .iter()
        .filter_map(|v| {
            v.get("filename")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    Ok(names)
}

// ---------------- Citation Replacement ----------------

/// Replaces citation markers of the form "【(\d+):(\d+)†source】" in the given text
/// with friendly file names using the mapping for the given bot.
fn replace_citations(text: &str, bot_name: &str) -> String {
    let re = Regex::new(r"【(\d+):(\d+)†source】").unwrap();
    let file_mapping = FILE_MAPPINGS.lock().unwrap();
    let names = file_mapping.get(bot_name);
    re.replace_all(text, |caps: &regex::Captures| {
        if let Some(names) = names {
            if let Ok(idx) = caps[1].parse::<usize>() {
                if idx > 0 && idx <= names.len() {
                    return format!("【{}†source】", names[idx - 1]);
                }
            }
        }
        caps[0].to_string()
    })
    .into_owned()
}

// ---------------- HTTP Helper ----------------

/// Reads the full request body into bytes.
async fn body_to_bytes(body: Body) -> Result<Vec<u8>, hyper::Error> {
    info!("Converting request body to bytes...");
    let bytes = to_bytes(body).await?;
    info!("Finished converting body ({} bytes)", bytes.len());
    Ok(bytes.to_vec())
}

// ---------------- Telegram Update Types ----------------

#[derive(Deserialize, Debug)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Deserialize, Debug)]
struct Message {
    message_id: i64,
    from: Option<User>,
    chat: Chat,
    date: i64,
    text: Option<String>,
}

#[derive(Deserialize, Debug)]
struct User {
    id: i64,
    first_name: String,
}

#[derive(Deserialize, Debug)]
struct Chat {
    id: i64,
}

// ---------------- HTTP Handler ----------------

/// Expects the URL path to be in the format "/{bot_name}/webhook".
async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let raw_path = req.uri().path().to_owned();
    info!("Incoming request: {} {}", req.method(), raw_path);
    let segments: Vec<&str> = raw_path.trim_matches('/').split('/').collect();
    if segments.len() < 2 || segments[1] != "webhook" {
        info!("Request did not match expected format.");
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap());
    }
    let bot_name = segments[0];
    info!("Bot name from URL: {}", bot_name);

    // Clone the bot configuration so no MutexGuard is held across awaits.
    let bot_config = {
        let bot_configs = BOT_CONFIGS.lock().unwrap();
        bot_configs.get(bot_name).cloned()
    };
    let bot_config = match bot_config {
        Some(cfg) => cfg,
        None => {
            error!("No configuration found for bot: {}", bot_name);
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Bot not found"))
                .unwrap());
        }
    };

    let whole_body = match body_to_bytes(req.into_body()).await {
        Ok(bytes) => {
            info!("Received body: {:?}", String::from_utf8_lossy(&bytes));
            bytes
        }
        Err(err) => {
            error!("Error reading body: {}", err);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Bad Request"))
                .unwrap());
        }
    };

    let update: Update = match serde_json::from_slice::<Update>(&whole_body) {
        Ok(upd) => {
            // Now we can log the fields (thus "using" them).
            info!("Parsed update with update_id: {}", upd.update_id);
            if let Some(ref msg) = upd.message {
                info!(
                    "Message details: message_id: {}, date: {}",
                    msg.message_id, msg.date
                );
                if let Some(ref user) = msg.from {
                    info!("From: id: {}, first_name: {}", user.id, user.first_name);
                }
            }
            upd
        }
        Err(err) => {
            error!("Failed to parse update: {}", err);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Bad Request"))
                .unwrap());
        }
    };

    if let Some(message) = update.message {
        if let Some(text) = message.text {
            let chat_id = message.chat.id;
            info!("Received message from chat {}: {}", chat_id, text);
            // Clone bot_config and bot_name for the spawned task.
            let bot_config_clone = bot_config.clone();
            let bot_name_owned = bot_name.to_owned();
            tokio::spawn(async move {
                call_openai_api_and_send(chat_id, text, &bot_config_clone, &bot_name_owned).await;
            });
        } else {
            info!("No text found in the message.");
        }
    } else {
        info!("No message found in update.");
    }
    Ok(Response::new(Body::from("OK")))
}

// ---------------- SSE Processing ----------------

/// Processes a single SSE data line and, after replacing citation markers, sends the sentence.
async fn process_line(
    line: &str,
    sentence_buffer: &mut String,
    chat_id: i64,
    bot_name: &str,
) -> bool {
    if !line.starts_with("data:") {
        return false;
    }
    let data = line.trim_start_matches("data:").trim();
    if data == "[DONE]" {
        info!("Stream ended with [DONE].");
        return true;
    }
    let delta_json = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(json) => json,
        Err(e) => {
            info!("Failed to parse SSE data: {}. Data: {}", e, data);
            return false;
        }
    };
    let delta = match delta_json.get("delta") {
        Some(d) => d,
        None => return false,
    };
    let content_arr = match delta.get("content").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return false,
    };
    for part in content_arr {
        if let Some(text_val) = part
            .get("text")
            .and_then(|t| t.get("value"))
            .and_then(|s| s.as_str())
        {
            sentence_buffer.push_str(text_val);
            info!("Buffer updated: {}", sentence_buffer);
            while let Some(pos) = sentence_buffer.find("\n\n") {
                let sentence: String = sentence_buffer.drain(..pos + 2).collect();
                let sentence = sentence.trim();
                if !sentence.is_empty() {
                    let processed_sentence = replace_citations(sentence, bot_name);
                    info!("Sending sentence: {}", processed_sentence);
                    if let Err(e) = send_reply(chat_id, processed_sentence, bot_name).await {
                        error!("Failed to send Telegram message: {}", e);
                    }
                }
            }
        }
    }
    false
}

/// Processes an entire SSE event block.
async fn process_event_block(
    event_block: &str,
    sentence_buffer: &mut String,
    chat_id: i64,
    bot_name: &str,
) -> bool {
    for line in event_block.lines() {
        if process_line(line, sentence_buffer, chat_id, bot_name).await {
            return true;
        }
    }
    false
}

// ---------------- OpenAI API Call ----------------

/// Calls the OpenAI API with streaming enabled, using the bot's configuration.
async fn call_openai_api_and_send(
    chat_id: i64,
    prompt: String,
    bot_config: &BotConfig,
    bot_name: &str,
) {
    info!("Calling OpenAI API with prompt: {}", prompt);
    let openai_token = env::var("OPENAI_TOKEN").expect("OPENAI_TOKEN not set in .env");
    let client = reqwest::Client::new();
    let run_url = "https://api.openai.com/v1/threads/runs";
    let run_payload = json!({
        "assistant_id": bot_config.openai_assistant_id,
        "thread": {
            "messages": [
                { "role": "user", "content": prompt }
            ],
            "tool_resources": {
                "file_search": {
                    "vector_store_ids": [ bot_config.vector_store_id ]
                }
            }
        },
        "tools": [
            { "type": "file_search" }
        ],
        "tool_choice": { "type": "file_search" },
        "stream": true
    });
    let resp = client
        .post(run_url)
        .header("OpenAI-Beta", "assistants=v2")
        .bearer_auth(&openai_token)
        .json(&run_payload)
        .send()
        .await;
    if let Err(err) = resp {
        error!("Error calling OpenAI API: {}", err);
        let _ = send_reply(chat_id, "Error calling OpenAI API".to_string(), bot_name).await;
        return;
    }
    let response = resp.unwrap();
    info!("Started streaming response from OpenAI.");
    let mut sentence_buffer = String::new();
    let stream_timeout = Duration::from_secs(30);
    let stream_future = async {
        let mut response_stream = response.bytes_stream();
        while let Some(item) = response_stream.next().await {
            match item {
                Ok(chunk) => {
                    info!("Received a new chunk from the stream.");
                    let chunk_str = match std::str::from_utf8(&chunk) {
                        Ok(s) => s.to_string(),
                        Err(e) => {
                            error!("Failed to decode chunk: {}", e);
                            continue;
                        }
                    };
                    info!("Chunk content: {}", chunk_str);
                    let normalized_chunk = chunk_str.replace("\r\n", "\n");
                    for event_block in normalized_chunk.split("\n\n") {
                        let event_block = event_block.trim();
                        if event_block.is_empty() {
                            continue;
                        }
                        info!("Processing event block: {}", event_block);
                        if process_event_block(event_block, &mut sentence_buffer, chat_id, bot_name)
                            .await
                        {
                            info!("Termination signal encountered during event block processing.");
                            return;
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading stream chunk: {}", e);
                    break;
                }
            }
        }
    };
    let _ = timeout(stream_timeout, stream_future).await;
    if !sentence_buffer.trim().is_empty() {
        let remaining = sentence_buffer.trim().to_string();
        info!("Sending remaining text: {}", remaining);
        if let Err(e) = send_reply(chat_id, remaining, bot_name).await {
            error!("Failed to send Telegram message: {}", e);
        }
    }
}

// ---------------- Telegram Reply ----------------

/// Sends a reply using the Telegram Bot API, using the bot's token.
async fn send_reply(chat_id: i64, text: String, bot_name: &str) -> Result<(), reqwest::Error> {
    info!("Sending reply to chat {}: {}", chat_id, text);
    let bot_config = {
        let bot_configs = BOT_CONFIGS.lock().unwrap();
        bot_configs.get(bot_name).cloned()
    };
    let bot_config = match bot_config {
        Some(cfg) => cfg,
        None => {
            error!("No bot config found for {}", bot_name);
            return Ok(());
        }
    };
    let telegram_token = &bot_config.telegram_bot_token;
    let url = format!("https://api.telegram.org/bot{}/sendMessage", telegram_token);
    let client = reqwest::Client::new();
    let params = json!({
        "chat_id": chat_id,
        "text": text,
    });
    let response = client.post(&url).json(&params).send().await?;
    info!("Telegram API response: {:?}", response.text().await);
    Ok(())
}

// ---------------- Main Initialization ----------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    // Load bot configurations.
    let configs = load_bot_configs()?;
    {
        let mut bot_configs = BOT_CONFIGS.lock().unwrap();
        *bot_configs = configs;
    }
    info!(
        "Loaded bot configurations for {} bot(s).",
        BOT_CONFIGS.lock().unwrap().len()
    );

    // For each bot, load its file names mapping.
    {
        let bot_configs = BOT_CONFIGS.lock().unwrap();
        for config in bot_configs.values() {
            match load_file_names(&config.vector_store_id) {
                Ok(names) => {
                    let mut mappings = FILE_MAPPINGS.lock().unwrap();
                    mappings.insert(config.bot_name.clone(), names);
                    info!("Loaded file names for bot {}.", config.bot_name);
                }
                Err(e) => {
                    error!(
                        "Failed to load file names for bot {}: {}",
                        config.bot_name, e
                    );
                }
            }
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("Starting application...");
    std::io::stdout().flush().unwrap();

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid number");
    let addr = ([0, 0, 0, 0], port).into();
    info!("Binding server to address: http://{}", addr);
    std::io::stdout().flush().unwrap();

    let make_svc = make_service_fn(|_conn| async {
        info!("New connection established.");
        Ok::<_, Infallible>(service_fn(handle_request))
    });
    let server = Server::bind(&addr).serve(make_svc);

    info!("Webhook receiver listening on http://{}", addr);
    std::io::stdout().flush().unwrap();

    match server.await {
        Ok(_) => info!("Server ended gracefully."),
        Err(e) => error!("Server error: {}", e),
    }
    Ok(())
}
