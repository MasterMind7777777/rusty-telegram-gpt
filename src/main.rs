// src/main.rs

use dotenv::dotenv;
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::env;
use std::io::{self, Write};

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

/// Helper function to collect the body into a Vec<u8>
async fn body_to_bytes(body: Body) -> Result<Vec<u8>, hyper::Error> {
    println!("Converting request body to bytes...");
    let bytes = to_bytes(body).await?;
    println!("Finished converting body ({} bytes)", bytes.len());
    Ok(bytes.to_vec())
}

async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let raw_path = req.uri().path();
    println!("Incoming request: {} {}", req.method(), raw_path);

    // Normalize path by trimming both leading and trailing slashes.
    let normalized_path = raw_path.trim_matches('/');
    println!("Normalized path: {}", normalized_path);

    // Only handle POST requests to the "webhook" endpoint.
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

                // Call OpenAI API with the message text.
                let openai_response = call_openai_api(text).await;
                println!("OpenAI response: {}", openai_response);

                // Reply to the user via Telegram.
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

async fn call_openai_api(prompt: String) -> String {
    println!("Calling OpenAI API with prompt: {}", prompt);
    let openai_token = env::var("OPENAI_TOKEN").expect("OPENAI_TOKEN not set in .env");
    let client = reqwest::Client::new();
    let assistant_id = "asst_a4UKVFs40KuWDofTyc3aqblf"; // DAPConsult's assistant ID

    // --- Step 1: Create thread and run in one request ---
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
        }
    });

    let run_resp = client
        .post(run_url)
        .header("OpenAI-Beta", "assistants=v2")
        .bearer_auth(&openai_token)
        .json(&run_payload)
        .send()
        .await;

    let thread_id = match run_resp {
        Ok(resp) => {
            let json_resp = resp.json::<serde_json::Value>().await.unwrap();
            println!("Received run object: {:?}", json_resp);
            // The run object contains a "thread_id" field.
            json_resp
                .get("thread_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        }
        Err(err) => {
            eprintln!("Error creating thread run: {}", err);
            return "Error creating thread run".to_string();
        }
    };

    if thread_id.is_empty() {
        return "Failed to create thread run".to_string();
    }

    // --- Step 2: List messages in the thread to get the assistant's reply ---
    let messages_url = format!("https://api.openai.com/v1/threads/{}/messages", thread_id);
    let messages_resp = client
        .get(&messages_url)
        .header("OpenAI-Beta", "assistants=v2")
        .bearer_auth(&openai_token)
        .send()
        .await;

    match messages_resp {
        Ok(resp) => {
            let json_resp = resp.json::<serde_json::Value>().await.unwrap();
            println!("Received messages list: {:?}", json_resp);
            // The response should have a "data" field containing an array of messages.
            if let Some(data) = json_resp.get("data").and_then(|v| v.as_array()) {
                // Look for the first message where role is "assistant".
                if let Some(assistant_msg) = data
                    .iter()
                    .find(|msg| msg.get("role").and_then(|v| v.as_str()) == Some("assistant"))
                {
                    // The message's content is an array of parts.
                    if let Some(content_arr) =
                        assistant_msg.get("content").and_then(|v| v.as_array())
                    {
                        // Join all text parts.
                        let reply: String = content_arr
                            .iter()
                            .filter_map(|part| {
                                part.get("text")
                                    .and_then(|t| t.get("value"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            })
                            .collect::<Vec<String>>()
                            .join(" ");
                        if !reply.is_empty() {
                            return reply;
                        }
                    }
                }
            }
            "No response".to_string()
        }
        Err(err) => {
            eprintln!("Error listing messages: {}", err);
            "Error listing messages".to_string()
        }
    }
}

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
    io::stdout().flush().unwrap();

    dotenv().ok();
    println!("Environment loaded.");
    io::stdout().flush().unwrap();

    // Read the port from .env (or default to 8080 if not set)
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid number");
    let addr = ([0, 0, 0, 0], port).into();
    println!("Binding server to address: http://{}", addr);
    io::stdout().flush().unwrap();

    let make_svc = make_service_fn(|_conn| async {
        println!("New connection established.");
        // For each connection, we create a service to handle requests.
        Ok::<_, Infallible>(service_fn(handle_request))
    });
    let server = Server::bind(&addr).serve(make_svc);

    println!("Webhook receiver listening on http://{}", addr);
    io::stdout().flush().unwrap();

    // Await the server future.
    match server.await {
        Ok(_) => println!("Server ended gracefully."),
        Err(e) => eprintln!("Server error: {}", e),
    }
    Ok(())
}
