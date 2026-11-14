#[tokio::test]
async fn synthetic_raw_delivery_acceptance() {
    for scenario in oura_collector_sim::SCENARIOS {
        let result = oura_collector_sim::run_scenario(scenario).await.unwrap();
        assert_eq!(result, format!("synthetic: {scenario} passed\n"));
    }
}
