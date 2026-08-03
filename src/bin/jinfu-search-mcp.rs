use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = jinfu_search::mcp::JinfuSearchMcp::default()
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
