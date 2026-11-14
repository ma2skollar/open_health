use std::{sync::Arc, time::Duration};
use oura_ble::RingTransport;
use rand::RngCore;
use crate::{clock::Clock, delivery::UploadSink, storage::Outbox, CollectorError, Config};

pub struct Collector<T: RingTransport, S: UploadSink> {
    pub outbox: Outbox,
    transport: T,
    sink: S,
    config: Config,
    clock: Arc<dyn Clock>,
}
impl<T: RingTransport, S: UploadSink> Collector<T, S> {
    pub fn new(config: Config, transport: T, sink: S, clock: Arc<dyn Clock>) -> Result<Self, CollectorError> {
        let outbox = Outbox::open(&config)?;
        Ok(Self { outbox, transport, sink, config, clock })
    }
    pub async fn collect_once(&mut self) -> Result<(), CollectorError> {
        self.outbox.preflight_history()?;
        let now = self.clock.now_unix_ms();
        if now < 0 { return Err(CollectorError::Clock); }
        let mut heartbeat = self.outbox.heartbeat()?;
        heartbeat.ble_attempt_unix_ms = Some(now);
        heartbeat.clock_synchronized = self.clock.synchronized();
        heartbeat.disk_free_bytes = fs2::available_space(&self.config.state_dir).ok();
        heartbeat.pi_temperature_millicelsius = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok().and_then(|s| s.trim().parse().ok());
        self.outbox.save_heartbeat(&heartbeat)?;
        let timeout = Duration::from_secs(self.config.operation_timeout_secs);
        let cursor = self.outbox.cursor()?;
        let result = async {
            tokio::time::timeout(timeout, self.transport.connect()).await.map_err(|_| CollectorError::Timeout)??;
            let info = tokio::time::timeout(timeout, self.transport.device_info()).await.map_err(|_| CollectorError::Timeout)??;
            heartbeat.battery_percent = Some(tokio::time::timeout(timeout, self.transport.battery()).await.map_err(|_| CollectorError::Timeout)??);
            let history = tokio::time::timeout(timeout + Duration::from_secs(5), self.transport.drain_history(cursor)).await.map_err(|_| CollectorError::Timeout)??;
            self.outbox.commit_history(&history, &info, heartbeat, self.clock.now_unix_ms())?;
            Ok(())
        }.await;
        // History shutdown only disconnects. Never invoke live-HR/feature cleanup.
        let disconnected = tokio::time::timeout(timeout, self.transport.disconnect()).await;
        if matches!(&result, Err(CollectorError::Ring(oura_ble::RingError::CursorRegression))) {
            self.outbox.block_history()?;
        }
        match result {
            Err(error) => Err(error),
            Ok(()) => { disconnected.map_err(|_| CollectorError::Timeout)??; Ok(()) }
        }
    }
    pub async fn upload_once(&mut self) -> Result<bool, CollectorError> {
        let now = self.clock.now_unix_ms();
        if !self.clock.synchronized() || now < 0 { return Err(CollectorError::Clock); }
        let Some(pending) = self.outbox.next_pending(now)? else { return Ok(false); };
        let result = match tokio::time::timeout(Duration::from_secs(35), self.sink.upload(&pending)).await {
            Ok(result) => result,
            Err(_) => Err(CollectorError::Timeout),
        };
        let accepted = result.and_then(|ack| self.outbox.acknowledge(&ack, self.clock.now_unix_ms()));
        if let Err(error) = accepted {
            self.outbox.record_upload_failure(&pending.batch.batch_id, now, rand::rngs::OsRng.next_u64())?;
            return Err(error);
        }
        Ok(true)
    }
    fn report(&mut self, error: &CollectorError) -> Result<(), CollectorError> {
        let mut heartbeat = self.outbox.heartbeat()?;
        if heartbeat.error_codes.len() == 1 && heartbeat.error_codes[0] == error.code() {
            return Ok(());
        }
        heartbeat.error_codes = vec![error.code().into()];
        self.outbox.save_heartbeat(&heartbeat)?;
        eprintln!("{}", error.code());
        Ok(())
    }
    pub async fn tick(&mut self) -> Result<(), CollectorError> {
        let now = self.clock.now_unix_ms();
        self.outbox.observe_clock(now)?;
        // Upload first so pressure can recover without another ring attempt.
        if let Err(error) = self.upload_once().await { self.report(&error)?; }
        if self.outbox.collection_due(now)? {
            if let Err(error) = self.collect_once().await {
                self.outbox.record_collection_failure(now, rand::rngs::OsRng.next_u64())?;
                self.report(&error)?;
            }
        }
        let mut heartbeat = self.outbox.heartbeat()?;
        heartbeat.clock_synchronized = self.clock.synchronized();
        heartbeat.disk_free_bytes = fs2::available_space(&self.config.state_dir).ok();
        if let Err(error) = self.outbox.enqueue_heartbeat(heartbeat, now) { self.report(&error)?; }
        Ok(())
    }
    pub async fn disconnect(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.transport.disconnect()).await;
    }
}
