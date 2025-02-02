use dotenv::dotenv;
use futures_util::future::join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs::{create_dir_all, read_dir, read_to_string, File};
use std::io::Write;

// ---------------- Types for the Vector Store Summary ----------------

#[derive(Debug, Deserialize, Serialize)]
struct FileCounts {
    cancelled: u32,
    completed: u32,
    failed: u32,
    in_progress: u32,
    total: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct VectorStore {
    created_at: u64,
    // expires_after may be an object (or null)
    expires_after: Option<serde_json::Value>,
    // expires_at: integer or null.
    expires_at: Option<u64>,
    file_counts: FileCounts,
    id: String,
    // last_active_at: integer or null.
    last_active_at: Option<u64>,
    metadata: serde_json::Value,
    // Change name from String to Option<String> to allow null.
    name: Option<String>,
    object: String,
    // status may be null in practice.
    status: Option<String>,
    usage_bytes: u64,
}

// ---------------- Types for the Vector Store Files List ----------------

#[derive(Debug, Deserialize, Serialize)]
struct VectorStoreFile {
    id: String,
    object: String,
    created_at: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct VectorStoreFilesResponse {
    object: String,
    data: Vec<VectorStoreFile>,
    first_id: Option<String>,
    last_id: Option<String>,
    has_more: bool,
}

// ---------------- Type for File Details (from the Files API) ----------------
//
// Even though the documentation says these fields are strings,
// in practice some may come as null. We declare them as Option types.
#[derive(Debug, Deserialize, Serialize)]
struct FileDetail {
    id: String,
    object: String,
    created_at: u64,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    // Additional fields can be added here.
}

// ---------------- Bot Configuration Type ----------------

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

/// Retrieves detailed information for a given file ID and logs the full raw JSON.
async fn get_file_detail(
    client: Client,
    openai_token: String,
    file_id: String,
) -> Result<FileDetail, Box<dyn Error>> {
    let url = format!("https://api.openai.com/v1/files/{}", file_id);
    let response = client
        .get(&url)
        .bearer_auth(openai_token)
        .header("Content-Type", "application/json")
        .send()
        .await?;
    let status = response.status();
    let raw_bytes = response.bytes().await?;
    let raw_json = String::from_utf8_lossy(&raw_bytes);
    println!("Raw file detail for {}: {}", file_id, raw_json);

    // If the response contains an error object, skip it.
    if raw_json.contains("\"error\"") {
        eprintln!(
            "Skipping file {} because the response contains an error: {}",
            file_id, raw_json
        );
        return Err(format!("Error response for file {}", file_id).into());
    }

    if !status.is_success() {
        return Err(format!(
            "Failed to fetch file {}: {} - {}",
            file_id, status, raw_json
        )
        .into());
    }
    match serde_json::from_slice::<FileDetail>(&raw_bytes) {
        Ok(detail) => Ok(detail),
        Err(e) => {
            let error_msg = format!(
                "Error decoding FileDetail for file {}: {}. Raw response: {}",
                file_id, e, raw_json
            );
            eprintln!("{}", error_msg);
            Err(error_msg.into())
        }
    }
}

/// Processes one bot’s storage and logs every raw response.
async fn process_storage(
    config: BotConfig,
    openai_token: String,
    data_dir: String,
    client: Client,
) -> Result<(), Box<dyn Error>> {
    println!("Processing bot: {}", config.bot_name);

    // 1. Fetch vector store summary.
    let summary_url = format!(
        "https://api.openai.com/v1/vector_stores/{}",
        config.vector_store_id
    );
    let summary_response = client
        .get(&summary_url)
        .bearer_auth(openai_token.clone())
        .header("Content-Type", "application/json")
        .header("OpenAI-Beta", "assistants=v2")
        .send()
        .await?;
    let summary_status = summary_response.status();
    let summary_bytes = summary_response.bytes().await?;
    let raw_summary = String::from_utf8_lossy(&summary_bytes);
    println!(
        "Raw vector store summary for {}: {}",
        config.vector_store_id, raw_summary
    );
    if !summary_status.is_success() {
        eprintln!(
            "Failed to fetch vector store summary for {}. Status: {}. Response: {}",
            config.bot_name, summary_status, raw_summary
        );
        return Err("Summary fetch error".into());
    }
    let store_summary: VectorStore = serde_json::from_slice(&summary_bytes)?;
    let summary_filename = format!("{}/{}_summary.json", data_dir, config.vector_store_id);
    let mut summary_file = File::create(&summary_filename)?;
    summary_file.write_all(serde_json::to_string_pretty(&store_summary)?.as_bytes())?;
    println!("Saved vector store summary to {}", summary_filename);

    // 2. Fetch vector store files list.
    let files_url = format!(
        "https://api.openai.com/v1/vector_stores/{}/files",
        config.vector_store_id
    );
    let files_response = client
        .get(&files_url)
        .bearer_auth(openai_token.clone())
        .header("Content-Type", "application/json")
        .header("OpenAI-Beta", "assistants=v2")
        .send()
        .await?;
    let files_status = files_response.status();
    let files_bytes = files_response.bytes().await?;
    let raw_files = String::from_utf8_lossy(&files_bytes);
    println!(
        "Raw vector store files list for {}: {}",
        config.vector_store_id, raw_files
    );
    if !files_status.is_success() {
        eprintln!(
            "Failed to fetch vector store files for {}. Status: {}. Response: {}",
            config.bot_name, files_status, raw_files
        );
        return Err("Files list fetch error".into());
    }
    let files_data: VectorStoreFilesResponse = serde_json::from_slice(&files_bytes)?;

    // 3. For each file in the list, fetch detailed info concurrently.
    let mut detail_futures = Vec::new();
    for file in files_data.data.into_iter() {
        let client_clone = client.clone();
        let token_clone = openai_token.clone();
        detail_futures.push(get_file_detail(client_clone, token_clone, file.id));
    }
    let detail_results = join_all(detail_futures).await;
    let file_details: Vec<FileDetail> = detail_results.into_iter().filter_map(Result::ok).collect();

    let files_filename = format!("{}/{}_files_details.json", data_dir, config.vector_store_id);
    let mut files_file = File::create(&files_filename)?;
    files_file.write_all(serde_json::to_string_pretty(&file_details)?.as_bytes())?;
    println!("Saved vector store file details to {}", files_filename);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenv().ok();

    // Get environment variables.
    let openai_token = env::var("OPENAI_TOKEN").expect("OPENAI_TOKEN not set in .env");
    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    create_dir_all(&data_dir)?;

    // Load bot configurations.
    let bot_configs = load_bot_configs()?;
    if bot_configs.is_empty() {
        eprintln!("No bot configurations found in bot_config/");
        return Err("No bot configs".into());
    }

    // Create a reqwest client.
    let client = Client::new();

    // Process each bot concurrently.
    let tasks: Vec<_> = bot_configs
        .into_values()
        .map(|config| {
            let token_clone = openai_token.clone();
            let data_dir_clone = data_dir.clone();
            let client_clone = client.clone();
            process_storage(config, token_clone, data_dir_clone, client_clone)
        })
        .collect();

    let results = join_all(tasks).await;
    for res in results {
        if let Err(e) = res {
            eprintln!("Error processing storage: {}", e);
        }
    }

    println!("Finished processing all bot storages.");
    Ok(())
}
