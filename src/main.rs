use dotenv::dotenv;
use futures_util::StreamExt; // for bytes_stream and stream.next()
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use log::{error, info};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::env;
use std::fs::read_to_string;
use std::io::Write;
use std::sync::Mutex;
use tokio::time::{timeout, Duration};

// ---------------- Global File Mapping ----------------
// This global will store a vector of friendly filenames in the order
// corresponding to the file details returned by OpenAI.
// We assume that the citation marker's first number is 1-indexed into this vector.
static FILE_NAMES: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

// Helper function to load file names from a JSON file.
// We expect the file details JSON to be located at "{DATA_DIR}/{store_id}_files_details.json".
fn load_file_names(store_id: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // Get the data directory from environment (or default to "./data")
    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let path = format!("{}/{}_files_details.json", data_dir, store_id);
    let contents = read_to_string(&path)?;
    // Parse as JSON array of objects.
    let file_details: Vec<serde_json::Value> = serde_json::from_str(&contents)?;
    // Extract the "filename" field from each file.
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

// Helper function to replace citation markers in the text using the global FILE_NAMES vector.
// It looks for markers of the form "【(\d+):(\d+)†source】" and replaces the first number
// (assumed to be 1-indexed) with the corresponding friendly filename.
fn replace_citations(text: &str) -> String {
    // Regex to capture markers like "【4:3†source】"
    let re = Regex::new(r"【(\d+):(\d+)†source】").unwrap();
    // Lock the global FILE_NAMES vector.
    let file_names = FILE_NAMES.lock().unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        if let Ok(idx) = caps[1].parse::<usize>() {
            // Convert 1-indexed to 0-indexed.
            if idx > 0 && idx <= file_names.len() {
                return format!("【{}†source】", file_names[idx - 1]);
            }
        }
        // Fallback: return the original marker if parsing fails or index is out of range.
        caps[0].to_string()
    })
    .into_owned()
}

/// Helper: Convert the full request body to bytes.
async fn body_to_bytes(body: Body) -> Result<Vec<u8>, hyper::Error> {
    info!("Converting request body to bytes...");
    let bytes = to_bytes(body).await?;
    info!("Finished converting body ({} bytes)", bytes.len());
    Ok(bytes.to_vec())
}

/// HTTP handler for incoming webhook requests.
async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let raw_path = req.uri().path();
    info!("Incoming request: {} {}", req.method(), raw_path);

    // Normalize the path by trimming both leading and trailing slashes.
    let normalized_path = raw_path.trim_matches('/');
    info!("Normalized path: {}", normalized_path);

    if req.method() == Method::POST && normalized_path == "webhook" {
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

        let update: Update = match serde_json::from_slice(&whole_body) {
            Ok(upd) => {
                info!("Parsed update successfully.");
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
                // Spawn a task that streams and sends Telegram messages as data arrives.
                tokio::spawn(async move {
                    call_openai_api_and_send(chat_id, text).await;
                });
            } else {
                info!("No text found in the message.");
            }
        } else {
            info!("No message found in update.");
        }
        Ok(Response::new(Body::from("OK")))
    } else {
        info!("Request did not match POST webhook endpoint.");
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap())
    }
}

/// Processes a single SSE data line.
/// Returns true if a termination signal ("[DONE]") is encountered.
async fn process_line(line: &str, sentence_buffer: &mut String, chat_id: i64) -> bool {
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
            // Append received text to the buffer.
            sentence_buffer.push_str(text_val);
            info!("Buffer updated: {}", sentence_buffer);

            // Check for double newlines and send complete sentences.
            while let Some(pos) = sentence_buffer.find("\n\n") {
                let sentence: String = sentence_buffer.drain(..pos + 2).collect();
                let sentence = sentence.trim();
                if !sentence.is_empty() {
                    // Replace citation markers with friendly file names.
                    let processed_sentence = replace_citations(sentence);
                    info!("Sending sentence: {}", processed_sentence);
                    if let Err(e) = send_reply(chat_id, processed_sentence).await {
                        error!("Failed to send Telegram message: {}", e);
                    }
                }
            }
        }
    }
    false
}

/// Processes an entire SSE event block (split by double newlines).
/// Returns true if a termination signal is encountered.
async fn process_event_block(
    event_block: &str,
    sentence_buffer: &mut String,
    chat_id: i64,
) -> bool {
    // Process each line in the event block.
    for line in event_block.lines() {
        if process_line(line, sentence_buffer, chat_id).await {
            return true;
        }
    }
    false
}

/// Calls the OpenAI API with streaming enabled and sends Telegram messages
/// whenever a double newline delimiter is found in the accumulated text.
async fn call_openai_api_and_send(chat_id: i64, prompt: String) {
    info!("Calling OpenAI API with prompt: {}", prompt);
    let openai_token = env::var("OPENAI_TOKEN").expect("OPENAI_TOKEN not set in .env");
    let client = reqwest::Client::new();
    let assistant_id = "asst_a4UKVFs40KuWDofTyc3aqblf"; // DAPConsult's assistant ID

    let run_url = "https://api.openai.com/v1/threads/runs";
    let run_payload = json!({
        "assistant_id": assistant_id,
        "thread": {
            "messages": [
                { "role": "user", "content": prompt }
            ],
            "tool_resources": {
                "file_search": {
                    "vector_store_ids": ["vs_GKxymyy9y9UG5XjbxmVkpm6I"]
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
        let _ = send_reply(chat_id, "Error calling OpenAI API".to_string()).await;
        return;
    }

    let response = resp.unwrap();
    info!("Started streaming response from OpenAI.");

    // A buffer to accumulate text until a double newline is encountered.
    let mut sentence_buffer = String::new();
    // Set a maximum duration for streaming (e.g. 30 seconds).
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
                    // Normalize newlines.
                    let normalized_chunk = chunk_str.replace("\r\n", "\n");
                    // Split by double newline to separate SSE events.
                    for event_block in normalized_chunk.split("\n\n") {
                        let event_block = event_block.trim();
                        if event_block.is_empty() {
                            continue;
                        }
                        info!("Processing event block: {}", event_block);
                        // Process this event block.
                        if process_event_block(event_block, &mut sentence_buffer, chat_id).await {
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

    // At the end of the stream, send any remaining text.
    if !sentence_buffer.trim().is_empty() {
        let remaining = sentence_buffer.trim().to_string();
        info!("Sending remaining text: {}", remaining);
        if let Err(e) = send_reply(chat_id, remaining).await {
            error!("Failed to send Telegram message: {}", e);
        }
    }
}

/// Sends the reply to the Telegram Bot API.
async fn send_reply(chat_id: i64, text: String) -> Result<(), reqwest::Error> {
    info!("Sending reply to chat {}: {}", chat_id, text);
    let telegram_token =
        env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set in .env");
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

#[derive(Deserialize)]
#[allow(dead_code)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Message {
    message_id: i64,
    from: Option<User>,
    chat: Chat,
    date: i64,
    text: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct User {
    id: i64,
    first_name: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Chat {
    id: i64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    // Initialize file mapping from a JSON file.
    // Assume the vector store ID is fixed for your bot (used in the run_payload)
    let store_id = "vs_GKxymyy9y9UG5XjbxmVkpm6I";
    match load_file_names(store_id) {
        Ok(names) => {
            let mut mapping = FILE_NAMES.lock().unwrap();
            *mapping = names;
            info!("Loaded {} file names.", mapping.len());
        }
        Err(e) => {
            error!("Failed to load file names: {}", e);
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("Starting application...");
    std::io::stdout().flush().unwrap();

    info!("Environment loaded.");
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
