mod common;
use common::*;
use std::sync::Arc;
use oura_collector::{Outbox,FakeClock,delivery::{HttpUploadSink,UploadSink}};
use oura_protocol_v1::ingest::{self,SigningFields};
use tokio::io::{AsyncReadExt,AsyncWriteExt};

#[tokio::test]
async fn https_uncertain_ack_retries_exact_body_with_fresh_nonce() {
    use tokio_rustls::{TlsAcceptor,rustls::{self,pki_types::PrivatePkcs8KeyDer}};
    let (dir,mut config)=fixture();
    let mut store=Outbox::open(&config).unwrap(); append(&mut store,1);
    let pending=store.next_pending(i64::MAX).unwrap().unwrap();
    let frozen=pending.body.clone();
    let receipt=serde_json::to_vec(&ack(&pending)).unwrap();
    let cert=rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let ca=dir.path().join("synthetic-ca.pem"); std::fs::write(&ca,cert.cert.pem()).unwrap();
    config.ca_certificate_file=Some(ca);
    let server=rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()],PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into()).unwrap();
    let acceptor=TlsAcceptor::from(Arc::new(server));
    let listener=tokio::net::TcpListener::bind(("localhost",0)).await.unwrap();
    config.ingest_url=format!("https://localhost:{}/ingest/v1/batches",listener.local_addr().unwrap().port());
    let service=tokio::spawn(async move {
        let mut nonces=Vec::new();
        for attempt in 0..2 {
            let (stream,_)=listener.accept().await.unwrap();
            let mut stream=acceptor.accept(stream).await.unwrap();
            let mut request=Vec::new();
            let header_end=loop {
                let mut chunk=[0u8;1024]; let n=stream.read(&mut chunk).await.unwrap(); assert!(n>0);
                request.extend_from_slice(&chunk[..n]); assert!(request.len()<64*1024);
                if let Some(start)=request.windows(4).position(|s|s==b"\r\n\r\n") {break start+4;}
            };
            let headers=String::from_utf8(request[..header_end].to_vec()).unwrap();
            assert_eq!(headers.lines().next().unwrap(),"POST /ingest/v1/batches HTTP/1.1");
            let header=|name:&str| headers.lines().skip(1).filter_map(|line|line.split_once(':')).find(|(key,_)|key.eq_ignore_ascii_case(name)).unwrap().1.trim().to_string();
            let length=header("content-length").parse::<usize>().unwrap();
            assert!(length<=ingest::MAX_BODY_BYTES);
            while request.len()-header_end<length {
                let mut chunk=[0u8;4096]; let n=stream.read(&mut chunk).await.unwrap(); assert!(n>0); request.extend_from_slice(&chunk[..n]);
            }
            let body=&request[header_end..header_end+length];
            assert_eq!(body,frozen.as_ref());
            assert_eq!(header("content-type"),ingest::CONTENT_TYPE);
            assert!(!headers.to_ascii_lowercase().contains("authorization:"));
            assert!(!headers.to_ascii_lowercase().contains("content-encoding:"));
            let fields=SigningFields { key_id:header(ingest::HEADER_KEY_ID),device_id:header(ingest::HEADER_DEVICE_ID),timestamp:header(ingest::HEADER_TIMESTAMP).parse().unwrap(),nonce:header(ingest::HEADER_NONCE),body_sha256:header(ingest::HEADER_BODY_SHA256) };
            assert_eq!(fields.body_sha256,ingest::body_hash(body));
            fields.verify(&[0x42;32],&header(ingest::HEADER_SIGNATURE),1_700_000_000).unwrap();
            nonces.push(fields.nonce);
            if attempt==0 {continue;} // Receipt committed, response lost.
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",receipt.len());
            stream.write_all(response.as_bytes()).await.unwrap(); stream.write_all(&receipt).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        nonces
    });
    let clock=Arc::new(FakeClock::new(1_700_000_000_000));
    let mut sink=HttpUploadSink::new(&config,clock.clone()).unwrap();
    assert!(sink.upload(&pending).await.is_err());
    assert_eq!(store.stats().unwrap().batches,1);
    drop(store);
    let mut reopened=Outbox::open(&config).unwrap();
    let retried=reopened.next_pending(i64::MAX).unwrap().unwrap();
    let accepted=sink.upload(&retried).await.unwrap(); reopened.acknowledge(&accepted,1_700_000_000_001).unwrap();
    assert_eq!(reopened.stats().unwrap().batches,0);
    assert_eq!(reopened.receipt_count().unwrap(),1);
    let nonces=tokio::time::timeout(std::time::Duration::from_secs(10),service).await.unwrap().unwrap();
    assert_ne!(nonces[0],nonces[1]);
}
