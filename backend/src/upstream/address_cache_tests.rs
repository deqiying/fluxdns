//! 配置绑定 resolver 的时间、查填、取消与换代契约。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use hickory_proto::{
    op::ResponseCode,
    rr::{RData, Record, RecordType, rdata::A},
};

use crate::dns::{CanonicalQuery, CanonicalResponse, RequestContext};
use crate::ports::exchange::{ConnectorId, DnsExchange, UpstreamOutcome};
use crate::ports::testing::FakeClock;

use super::*;

struct BootstrapProbe {
    id: ConnectorId,
    calls: AtomicU32,
    ttl: AtomicU32,
    ip: AtomicU32,
    blocked: AtomicBool,
    failed: AtomicBool,
    gate: Semaphore,
    aaaa_elapsed: Mutex<Duration>,
    clock: Arc<FakeClock>,
}

impl DnsExchange for BootstrapProbe {
    fn connector_id(&self) -> &ConnectorId {
        &self.id
    }

    fn exchange<'a>(
        &'a self,
        query: &'a CanonicalQuery,
        _context: &'a RequestContext,
    ) -> PortFuture<'a, UpstreamOutcome> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.blocked.load(Ordering::SeqCst) {
                self.gate.acquire().await.unwrap().forget();
            }
            let answers = if query.question().query_type() == RecordType::A {
                vec![Record::from_rdata(
                    query.question().name().clone(),
                    self.ttl.load(Ordering::SeqCst),
                    RData::A(A(self.ip.load(Ordering::SeqCst).into())),
                )]
            } else {
                self.clock.advance(*self.aaaa_elapsed.lock().unwrap());
                Vec::new()
            };
            let response = if self.failed.load(Ordering::SeqCst) {
                CanonicalResponse::empty_response(query, ResponseCode::ServFail).unwrap()
            } else {
                CanonicalResponse::response_with_code(query, ResponseCode::NoError, answers)
                    .unwrap()
            };
            UpstreamOutcome::Response(response)
        })
    }
}

fn upstream() -> ResolvedUpstream {
    ResolvedUpstream::Doh {
        id: ConfigId::new("target").unwrap(),
        address: "https://resolver.example.test:8443/dns-query"
            .parse()
            .unwrap(),
        bootstrap: Some(ConfigId::new("bootstrap").unwrap()),
        connect_ip: None,
        proxy: None,
        edns_client_subnet: None,
    }
}

fn fixture(ttl: u32) -> (TokioDohAddressResolver, Arc<BootstrapProbe>, Arc<FakeClock>) {
    fixture_for(ttl, &upstream())
}

fn fixture_for(
    ttl: u32,
    upstream: &ResolvedUpstream,
) -> (TokioDohAddressResolver, Arc<BootstrapProbe>, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::default());
    let probe = Arc::new(BootstrapProbe {
        id: ConnectorId::new("bootstrap").unwrap(),
        calls: AtomicU32::new(0),
        ttl: AtomicU32::new(ttl),
        ip: AtomicU32::new(u32::from(std::net::Ipv4Addr::LOCALHOST)),
        blocked: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        gate: Semaphore::new(0),
        aaaa_elapsed: Mutex::new(Duration::ZERO),
        clock: clock.clone(),
    });
    let registry = Arc::new(BootstrapConnectorRegistry::default());
    registry.insert(ConfigId::new("bootstrap").unwrap(), probe.clone());
    let mut resolver = TokioDohAddressResolver::for_upstream(upstream, registry).unwrap();
    resolver.clock = clock.clone();
    (resolver, probe, clock)
}

fn address_request() -> DohAddressRequest {
    DohAddressRequest::new(
        "resolver.example.test",
        8443,
        Some(ConfigId::new("bootstrap").unwrap()),
    )
}

async fn lookup(resolver: &TokioDohAddressResolver) -> Result<Vec<SocketAddr>, PortError> {
    resolver
        .resolve(
            address_request(),
            Deadline::new(resolver.clock.monotonic_now() + Duration::from_secs(10)),
            &Cancellation::new(),
        )
        .await
}

async fn wait_for_calls(probe: &BootstrapProbe, expected: u32) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while probe.calls.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bound_cache_honors_short_zero_and_capped_ttl() {
    for (ttl, elapsed) in [(1, 1), (7200, 3600)] {
        let (resolver, probe, clock) = fixture(ttl);
        assert_eq!(
            lookup(&resolver).await.unwrap(),
            vec!["127.0.0.1:8443".parse().unwrap()]
        );
        lookup(&resolver).await.unwrap();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        clock.advance(Duration::from_secs(elapsed));
        probe.ip.store(
            u32::from(std::net::Ipv4Addr::new(127, 0, 0, 2)),
            Ordering::SeqCst,
        );
        assert_eq!(
            lookup(&resolver).await.unwrap(),
            vec!["127.0.0.2:8443".parse().unwrap()]
        );
        assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
    }
    let (resolver, probe, _) = fixture(0);
    lookup(&resolver).await.unwrap();
    lookup(&resolver).await.unwrap();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        resolver
            .binding
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .cached_entry_count(),
        0
    );
}

#[tokio::test]
async fn cache_hits_still_honor_cancellation_and_deadline() {
    let (resolver, probe, clock) = fixture(60);
    lookup(&resolver).await.unwrap();
    let cancelled = Cancellation::new();
    cancelled.cancel(CancelReason::ClientDisconnected);
    let error = resolver
        .resolve(
            address_request(),
            Deadline::new(clock.monotonic_now() + Duration::from_secs(5)),
            &cancelled,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.class(),
        PortErrorClass::Cancelled(CancelReason::ClientDisconnected)
    ));
    let error = resolver
        .resolve(
            address_request(),
            Deadline::new(clock.monotonic_now()),
            &Cancellation::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::Timeout));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn http_adapters_use_the_refreshed_address_and_keep_the_original_host() {
    use tokio::net::TcpListener;
    use tokio_rustls::{TlsAcceptor, rustls};

    async fn reply<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        mut stream: S,
        body: u8,
    ) {
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await.unwrap());
            assert!(headers.len() < 4096);
        }
        let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
        assert!(headers.contains("host: resolver.example.test:"));
        assert!(headers.contains("post /dns-query http/1.1"));
        assert_eq!(stream.read_u8().await.unwrap(), 42);
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 1\r\nConnection: close\r\n\r\n").await.unwrap();
        stream.write_all(&[body]).await.unwrap();
    }

    async fn serve(listener: TcpListener, count: usize, body: u8, tls: Option<TlsAcceptor>) {
        for _ in 0..count {
            let (stream, _) = listener.accept().await.unwrap();
            if let Some(tls) = &tls {
                let stream = tls.accept(stream).await.unwrap();
                assert_eq!(
                    stream.get_ref().1.server_name(),
                    Some("resolver.example.test")
                );
                reply(stream, body).await;
            } else {
                reply(stream, body).await;
            }
        }
    }

    crate::ensure_rustls_crypto_provider();
    for mode in ["tokio", "reqwest", "https"] {
        let (tls, certificate) = if mode == "https" {
            let certified =
                rcgen::generate_simple_self_signed(vec!["resolver.example.test".to_owned()])
                    .unwrap();
            let der = certified.cert.der().to_vec();
            let mut config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![rustls::pki_types::CertificateDer::from(der.clone())],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(
                        certified.signing_key.serialize_der(),
                    )
                    .into(),
                )
                .unwrap();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            (Some(TlsAcceptor::from(Arc::new(config))), Some(der))
        } else {
            (None, None)
        };
        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = first.local_addr().unwrap().port();
        let second = TcpListener::bind(("127.0.0.2", port)).await.unwrap();
        let first_server = tokio::spawn(serve(first, 2, 1, tls.clone()));
        let second_server = tokio::spawn(serve(second, 1, 2, tls));
        let scheme = if mode == "https" { "https" } else { "http" };
        let endpoint: Url = format!("{scheme}://resolver.example.test:{port}/dns-query")
            .parse()
            .unwrap();
        let mut upstream = upstream();
        if let ResolvedUpstream::Doh { address, .. } = &mut upstream {
            *address = endpoint.clone();
        }
        let (resolver, probe, clock) = fixture_for(1, &upstream);
        let resolver = Arc::new(resolver);
        let transport: Box<dyn DohHttpTransport> = match mode {
            "https" => Box::new(
                crate::upstream::ReqwestDohHttpTransport::with_test_root_certificate(
                    resolver,
                    certificate.as_ref().unwrap(),
                )
                .unwrap(),
            ),
            "reqwest" => Box::new(crate::upstream::ReqwestDohHttpTransport::new(resolver).unwrap()),
            _ => Box::new(TokioDohHttpTransport::with_resolver(resolver)),
        };
        for expected in [1, 1, 2] {
            if expected == 2 {
                probe.ip.store(
                    u32::from(std::net::Ipv4Addr::new(127, 0, 0, 2)),
                    Ordering::SeqCst,
                );
                clock.advance(Duration::from_secs(1));
            }
            let response = transport
                .post(
                    DohHttpRequest::new_with_bootstrap(
                        endpoint.clone(),
                        None,
                        Some(ConfigId::new("bootstrap").unwrap()),
                        vec![42],
                    ),
                    Deadline::new(Instant::now() + Duration::from_secs(10)),
                    &Cancellation::new(),
                )
                .await
                .unwrap();
            assert_eq!(response.body, vec![expected]);
            assert_eq!(
                probe.calls.load(Ordering::SeqCst),
                if expected == 2 { 4 } else { 2 }
            );
        }
        first_server.await.unwrap();
        second_server.await.unwrap();
    }
}

#[tokio::test]
async fn socks5_receives_refreshed_target_addresses() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = listener.local_addr().unwrap();
    let root = std::path::PathBuf::from(crate::config::test_support::absolute_path(
        "bootstrap-cache-socks5",
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("proxy-url");
    std::fs::write(&path, format!("socks5://{proxy_address}")).unwrap();
    let profile = OutboundProfile::from_resolved(
        &crate::config::resolve::ResolvedOutbound {
            id: ConfigId::new("proxy").unwrap(),
            kind: crate::config::model::OutboundType::Socks5,
            proxy_url: crate::config::resolve::ResolvedSecretRef {
                env: None,
                file: Some(path),
            },
        },
        1024,
    )
    .unwrap();
    let server = tokio::spawn(async move {
        for last_octet in [1, 1, 2] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await.unwrap();
            let mut target = [0; 10];
            stream.read_exact(&mut target).await.unwrap();
            assert_eq!(target, [5, 1, 0, 1, 127, 0, 0, last_octet, 32, 251]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(stream.read_u8().await.unwrap());
                assert!(headers.len() < 4096);
            }
            assert!(
                String::from_utf8(headers)
                    .unwrap()
                    .contains("resolver.example.test:8443")
            );
            assert_eq!(stream.read_u8().await.unwrap(), 42);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 1\r\nConnection: close\r\n\r\n").await.unwrap();
            stream.write_all(&[last_octet]).await.unwrap();
        }
    });
    let (resolver, probe, clock) = fixture(1);
    let transport = TokioSocks5DohHttpTransport::new(
        profile,
        Arc::new(crate::upstream::TokioOutboundDialer::new()),
        Arc::new(crate::upstream::TokioOutboundAddressResolver::new()),
        Arc::new(resolver),
    );
    for expected in [1, 1, 2] {
        if expected == 2 {
            probe.ip.store(
                u32::from(std::net::Ipv4Addr::new(127, 0, 0, 2)),
                Ordering::SeqCst,
            );
            clock.advance(Duration::from_secs(1));
        }
        let response = transport
            .post(
                DohHttpRequest::new_with_bootstrap(
                    "http://resolver.example.test:8443/dns-query"
                        .parse()
                        .unwrap(),
                    None,
                    Some(ConfigId::new("bootstrap").unwrap()),
                    vec![42],
                ),
                Deadline::new(Instant::now() + Duration::from_secs(5)),
                &Cancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.body, vec![expected]);
    }
    assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
    server.await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn waiting_for_aaaa_and_filling_state_never_renews_the_a_answer() {
    let (resolver, probe, clock) = fixture(2);
    *probe.aaaa_elapsed.lock().unwrap() = Duration::from_secs(1);
    lookup(&resolver).await.unwrap();
    clock.advance(Duration::from_secs(1));
    lookup(&resolver).await.unwrap();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 4);

    let (resolver, probe, _) = fixture(1);
    *probe.aaaa_elapsed.lock().unwrap() = Duration::from_secs(2);
    lookup(&resolver).await.unwrap();
    assert_eq!(
        resolver
            .binding
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .cached_entry_count(),
        0
    );
}

#[tokio::test]
async fn identity_mismatch_and_expired_failure_never_use_system_fallback() {
    let (resolver, probe, clock) = fixture(1);
    let wrong = DohAddressRequest::new("other.example.test", 8443, address_request().bootstrap);
    let error = resolver
        .resolve(
            wrong,
            Deadline::new(clock.monotonic_now() + Duration::from_secs(10)),
            &Cancellation::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error.class(), PortErrorClass::InvalidInput));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    lookup(&resolver).await.unwrap();
    probe.failed.store(true, Ordering::SeqCst);
    lookup(&resolver).await.unwrap();
    clock.advance(Duration::from_secs(1));
    assert!(matches!(
        lookup(&resolver).await.unwrap_err().class(),
        PortErrorClass::Unavailable
    ));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn concurrent_misses_share_a_fill_and_config_generations_are_isolated() {
    let (resolver, probe, _) = fixture(60);
    probe.blocked.store(true, Ordering::SeqCst);
    let first = tokio::spawn({
        let resolver = resolver.clone();
        async move { lookup(&resolver).await }
    });
    wait_for_calls(&probe, 1).await;
    let second = tokio::spawn({
        let resolver = resolver.clone();
        async move { lookup(&resolver).await }
    });
    probe.blocked.store(false, Ordering::SeqCst);
    probe.gate.add_permits(1);
    assert_eq!(
        first.await.unwrap().unwrap(),
        second.await.unwrap().unwrap()
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);

    let (new, new_probe, _) = fixture(60);
    new_probe.ip.store(
        u32::from(std::net::Ipv4Addr::new(127, 0, 0, 2)),
        Ordering::SeqCst,
    );
    assert_eq!(
        lookup(&new).await.unwrap(),
        vec!["127.0.0.2:8443".parse().unwrap()]
    );
    assert_eq!(
        lookup(&resolver).await.unwrap(),
        vec!["127.0.0.1:8443".parse().unwrap()]
    );
}

#[tokio::test]
async fn cancelling_a_waiter_does_not_cancel_the_fill() {
    let (resolver, probe, clock) = fixture(60);
    probe.blocked.store(true, Ordering::SeqCst);
    let first = tokio::spawn({
        let resolver = resolver.clone();
        async move { lookup(&resolver).await }
    });
    wait_for_calls(&probe, 1).await;
    let cancellation = Cancellation::new();
    let waiting = resolver.resolve(
        address_request(),
        Deadline::new(clock.monotonic_now() + Duration::from_secs(10)),
        &cancellation,
    );
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut waiting)
            .await
            .is_err()
    );
    cancellation.cancel(CancelReason::Shutdown);
    assert!(matches!(
        waiting.await.unwrap_err().class(),
        PortErrorClass::Cancelled(CancelReason::Shutdown)
    ));
    probe.blocked.store(false, Ordering::SeqCst);
    probe.gate.add_permits(1);
    first.await.unwrap().unwrap();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelled_timed_out_and_dropped_fill_owners_release_the_permit() {
    for exit in ["cancel", "timeout", "drop"] {
        let (resolver, probe, clock) = fixture(60);
        probe.blocked.store(true, Ordering::SeqCst);
        let cancellation = Cancellation::new();
        let first = tokio::spawn({
            let resolver = resolver.clone();
            let cancellation = cancellation.clone();
            let deadline = Deadline::new(clock.monotonic_now() + Duration::from_secs(1));
            async move {
                resolver
                    .resolve(address_request(), deadline, &cancellation)
                    .await
            }
        });
        wait_for_calls(&probe, 1).await;
        match exit {
            "cancel" => {
                cancellation.cancel(CancelReason::Shutdown);
                assert!(matches!(
                    first.await.unwrap().unwrap_err().class(),
                    PortErrorClass::Cancelled(CancelReason::Shutdown)
                ));
            }
            "timeout" => {
                clock.advance(Duration::from_secs(1));
                assert!(matches!(
                    first.await.unwrap().unwrap_err().class(),
                    PortErrorClass::Timeout
                ));
            }
            _ => {
                first.abort();
                assert!(first.await.unwrap_err().is_cancelled());
            }
        }
        probe.blocked.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), lookup(&resolver))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 3);
    }
}
