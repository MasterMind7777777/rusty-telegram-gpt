// src/main.rs

use dotenv::dotenv;
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::Body;
use hyper::{Method, Request, Response, Server, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::env;

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
    let bytes = to_bytes(body).await?;
    Ok(bytes.to_vec())
}

async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    // Only handle POST requests to "/webhook"
    if req.method() == Method::POST && req.uri().path() == "/webhook" {
        let whole_body = match body_to_bytes(req.into_body()).await {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("Error reading body: {}", err);
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from("Bad Request"))
                    .unwrap());
            }
        };

        let update: Update = match serde_json::from_slice(&whole_body) {
            Ok(upd) => upd,
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
                // Reply to the user via Telegram.
                let _ = send_reply(chat_id, openai_response).await;
            }
        }
        Ok(Response::new(Body::from("OK")))
    } else {
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap())
    }
}

async fn call_openai_api(prompt: String) -> String {
    let openai_token = env::var("OPENAI_TOKEN").expect("OPENAI_TOKEN not set in .env");
    let client = reqwest::Client::new();
    let url = "https://api.openai.com/v1/assistants";
    let params = json!({
        "prompt": prompt,
    });

    match client
        .post(url)
        .bearer_auth(openai_token)
        .json(&params)
        .send()
        .await
    {
        Ok(response) => match response.json::<serde_json::Value>().await {
            Ok(json_resp) => json_resp["response"]
                .as_str()
                .unwrap_or("No response")
                .to_string(),
            Err(err) => {
                eprintln!("Error parsing OpenAI response: {}", err);
                "Error parsing response".to_string()
            }
        },
        Err(err) => {
            eprintln!("Error calling OpenAI API: {}", err);
            "Error calling OpenAI API".to_string()
        }
    }
}

async fn send_reply(chat_id: i64, text: String) -> Result<(), reqwest::Error> {
    let telegram_token =
        env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set in .env");
    let url = format!("https://api.telegram.org/bot{}/sendMessage", telegram_token);
    let client = reqwest::Client::new();
    let params = json!({
        "chat_id": chat_id,
        "text": text,
    });
    client.post(url).json(&params).send().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    // Read the port from .env (or default to 8080 if not set)
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid number");
    let addr = ([0, 0, 0, 0], port).into();

    let make_svc = make_service_fn(|_conn| async {
        // For each connection, we create a service to handle requests.
        Ok::<_, Infallible>(service_fn(handle_request))
    });
    let server = Server::bind(&addr).serve(make_svc);

    println!("Webhook receiver listening on http://{}", addr);
    server.await?;
    Ok(())
}
