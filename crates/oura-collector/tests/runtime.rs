mod common;
use common::*;
use std::{sync::Arc, time::Duration};
use oura_collector::{CollectorError, FakeClock, Outbox, delivery::{HttpUploadSink, UploadSink, sign_attempt}};

#[test]
fn signing_retries_rotate_nonce_key_and_gate_clock() {
    let (_dir, mut config)=fixture();
    let mut store=Outbox::open(&config).unwrap();
    append(&mut store,1);
    let pending=store.next_pending(i64::MAX).unwrap().unwrap();
    let clock=FakeClock::new(1_700_000_000_000);
    let first=sign_attempt(&config,&pending,&clock,[1;16]).unwrap();
    clock.advance(Duration::from_secs(1));
    let second=sign_attempt(&config,&pending,&clock,[2;16]).unwrap();
    assert_ne!(first.fields.nonce,second.fields.nonce);
    assert_ne!(first.fields.timestamp,second.fields.timestamp);
    assert_ne!(first.signature,second.signature);
    assert_eq!(first.fields.body_sha256,second.fields.body_sha256);
    first.fields.verify(&[0x42;32],&first.signature,first.fields.timestamp).unwrap();
    std::fs::write(&config.signing_key_file,[0x24;32]).unwrap();
    config.key_id="rotated-key".into();
    let rotated=sign_attempt(&config,&pending,&clock,[2;16]).unwrap();
    assert_eq!(rotated.fields.key_id,"rotated-key");
    rotated.fields.verify(&[0x24;32],&rotated.signature,rotated.fields.timestamp).unwrap();
    clock.set_synchronized(false);
    assert!(matches!(sign_attempt(&config,&pending,&clock,[3;16]),Err(CollectorError::Clock)));
    assert_eq!(store.stats().unwrap().batches,1);
}
#[test]
fn key_file_modes_symlinks_lengths_and_side_effect_free_validation() {
    use std::os::unix::fs::{PermissionsExt,symlink};
    let (dir,config)=fixture();
    let original=config.signing_key_file.clone();
    assert!(oura_collector::config::load_signing_key(&original).is_ok());
    std::fs::set_permissions(&original,std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(oura_collector::config::load_signing_key(&original),Err(CollectorError::Permissions)));
    std::fs::set_permissions(&original,std::fs::Permissions::from_mode(0o600)).unwrap();
    let link=dir.path().join("link"); symlink(&original,&link).unwrap();
    assert!(oura_collector::config::load_signing_key(&link).is_err());
    std::fs::write(&original,[0u8;31]).unwrap();
    assert!(matches!(oura_collector::config::load_signing_key(&original),Err(CollectorError::Key)));
    std::fs::remove_file(original).unwrap();
    config.validate().unwrap();
    assert!(!config.state_dir.exists());
    for url in ["http://localhost/ingest/v1/batches","https://u:p@localhost/ingest/v1/batches","https://localhost/ingest/v1/batches?q=1","https://localhost/ingest/v1/batches/","https://localhost/elsewhere"] {
        let mut bad=config.clone(); bad.ingest_url=url.into(); assert!(bad.validate().is_err());
    }
}

#[tokio::test]
async fn wrong_certificate_and_hostname_never_delete_outbox() {
    use tokio_rustls::{TlsAcceptor, rustls::{self,pki_types::{CertificateDer,PrivatePkcs8KeyDer}}};
    let (dir,mut config)=fixture();
    let mut store=Outbox::open(&config).unwrap(); append(&mut store,1);
    let pending=store.next_pending(i64::MAX).unwrap().unwrap();
    let certified=rcgen::generate_simple_self_signed(vec!["wrong-host.invalid".into()]).unwrap();
    let der=certified.cert.der().clone();
    let key=PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
    let provider=rustls::crypto::ring::default_provider();
    let server=rustls::ServerConfig::builder_with_provider(Arc::new(provider)).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(vec![CertificateDer::from(der)],key.into()).unwrap();
    let acceptor=TlsAcceptor::from(Arc::new(server));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.ingest_url=format!("https://localhost:{}/ingest/v1/batches",listener.local_addr().unwrap().port());
    let service=tokio::spawn(async move {
        for _ in 0..2 { let (stream,_)=listener.accept().await.unwrap(); let _=acceptor.accept(stream).await; }
    });
    let clock=Arc::new(FakeClock::new(1_700_000_000_000));
    let mut sink=HttpUploadSink::new(&config,clock.clone()).unwrap();
    assert!(sink.upload(&pending).await.is_err());
    let ca=dir.path().join("synthetic-ca.pem"); std::fs::write(&ca,certified.cert.pem()).unwrap();
    config.ca_certificate_file=Some(ca);
    let mut trusted_wrong_name=HttpUploadSink::new(&config,clock).unwrap();
    assert!(trusted_wrong_name.upload(&pending).await.is_err());
    tokio::time::timeout(Duration::from_secs(10),service).await.unwrap().unwrap();
    assert_eq!(store.stats().unwrap().batches,1);
    assert_eq!(store.receipt_count().unwrap(),0);
}
