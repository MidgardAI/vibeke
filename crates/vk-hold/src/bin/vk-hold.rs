//! Standalone holder binary used by tests; the product binary dispatches `vibeke hold`.
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .map(std::path::PathBuf::from)
    };
    let spec = get("--spec").ok_or_else(|| anyhow::anyhow!("--spec required"))?;
    vk_hold::main_daemon(&spec, get("--log").as_deref())
}
