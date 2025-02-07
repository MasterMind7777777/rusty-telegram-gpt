use chrono::{DateTime, Duration as ChronoDuration, Utc};
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

// ---------------- Sled-Based User Context ----------------

#[derive(Serialize, Deserialize, Debug)]
struct UserContext {
    thread_id: String,
    last_inquiry: DateTime<Utc>,
}

static USER_CONTEXT_DB: Lazy<sled::Db> =
    Lazy::new(|| sled::open("user_context_db").expect("Failed to open sled database"));

fn store_user_context(
    user_id: i64,
    context: &UserContext,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let key = user_id.to_string();
    let value = serde_json::to_vec(context)?;
    USER_CONTEXT_DB.insert(key, value)?;
    USER_CONTEXT_DB.flush()?;
    Ok(())
}

fn get_user_context(
    user_id: i64,
) -> Result<Option<UserContext>, Box<dyn std::error::Error + Send + Sync>> {
    let key = user_id.to_string();
    if let Some(value) = USER_CONTEXT_DB.get(key)? {
        let context: UserContext = serde_json::from_slice(&value)?;
        Ok(Some(context))
    } else {
        Ok(None)
    }
}

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

// Loads the friendly file names for a given vector store ID.
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

            // Check and remove an expired user context (if any)
            if let Some(uid) = message.from.as_ref().map(|u| u.id) {
                match get_user_context(uid) {
                    Ok(Some(context)) => {
                        let now = Utc::now();
                        if now.signed_duration_since(context.last_inquiry)
                            > ChronoDuration::minutes(10)
                        {
                            info!(
                                "Context for user {} is older than 10 minutes. Resetting.",
                                uid
                            );
                            let _ = USER_CONTEXT_DB.remove(uid.to_string());
                        } else {
                            info!("Existing context for user {}: {:?}", uid, context);
                        }
                    }
                    Ok(None) => info!("No existing context for user {}", uid),
                    Err(e) => error!("Error retrieving user context: {}", e),
                }
            }

            info!("Received message from chat {}: {}", chat_id, text);
            let bot_config_clone = bot_config.clone();
            let bot_name_owned = bot_name.to_owned();
            // Pass the optional user_id (if available) to the spawned task.
            let user_id = message.from.as_ref().map(|u| u.id);
            tokio::spawn(async move {
                call_openai_api_and_send(chat_id, text, user_id, &bot_config_clone, bot_name_owned)
                    .await;
            });
        } else {
            info!("No text found in the message.");
        }
    } else {
        info!("No message found in update.");
    }
    Ok(Response::new(Body::from("OK")))
}

// ---------------- New Helper Functions for Endpoints ----------------

/// Creates a new thread using the Create Thread endpoint.
async fn create_thread(
    client: &reqwest::Client,
    token: &str,
    prompt: &str,
    bot_config: &BotConfig,
) -> Result<String, reqwest::Error> {
    let url = "https://api.openai.com/v1/threads";
    let payload = json!({
        "messages": [
            { "role": "user", "content": prompt }
        ],
        "tool_resources": {
            "file_search": {
                "vector_store_ids": [ bot_config.vector_store_id ]
            }
        }
    });
    let resp = client
        .post(url)
        .header("OpenAI-Beta", "assistants=v2")
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await?;
    let resp_json: serde_json::Value = resp.json().await?;
    // Expect the thread object to include an "id" field.
    let thread_id = resp_json["id"].as_str().unwrap_or_default().to_string();
    Ok(thread_id)
}

/// Creates a new message in an existing thread.
async fn create_message(
    client: &reqwest::Client,
    token: &str,
    thread_id: &str,
    prompt: &str,
) -> Result<(), reqwest::Error> {
    let url = format!("https://api.openai.com/v1/threads/{}/messages", thread_id);
    let payload = json!({
        "role": "user",
        "content": prompt,
    });
    client
        .post(&url)
        .header("OpenAI-Beta", "assistants=v2")
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await?;
    Ok(())
}

/// Creates a run (with streaming enabled) for the given thread.
async fn create_run(
    client: &reqwest::Client,
    token: &str,
    thread_id: &str,
    bot_config: &BotConfig,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("https://api.openai.com/v1/threads/{}/runs", thread_id);
    let payload = json!({
        "assistant_id": bot_config.openai_assistant_id,
        "stream": true,
        // You can include other optional parameters here.
    });
    let resp = client
        .post(&url)
        .header("OpenAI-Beta", "assistants=v2")
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await?;
    Ok(resp)
}

// ---------------- Updated OpenAI API Call ----------------

/// Depending on whether there’s a stored thread (and it’s less than 10 minutes old),
/// either create a new thread or use the existing one.
/// Then, post a new message (if using an existing thread) and create a run.
async fn call_openai_api_and_send(
    chat_id: i64,
    prompt: String,
    user_id: Option<i64>,
    bot_config: &BotConfig,
    bot_name: String,
) {
    info!("Calling OpenAI API with prompt: {}", prompt);
    let openai_token = env::var("OPENAI_TOKEN").expect("OPENAI_TOKEN not set in .env");
    let client = reqwest::Client::new();
    let thread_id: String;

    // If we have a user_id, try to use the stored context.
    if let Some(uid) = user_id {
        if let Ok(Some(context)) = get_user_context(uid) {
            thread_id = context.thread_id;
            // Post a new message to the existing thread.
            if let Err(e) = create_message(&client, &openai_token, &thread_id, &prompt).await {
                error!("Error creating message in thread {}: {}", thread_id, e);
                let _ = send_reply(chat_id, "Error creating message".to_string(), &bot_name).await;
                return;
            }
        } else {
            // No valid stored thread—create a new one.
            match create_thread(&client, &openai_token, &prompt, bot_config).await {
                Ok(tid) => thread_id = tid,
                Err(e) => {
                    error!("Error creating thread: {}", e);
                    let _ =
                        send_reply(chat_id, "Error creating thread".to_string(), &bot_name).await;
                    return;
                }
            }
        }
    } else {
        // No user_id provided; create a new thread.
        match create_thread(&client, &openai_token, &prompt, bot_config).await {
            Ok(tid) => thread_id = tid,
            Err(e) => {
                error!("Error creating thread: {}", e);
                let _ = send_reply(chat_id, "Error creating thread".to_string(), &bot_name).await;
                return;
            }
        }
    }

    // Now create a run for the thread.
    let run_resp = match create_run(&client, &openai_token, &thread_id, bot_config).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Error creating run: {}", e);
            let _ = send_reply(chat_id, "Error creating run".to_string(), &bot_name).await;
            return;
        }
    };

    info!(
        "Started streaming response from OpenAI on thread {}.",
        thread_id
    );
    let mut sentence_buffer = String::new();
    let stream_timeout = Duration::from_secs(30);
    let stream_future = async {
        let mut response_stream = run_resp.bytes_stream();
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
                        if process_event_block(
                            event_block,
                            &mut sentence_buffer,
                            chat_id,
                            &bot_name,
                        )
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
        if let Err(e) = send_reply(chat_id, remaining, &bot_name).await {
            error!("Failed to send Telegram message: {}", e);
        }
    }

    // Update stored context with the (new or reused) thread ID.
    if let Some(uid) = user_id {
        let new_context = UserContext {
            thread_id: thread_id.clone(),
            last_inquiry: Utc::now(),
        };
        if let Err(e) =
            tokio::task::spawn_blocking(move || store_user_context(uid, &new_context)).await
        {
            error!("Failed to update user context for user {}: {:?}", uid, e);
        } else {
            info!("User context updated for user {}", uid);
        }
    }
}

// ---------------- SSE Processing ----------------

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

// ---------------- Telegram Reply ----------------

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

    let configs = load_bot_configs()?;
    {
        let mut bot_configs = BOT_CONFIGS.lock().unwrap();
        *bot_configs = configs;
    }
    info!(
        "Loaded bot configurations for {} bot(s).",
        BOT_CONFIGS.lock().unwrap().len()
    );

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
