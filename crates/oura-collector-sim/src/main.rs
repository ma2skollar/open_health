#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => println!("Usage: oura-collector-sim demo | scenario NAME\nScenarios: {}", oura_collector_sim::SCENARIOS.join(", ")),
        [command] if command == "--help" => println!("Usage: oura-collector-sim demo | scenario NAME"),
        [command] if command == "demo" => for name in oura_collector_sim::SCENARIOS { println!("{}", oura_collector_sim::run_scenario(name).await?); },
        [command, name] if command == "scenario" => println!("{}", oura_collector_sim::run_scenario(name).await?),
        _ => return Err("invalid simulator arguments".into()),
    }
    Ok(())
}
