use dotenv::dotenv;
use futures_util::StreamExt; // for bytes_stream and stream.next()
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::env;
use std::io::Write;
use tokio::time::{timeout, Duration};

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

/// Helper: Convert the full request body to bytes.
async fn body_to_bytes(body: Body) -> Result<Vec<u8>, hyper::Error> {
    println!("Converting request body to bytes...");
    let bytes = to_bytes(body).await?;
    println!("Finished converting body ({} bytes)", bytes.len());
    Ok(bytes.to_vec())
}

/// HTTP handler for incoming webhook requests.
async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let raw_path = req.uri().path();
    println!("Incoming request: {} {}", req.method(), raw_path);

    // Normalize the path by trimming both leading and trailing slashes.
    let normalized_path = raw_path.trim_matches('/');
    println!("Normalized path: {}", normalized_path);

    if req.method() == Method::POST && normalized_path == "webhook" {
        let whole_body = match body_to_bytes(req.into_body()).await {
            Ok(bytes) => {
                println!("Received body: {:?}", String::from_utf8_lossy(&bytes));
                bytes
            }
            Err(err) => {
                eprintln!("Error reading body: {}", err);
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from("Bad Request"))
                    .unwrap());
            }
        };

        let update: Update = match serde_json::from_slice(&whole_body) {
            Ok(upd) => {
                println!("Parsed update successfully.");
                upd
            }
            Err(err) => {
                eprintln!("Failed to parse update: {}", err);
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from("Bad Request"))
                    .unwrap());
            }
        };

        if let Some(message) = update.message {
            if let Some(text) = message.text {
                let chat_id = message.chat.id;
                println!("Received message from chat {}: {}", chat_id, text);

                let openai_response = call_openai_api(text).await;
                println!("OpenAI response: {}", openai_response);

                if let Err(e) = send_reply(chat_id, openai_response).await {
                    eprintln!("Failed to send reply: {}", e);
                } else {
                    println!("Reply sent to chat {}", chat_id);
                }
            } else {
                println!("No text found in the message.");
            }
        } else {
            println!("No message found in update.");
        }
        Ok(Response::new(Body::from("OK")))
    } else {
        println!("Request did not match POST webhook endpoint.");
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap())
    }
}

/// Calls the OpenAI API using the "Create thread and run" endpoint with streaming enabled.
/// It waits for SSE events, normalizes newlines, and accumulates reply text from delta events.
/// If the SSE event "[DONE]" is received or a timeout occurs, it returns the accumulated reply.
async fn call_openai_api(prompt: String) -> String {
    println!("Calling OpenAI API with prompt: {}", prompt);
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
        eprintln!("Error calling OpenAI API: {}", err);
        return "Error calling OpenAI API".to_string();
    }

    let response = resp.unwrap();
    println!("Started streaming response from OpenAI.");

    let mut reply = String::new();
    // Set a maximum duration for streaming (e.g. 30 seconds).
    let stream_timeout = Duration::from_secs(30);
    let stream_future = async {
        let mut response_stream = response.bytes_stream();
        while let Some(item) = response_stream.next().await {
            match item {
                Ok(chunk) => {
                    if let Ok(mut chunk_str) = std::str::from_utf8(&chunk).map(|s| s.to_string()) {
                        // Normalize newlines to "\n"
                        chunk_str = chunk_str.replace("\r\n", "\n");
                        // Split by double newline to separate SSE events.
                        for event_block in chunk_str.split("\n\n") {
                            let event_block = event_block.trim();
                            if event_block.is_empty() {
                                continue;
                            }
                            // Process each line.
                            for line in event_block.lines() {
                                if line.starts_with("data:") {
                                    let data = line.trim_start_matches("data:").trim();
                                    if data == "[DONE]" {
                                        println!("Stream ended with [DONE].");
                                        return;
                                    }
                                    match serde_json::from_str::<serde_json::Value>(data) {
                                        Ok(delta_json) => {
                                            if let Some(delta) = delta_json.get("delta") {
                                                if let Some(content_arr) =
                                                    delta.get("content").and_then(|v| v.as_array())
                                                {
                                                    for part in content_arr {
                                                        if let Some(text_val) = part
                                                            .get("text")
                                                            .and_then(|t| t.get("value"))
                                                            .and_then(|s| s.as_str())
                                                        {
                                                            reply.push_str(text_val);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            println!(
                                                "Failed to parse SSE data: {}. Data: {}",
                                                e, data
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error reading stream chunk: {}", e);
                    break;
                }
            }
        }
    };

    // Wait for the stream to finish or time out.
    let _ = timeout(stream_timeout, stream_future).await;
    if reply.is_empty() {
        "No response".to_string()
    } else {
        reply
    }
}

/// Sends the reply to the Telegram Bot API.
async fn send_reply(chat_id: i64, text: String) -> Result<(), reqwest::Error> {
    println!("Sending reply to chat {}: {}", chat_id, text);
    let telegram_token =
        env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set in .env");
    let url = format!("https://api.telegram.org/bot{}/sendMessage", telegram_token);
    let client = reqwest::Client::new();
    let params = json!({
        "chat_id": chat_id,
        "text": text,
    });
    let response = client.post(&url).json(&params).send().await?;
    println!("Telegram API response: {:?}", response.text().await);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting application...");
    std::io::stdout().flush().unwrap();

    dotenv().ok();
    println!("Environment loaded.");
    std::io::stdout().flush().unwrap();

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid number");
    let addr = ([0, 0, 0, 0], port).into();
    println!("Binding server to address: http://{}", addr);
    std::io::stdout().flush().unwrap();

    let make_svc = make_service_fn(|_conn| async {
        println!("New connection established.");
        Ok::<_, Infallible>(service_fn(handle_request))
    });
    let server = Server::bind(&addr).serve(make_svc);

    println!("Webhook receiver listening on http://{}", addr);
    std::io::stdout().flush().unwrap();

    match server.await {
        Ok(_) => println!("Server ended gracefully."),
        Err(e) => eprintln!("Server error: {}", e),
    }
    Ok(())
}
