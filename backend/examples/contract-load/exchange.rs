use std::{net::SocketAddr, sync::Arc, time::Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hickory_proto::{
    op::{Message, MessageType, OpCode},
    serialize::binary::{BinDecodable, BinDecoder},
};
use reqwest::{Client, header};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};

use crate::{
    Result,
    config::{Config, Endpoint, PreparedQuery, Protocol},
};

const MAX_WIRE: usize = u16::MAX as usize;

pub struct TargetClient {
    endpoint: Endpoint,
    protocol: Protocol,
    http: Option<Client>,
}

pub struct Worker {
    sessions: Vec<Session>,
}

#[derive(Default)]
struct Session {
    udp: Option<UdpSocket>,
    tcp: Option<TcpStream>,
}

/// 错误只输出稳定类别和 OS code，不输出 URL、qname 或原始 wire。
#[derive(Clone, Copy)]
pub struct Failure {
    pub class: &'static str,
    pub os_code: Option<i32>,
    pub http_status: Option<u16>,
}

impl Failure {
    fn new(class: &'static str) -> Self {
        Self {
            class,
            os_code: None,
            http_status: None,
        }
    }

    fn http(error: reqwest::Error) -> Self {
        Self::new(if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "http_connect"
        } else {
            "http_io"
        })
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self {
            class: "socket_io",
            os_code: error.raw_os_error(),
            http_status: None,
        }
    }
}

pub struct Observation {
    pub target: usize,
    pub latency_us: u64,
    pub bytes: usize,
    pub outcome: std::result::Result<u16, Failure>,
}

impl TargetClient {
    pub fn prepare(config: &Config) -> Result<Vec<Self>> {
        config
            .targets
            .iter()
            .map(|target| {
                let endpoint = target.endpoint()?;
                let http = if matches!(endpoint, Endpoint::Doh(_)) {
                    Some(
                        Client::builder()
                            .no_proxy()
                            .http1_only()
                            .redirect(reqwest::redirect::Policy::none())
                            .pool_max_idle_per_host(if config.reuse_connections {
                                config.concurrency
                            } else {
                                0
                            })
                            .timeout(config.timeout())
                            .build()
                            .map_err(|_| "无法创建 DoH client")?,
                    )
                } else {
                    None
                };
                Ok(Self {
                    endpoint,
                    protocol: target.protocol,
                    http,
                })
            })
            .collect()
    }
}

impl Worker {
    pub fn new(targets: usize) -> Self {
        Self {
            sessions: (0..targets).map(|_| Session::default()).collect(),
        }
    }

    /// 拨号、握手、发送和读取共用一个 deadline；失败连接不返回复用池。
    pub async fn run(
        mut self,
        config: Arc<Config>,
        targets: Arc<Vec<TargetClient>>,
        queries: Arc<Vec<PreparedQuery>>,
        sequence: u64,
    ) -> (Self, Observation) {
        let target = (sequence % targets.len() as u64) as usize;
        // 按 target 轮转后再推进 query，避免协议和查询集合形成固定的一一对应。
        let query_index =
            (sequence / targets.len() as u64).wrapping_add(config.seed) % queries.len() as u64;
        let query = &queries[query_index as usize];
        let id = (sequence as u16).wrapping_add(config.seed as u16);
        let start = Instant::now();
        let result = tokio::time::timeout(
            config.timeout(),
            exchange(
                &targets[target],
                &mut self.sessions[target],
                query,
                id,
                config.reuse_connections,
            ),
        )
        .await
        .unwrap_or_else(|_| Err(Failure::new("timeout")));
        if result.is_err() || !config.reuse_connections {
            self.sessions[target] = Session::default();
        }
        let (bytes, outcome) = match result {
            Ok((bytes, rcode)) => (bytes, Ok(rcode)),
            Err(error) => (0, Err(error)),
        };
        (
            self,
            Observation {
                target,
                latency_us: start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                bytes,
                outcome,
            },
        )
    }
}

async fn exchange(
    target: &TargetClient,
    session: &mut Session,
    expected: &PreparedQuery,
    id: u16,
    reuse: bool,
) -> std::result::Result<(usize, u16), Failure> {
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(expected.question.clone());
    let wire = message.to_vec().map_err(|_| Failure::new("query_encode"))?;
    let response = match &target.endpoint {
        Endpoint::Udp(address) => {
            if session.udp.is_none() {
                let bind: SocketAddr = if address.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                }
                .parse()
                .expect("literal bind address");
                let socket = UdpSocket::bind(bind).await?;
                socket.connect(address).await?;
                session.udp = Some(socket);
            }
            let socket = session.udp.as_ref().expect("UDP session initialized");
            socket.send(&wire).await?;
            let mut response = vec![0; MAX_WIRE];
            loop {
                let size = socket.recv(&mut response).await?;
                // connected UDP 限定 peer；旧 ID 不得冒充当前请求，等待仍受原预算约束。
                if size >= 2 && response[..2] != id.to_be_bytes() {
                    continue;
                }
                response.truncate(size);
                break response;
            }
        }
        Endpoint::Tcp(address) => {
            if session.tcp.is_none() {
                session.tcp = Some(TcpStream::connect(address).await?);
            }
            let stream = session.tcp.as_mut().expect("TCP session initialized");
            stream.write_u16(wire.len() as u16).await?;
            stream.write_all(&wire).await?;
            let size = usize::from(stream.read_u16().await?);
            let mut response = vec![0; size];
            stream.read_exact(&mut response).await?;
            response
        }
        Endpoint::Doh(url) => {
            let client = target.http.as_ref().expect("HTTP client initialized");
            let request = if matches!(target.protocol, Protocol::DohGet) {
                let mut url = url.clone();
                url.query_pairs_mut()
                    .append_pair("dns", &URL_SAFE_NO_PAD.encode(&wire));
                client.get(url)
            } else {
                client
                    .post(url.clone())
                    .header(header::CONTENT_TYPE, "application/dns-message")
                    .body(wire)
            };
            let request = request.header(header::ACCEPT, "application/dns-message");
            let request = if reuse {
                request
            } else {
                request.header(header::CONNECTION, "close")
            };
            let mut response = request.send().await.map_err(Failure::http)?;
            if response.status() != 200 {
                return Err(Failure {
                    http_status: Some(response.status().as_u16()),
                    ..Failure::new("http_status")
                });
            }
            let valid_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/dns-message"));
            if !valid_type {
                return Err(Failure::new("http_content_type"));
            }
            if response
                .content_length()
                .is_some_and(|size| size > MAX_WIRE as u64)
            {
                return Err(Failure::new("http_body_limit"));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(Failure::http)? {
                if chunk.len() > MAX_WIRE - body.len() {
                    return Err(Failure::new("http_body_limit"));
                }
                body.extend_from_slice(&chunk);
            }
            body
        }
    };
    let mut decoder = BinDecoder::new(&response);
    let message = Message::read(&mut decoder).map_err(|_| Failure::new("dns_decode"))?;
    if decoder.index() != response.len()
        || message.metadata.message_type != MessageType::Response
        || message.metadata.op_code != OpCode::Query
        || message.metadata.id != id
        || message.metadata.truncation
        || message.queries.len() != 1
        || message.queries[0] != expected.question
    {
        return Err(Failure::new("dns_envelope"));
    }
    let rcode = u16::from(message.metadata.response_code);
    if rcode != expected.expected_rcode || message.answers.len() < expected.min_answers {
        return Err(Failure::new("dns_answer"));
    }
    Ok((response.len(), rcode))
}
