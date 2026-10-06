//! `vibeke-relay`: thin wrapper over [`vk_relay::cli`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    vk_relay::cli::run(std::env::args().skip(1)).await
}
