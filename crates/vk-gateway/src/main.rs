//! `vibeke-gateway`: thin wrapper over [`vk_gateway::cli`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    vk_gateway::cli::run(std::env::args().skip(1)).await
}
