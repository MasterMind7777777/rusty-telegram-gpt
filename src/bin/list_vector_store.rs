use dotenv::dotenv;
use futures_util::future::join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::error::Error;
use std::fs::{create_dir_all, File};
use std::io::Write;

// --- Types for the vector store summary ---
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
    expires_after: Option<u64>,
    expires_at: Option<u64>,
    file_counts: FileCounts,
    id: String,
    last_active_at: u64,
    metadata: Value, // Arbitrary JSON.
    name: String,
    object: String,
    status: String,
    usage_bytes: u64,
}

// --- Types for the vector store files list ---
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

// --- Type for file details (from the Files API) ---
#[derive(Debug, Deserialize, Serialize)]
struct FileDetail {
    id: String,
    object: String,
    created_at: u64,
    filename: String,
    // Add additional fields if needed.
}

/// Retrieves detailed information for a given file ID.
async fn get_file_detail(
    client: &Client,
    openai_token: &str,
    file_id: &str,
) -> Result<FileDetail, Box<dyn Error>> {
    let url = format!("https://api.openai.com/v1/files/{}", file_id);
    let response = client
        .get(&url)
        .bearer_auth(openai_token)
        .header("Content-Type", "application/json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await?;
        return Err(format!("Failed to fetch file {}: {} - {}", file_id, status, body).into());
    }
    let detail: FileDetail = response.json().await?;
    Ok(detail)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load environment variables from the .env file.
    dotenv().ok();

    // Expect the vector store ID as the first command-line argument.
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <vector_store_id>", args[0]);
        std::process::exit(1);
    }
    let store_id = &args[1];

    // Retrieve the OpenAI API token.
    let openai_token = env::var("OPENAI_TOKEN").expect("OPENAI_TOKEN environment variable not set");

    // Retrieve the data directory from .env (or default to "./data")
    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    // Create the data directory if it doesn't exist.
    create_dir_all(&data_dir)?;

    let client = Client::new();

    // --- Fetch the Vector Store Summary ---
    let summary_url = format!("https://api.openai.com/v1/vector_stores/{}", store_id);
    let summary_response = client
        .get(&summary_url)
        .bearer_auth(&openai_token)
        .header("Content-Type", "application/json")
        .header("OpenAI-Beta", "assistants=v2")
        .send()
        .await?;
    let summary_status = summary_response.status();
    if !summary_status.is_success() {
        eprintln!(
            "Failed to fetch vector store summary. Status: {}",
            summary_status
        );
        let body = summary_response.text().await?;
        eprintln!("Response: {}", body);
        std::process::exit(1);
    }
    let store_summary: VectorStore = summary_response.json().await?;

    // Save the summary to a file in the data directory.
    let summary_filename = format!("{}/{}_summary.json", data_dir, store_id);
    let mut summary_file = File::create(&summary_filename)?;
    summary_file.write_all(serde_json::to_string_pretty(&store_summary)?.as_bytes())?;
    println!("Saved vector store summary to {}", summary_filename);

    // --- Fetch the Vector Store Files List ---
    let files_url = format!("https://api.openai.com/v1/vector_stores/{}/files", store_id);
    let files_response = client
        .get(&files_url)
        .bearer_auth(&openai_token)
        .header("Content-Type", "application/json")
        .header("OpenAI-Beta", "assistants=v2")
        .send()
        .await?;
    let files_status = files_response.status();
    if !files_status.is_success() {
        eprintln!(
            "Failed to fetch vector store files. Status: {}",
            files_status
        );
        let body = files_response.text().await?;
        eprintln!("Response: {}", body);
        std::process::exit(1);
    }
    let files_data: VectorStoreFilesResponse = files_response.json().await?;

    // --- Retrieve Detailed Info for Each File (to get the filename) ---
    let file_detail_futures = files_data
        .data
        .iter()
        .map(|file| get_file_detail(&client, &openai_token, &file.id));
    let file_details_results = join_all(file_detail_futures).await;
    let file_details: Vec<FileDetail> = file_details_results
        .into_iter()
        .filter_map(Result::ok)
        .collect();

    // Save the file details to a file in the data directory.
    let files_filename = format!("{}/{}_files_details.json", data_dir, store_id);
    let mut files_file = File::create(&files_filename)?;
    files_file.write_all(serde_json::to_string_pretty(&file_details)?.as_bytes())?;
    println!("Saved vector store file details to {}", files_filename);

    Ok(())
}
