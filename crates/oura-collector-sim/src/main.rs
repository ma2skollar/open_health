use oura_collector_sim::{reset_demo, run_scenario, SCENARIOS};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::pause();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [] => {
            for scenario in SCENARIOS {
                print!("{}", run_scenario(scenario).await?);
            }
        }
        [command] if command == "demo" => {
            for scenario in SCENARIOS {
                print!("{}", run_scenario(scenario).await?);
            }
        }
        [command] if command == "reset-demo" => print!("{}", reset_demo()?),
        [command, scenario] if command == "scenario" => {
            print!("{}", run_scenario(scenario).await?);
        }
        _ => return Err("usage: oura-collector-sim [demo | reset-demo | scenario NAME]".into()),
    }
    Ok(())
}
