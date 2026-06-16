// SPDX-License-Identifier: AGPL-3.0-only
//! TLS-in-transit end-to-end: with a certificate configured, both transports
//! serve over TLS — gRPC through tonic's `tls-ring` and REST through
//! `axum-server` over the same rustls/`ring` stack. A self-signed certificate is
//! generated in-process with `rcgen` (also `ring`-backed), so the test needs no
//! external tooling.
//!
//! Integration-test helpers are not `#[test]` fns, so opt into the unwrap/expect
//! allowance explicitly (ADR-0017 scopes the ban to non-test code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use quiver_proto::v1::{self, quiver_client::QuiverClient};
use quiver_server::{Config, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

const TEST_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn auth_request<T>(key: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {key}").parse().expect("valid metadata"),
    );
    request
}

// Generate a self-signed certificate (ring-backed) valid for localhost.
fn self_signed() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    (cert.cert.pem(), cert.signing_key.serialize_pem())
}

// A CA plus a CA-signed server certificate (for localhost) and a CA-signed
// client certificate, for exercising mutual TLS. Returns
// (ca_pem, server_cert_pem, server_key_pem, client_cert_pem, client_key_pem).
fn ca_signed_chain() -> (String, String, String, String, String) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().unwrap();
    let server_params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

    let client_key = KeyPair::generate().unwrap();
    let client_params = CertificateParams::new(vec!["quiver-client".to_owned()]).unwrap();
    let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();

    (
        ca_cert.pem(),
        server_cert.pem(),
        server_key.serialize_pem(),
        client_cert.pem(),
        client_key.serialize_pem(),
    )
}

// Connect a TLS gRPC channel, retrying until the spawned server is ready.
async fn grpc_tls_channel(port: u16, ca_pem: &str) -> Channel {
    for _ in 0..200 {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca_pem))
            .domain_name("localhost");
        let endpoint = Channel::from_shared(format!("https://127.0.0.1:{port}"))
            .unwrap()
            .tls_config(tls)
            .unwrap();
        if let Ok(channel) = endpoint.connect().await {
            return channel;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gRPC TLS server did not become ready");
}

// Complete a TLS handshake to the REST port and return the raw HTTP response to
// `GET /healthz`, retrying until the server is ready.
async fn rest_tls_healthz(port: u16, ca_pem: &str) -> String {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from("localhost").unwrap();

    for _ in 0..200 {
        if let Ok(tcp) = TcpStream::connect(("127.0.0.1", port)).await
            && let Ok(mut tls) = connector.connect(server_name.clone(), tcp).await
        {
            tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut buf = Vec::new();
            tls.read_to_end(&mut buf).await.unwrap();
            return String::from_utf8_lossy(&buf).into_owned();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("REST TLS server did not become ready");
}

#[tokio::test]
async fn tls_secures_both_rest_and_grpc() {
    let tmp = tempfile::tempdir().unwrap();
    let (cert_pem, key_pem) = self_signed();
    let cert_path = tmp.path().join("cert.pem");
    let key_path = tmp.path().join("key.pem");
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, &key_pem).unwrap();

    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_port = rest_listener.local_addr().unwrap().port();
    let grpc_port = grpc_listener.local_addr().unwrap().port();

    let config = Config {
        data_dir: tmp.path().join("data"),
        rest_addr: rest_listener.local_addr().unwrap(),
        grpc_addr: grpc_listener.local_addr().unwrap(),
        api_keys: vec![TEST_KEY.into()],
        encryption_key: Some(TEST_KEY.to_owned()),
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        tls_client_ca: None,
        master_key_file: None,
        audit_log: None,
        leader_url: None,
        leader_api_key: None,
        insecure: false,
    };
    tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    // gRPC over TLS: an authenticated create + list round-trips.
    let channel = grpc_tls_channel(grpc_port, &cert_pem).await;
    let mut client = QuiverClient::new(channel);
    client
        .create_collection(auth_request(
            TEST_KEY,
            v1::CreateCollectionRequest {
                name: "secure".to_owned(),
                dim: 4,
                metric: v1::Metric::L2 as i32,
                index: v1::IndexKind::Unspecified as i32,
                pq_subspaces: None,
                filterable: Vec::new(),
                multivector: false,
                vector_encryption: v1::VectorEncryption::None as i32,
            },
        ))
        .await
        .expect("create over TLS");
    let listed = client
        .list_collections(auth_request(TEST_KEY, v1::ListCollectionsRequest {}))
        .await
        .expect("list over TLS")
        .into_inner();
    assert!(
        listed.collections.iter().any(|c| c.name == "secure"),
        "the collection created over TLS should be listed"
    );

    // REST over TLS: the handshake completes and /healthz responds 200.
    let response = rest_tls_healthz(rest_port, &cert_pem).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "REST should answer 200 over TLS, got: {:?}",
        response.lines().next()
    );

    // A plaintext (non-TLS) request to the TLS REST port must not succeed.
    let plaintext = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{rest_port}/healthz"))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    assert!(
        plaintext.is_err(),
        "plaintext HTTP must be refused on the TLS port"
    );
}

#[tokio::test]
async fn mtls_requires_a_client_certificate() {
    let tmp = tempfile::tempdir().unwrap();
    let (ca_pem, server_cert_pem, server_key_pem, client_cert_pem, client_key_pem) =
        ca_signed_chain();
    let cert_path = tmp.path().join("server-cert.pem");
    let key_path = tmp.path().join("server-key.pem");
    let ca_path = tmp.path().join("ca.pem");
    std::fs::write(&cert_path, &server_cert_pem).unwrap();
    std::fs::write(&key_path, &server_key_pem).unwrap();
    std::fs::write(&ca_path, &ca_pem).unwrap();

    let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_port = grpc_listener.local_addr().unwrap().port();

    // mTLS on: the server requires client certs chaining to `ca_path`.
    let config = Config {
        data_dir: tmp.path().join("data"),
        rest_addr: rest_listener.local_addr().unwrap(),
        grpc_addr: grpc_listener.local_addr().unwrap(),
        api_keys: vec![TEST_KEY.into()],
        encryption_key: Some(TEST_KEY.to_owned()),
        tls_cert: Some(cert_path),
        tls_key: Some(key_path),
        tls_client_ca: Some(ca_path),
        master_key_file: None,
        audit_log: None,
        leader_url: None,
        leader_api_key: None,
        insecure: false,
    };
    tokio::spawn(async move {
        let _ = serve(config, rest_listener, grpc_listener).await;
    });

    // A client presenting a CA-signed certificate completes the handshake and,
    // with a valid bearer key, is served.
    let identity = Identity::from_pem(client_cert_pem.as_bytes(), client_key_pem.as_bytes());
    let mut authed = None;
    for _ in 0..200 {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca_pem.clone()))
            .identity(identity.clone())
            .domain_name("localhost");
        if let Ok(endpoint) = Channel::from_shared(format!("https://127.0.0.1:{grpc_port}"))
            .unwrap()
            .tls_config(tls)
            && let Ok(channel) = endpoint.connect().await
        {
            authed = Some(QuiverClient::new(channel));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut authed = authed.expect("a client with a CA-signed certificate should connect");
    authed
        .list_collections(auth_request(TEST_KEY, v1::ListCollectionsRequest {}))
        .await
        .expect("an mTLS + bearer request should be served");

    // A client WITHOUT a certificate cannot perform an operation: mandatory
    // client auth fails the TLS handshake, which tonic surfaces either at
    // connect or on the first request. The server is already proven up above.
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem.clone()))
        .domain_name("localhost");
    let endpoint = Channel::from_shared(format!("https://127.0.0.1:{grpc_port}"))
        .unwrap()
        .tls_config(tls)
        .unwrap();
    let refused = match endpoint.connect().await {
        Err(_) => true,
        Ok(channel) => QuiverClient::new(channel)
            .list_collections(auth_request(TEST_KEY, v1::ListCollectionsRequest {}))
            .await
            .is_err(),
    };
    assert!(
        refused,
        "a client presenting no certificate must be refused by mTLS"
    );
}
