use super::{HANDSHAKE_TIMEOUT, constant_time_eq, forward_bidirectional, with_tcp_idle_timeout};
use crate::config::Config;
use crate::dns::DnsResolver;
use crate::protocol::{Host, Target};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http_body_util::{BodyExt, Empty, combinators::UnsyncBoxBody};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{
    CONNECTION, CONTENT_LENGTH, EXPECT, HOST, HeaderMap, HeaderName, HeaderValue,
    PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRANSFER_ENCODING, UPGRADE, VIA,
};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, Ipv6Addr};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_HTTP_BUFFER_SIZE: usize = 32 * 1024;
type ProxyBody = UnsyncBoxBody<Bytes, hyper::Error>;
type Tunnel = (hyper::upgrade::OnUpgrade, TcpStream);

pub(super) async fn serve(stream: TcpStream, config: &Config, dns: &DnsResolver) -> io::Result<()> {
    let credentials = (!config.username.is_empty() && !config.username.contains(':'))
        .then(|| STANDARD.encode(format!("{}:{}", config.username, config.password)));
    let (tunnel_tx, mut tunnel_rx) = mpsc::channel(1);
    let upstreams = CancellationToken::new();
    let _stop_upstreams = upstreams.clone().drop_guard();
    let service = service_fn(|request| async {
        let response = if config.username.is_empty()
            || credentials
                .as_deref()
                .is_some_and(|expected| authorized(request.headers(), expected))
        {
            match handle_request(request, dns, &tunnel_tx, &upstreams).await {
                Ok(response) => response,
                Err(status) => empty_response(status),
            }
        } else {
            let mut response = empty_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED);
            response.headers_mut().insert(
                PROXY_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"Brume\", charset=\"UTF-8\""),
            );
            response
        };
        Ok::<_, Infallible>(response)
    });

    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT)
        .max_buf_size(MAX_HTTP_BUFFER_SIZE);
    let (activity, activity_rx) = watch::channel(Instant::now());
    let io = ClientIo {
        stream,
        activity: config.tcp_timeout.map(|_| activity),
    };
    let connection = async {
        http.serve_connection(TokioIo::new(io), service)
            .with_upgrades()
            .await
            .map_err(io::Error::other)
    };
    if let Some(timeout) = config.tcp_timeout {
        with_tcp_idle_timeout(connection, activity_rx, timeout).await?;
    } else {
        connection.await?;
    }

    // 保留 Hyper 已读入的隧道数据，并在原连接任务内继续持有客户端配额。
    if let Ok((upgrade, mut remote)) = tunnel_rx.try_recv() {
        let upgraded = upgrade.await.map_err(io::Error::other)?;
        let mut client = TokioIo::new(upgraded);
        forward_bidirectional(
            tokio::io::split(&mut client),
            &mut remote,
            config.tcp_timeout,
        )
        .await?;
    }
    Ok(())
}

async fn handle_request(
    mut request: Request<Incoming>,
    dns: &DnsResolver,
    tunnel: &mpsc::Sender<Tunnel>,
    upstreams: &CancellationToken,
) -> Result<Response<ProxyBody>, StatusCode> {
    let connect = request.method() == Method::CONNECT;
    if request.headers().contains_key(UPGRADE) {
        return Err(StatusCode::NOT_IMPLEMENTED);
    }
    if !supported_transfer_encoding(request.headers())
        || (connect
            && (!request.body().is_end_stream()
                || request.headers().contains_key(TRANSFER_ENCODING)))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let target = target(request.uri(), connect).ok_or(StatusCode::BAD_REQUEST)?;
    let remote = dns
        .connect_tcp(&target, HANDSHAKE_TIMEOUT)
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        })?;
    remote
        .set_nodelay(true)
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if connect {
        tunnel
            .try_send((hyper::upgrade::on(&mut request), remote))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(empty_response(StatusCode::OK));
    }
    forward_request(request, remote, upstreams.clone()).await
}

async fn forward_request(
    request: Request<Incoming>,
    remote: TcpStream,
    upstreams: CancellationToken,
) -> Result<Response<ProxyBody>, StatusCode> {
    let (mut parts, body) = request.into_parts();
    let authority = parts.uri.authority().ok_or(StatusCode::BAD_REQUEST)?;
    let host = HeaderValue::from_str(authority.as_str()).map_err(|_| StatusCode::BAD_REQUEST)?;
    // 直连源站时使用 origin-form；路由和 Host 均以代理请求的绝对 URI 为准。
    parts.uri = parts
        .uri
        .path_and_query()
        .cloned()
        .map(Uri::from)
        .unwrap_or_else(|| Uri::from_static("/"));
    let fields = connection_fields(&parts.headers);
    strip_hop_headers(&mut parts.headers, &fields);
    // Hyper 在代理读取请求体时处理客户端的 100 Continue。
    parts.headers.remove(EXPECT);
    parts.headers.insert(HOST, host);
    parts
        .headers
        .insert(CONNECTION, HeaderValue::from_static("close"));
    append_via(&mut parts.headers, parts.version);
    parts.version = Version::HTTP_11;
    let request = Request::from_parts(parts, relay_body(body, fields));

    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_buf_size(MAX_HTTP_BUFFER_SIZE)
        .handshake(TokioIo::new(remote))
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    tokio::spawn(async move {
        tokio::select! {
            _ = upstreams.cancelled() => {}
            _ = connection => {}
        }
    });
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let (mut parts, body) = response.into_parts();
    if parts.status.is_informational() || !supported_transfer_encoding(&parts.headers) {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let fields = connection_fields(&parts.headers);
    strip_hop_headers(&mut parts.headers, &fields);
    append_via(&mut parts.headers, parts.version);
    Ok(Response::from_parts(parts, relay_body(body, fields)))
}

fn target(uri: &Uri, connect: bool) -> Option<Target> {
    if if connect {
        uri.scheme().is_some() || uri.path_and_query().is_some()
    } else {
        uri.scheme_str() != Some("http")
    } {
        return None;
    }
    let authority = uri.authority()?;
    let host = authority.host();
    if host.is_empty() || authority.as_str().contains('@') {
        return None;
    }
    let port = match authority.port_u16() {
        Some(port) if port != 0 => port,
        None if !connect && authority.as_str() == host => 80,
        _ => return None,
    };
    let host = if let Some(ipv6) = host
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
    {
        Host::Ip(IpAddr::V6(ipv6.parse::<Ipv6Addr>().ok()?))
    } else {
        match host.parse() {
            Ok(ip) => Host::Ip(ip),
            Err(_) => Host::Domain(host.to_owned()),
        }
    };
    Some(Target { host, port })
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let values = headers.get_all(PROXY_AUTHORIZATION);
    let mut values = values.iter();
    values
        .next()
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.split_once(' '))
        .is_some_and(|(scheme, token)| {
            values.next().is_none()
                && scheme.eq_ignore_ascii_case("Basic")
                && constant_time_eq(token.trim().as_bytes(), expected.as_bytes())
        })
}

fn empty_response(status: StatusCode) -> Response<ProxyBody> {
    let body = Empty::new().map_err(|never| match never {}).boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
}

fn supported_transfer_encoding(headers: &HeaderMap) -> bool {
    let values = headers.get_all(TRANSFER_ENCODING);
    let mut values = values.iter();
    values.next().is_none_or(|value| {
        value.as_bytes().eq_ignore_ascii_case(b"chunked") && values.next().is_none()
    })
}

fn connection_fields(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .filter_map(|field| HeaderName::from_bytes(field.trim_ascii()).ok())
        .collect()
}

fn strip_hop_headers(headers: &mut HeaderMap, fields: &[HeaderName]) {
    if headers.remove(TRANSFER_ENCODING).is_some() {
        headers.remove(CONTENT_LENGTH);
    }
    for field in fields {
        headers.remove(field);
    }
    for field in [
        CONNECTION,
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        TE,
        UPGRADE,
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
    ] {
        headers.remove(field);
    }
}

fn relay_body(body: Incoming, fields: Vec<HeaderName>) -> ProxyBody {
    body.map_frame(move |frame| match frame.into_trailers() {
        Ok(mut trailers) => {
            strip_hop_headers(&mut trailers, &fields);
            Frame::trailers(trailers)
        }
        Err(frame) => frame,
    })
    .boxed_unsync()
}

fn append_via(headers: &mut HeaderMap, version: Version) {
    let via = if version == Version::HTTP_10 {
        "1.0 brume"
    } else {
        "1.1 brume"
    };
    headers.append(VIA, HeaderValue::from_static(via));
}

struct ClientIo {
    stream: TcpStream,
    activity: Option<watch::Sender<Instant>>,
}

impl AsyncRead for ClientIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(())))
            && buf.filled().len() > before
            && let Some(activity) = &self.activity
        {
            activity.send_replace(Instant::now());
        }
        result
    }
}

impl AsyncWrite for ClientIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        if matches!(result, Poll::Ready(Ok(n)) if n > 0)
            && let Some(activity) = &self.activity
        {
            activity.send_replace(Instant::now());
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
