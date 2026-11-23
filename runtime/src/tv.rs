use std::{
    fmt,
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use lgtv_companion_core::webos::{REGISTRATION_ID, Request, registration_frame};
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde_json::{Value, json};
use tokio::{
    net::{TcpSocket, TcpStream},
    time::timeout,
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug)]
pub(crate) struct ExchangeError {
    pub uri: String,
    cause: Box<dyn std::error::Error + Send + Sync>,
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TV request outcome is unknown: {}", self.cause)
    }
}

impl std::error::Error for ExchangeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.cause)
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MESSAGE: usize = 1024 * 1024;

struct PinnedCertificate {
    certificate: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PinnedCertificate")
    }
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.certificate {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub struct Tv {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    request_id: u64,
}

impl Tv {
    /// With WSS, the configured leaf certificate is the trust anchor.
    /// TLS signature checks still verify possession of its private key.
    pub async fn connect(
        address: SocketAddrV4,
        source_ip: Option<Ipv4Addr>,
        certificate: Option<Vec<u8>>,
    ) -> Result<Self> {
        Self::connect_path(address, source_ip, certificate, "/").await
    }

    async fn connect_path(
        address: SocketAddrV4,
        source_ip: Option<Ipv4Addr>,
        certificate: Option<Vec<u8>>,
        path: &str,
    ) -> Result<Self> {
        let (scheme, connector) = if let Some(certificate) = certificate {
            let provider = rustls::crypto::ring::default_provider();
            let verifier = PinnedCertificate {
                certificate,
                algorithms: provider.signature_verification_algorithms,
            };
            let config = ClientConfig::builder_with_provider(Arc::new(provider))
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth();
            ("wss", Connector::Rustls(Arc::new(config)))
        } else {
            ("ws", Connector::Plain)
        };
        let socket = TcpSocket::new_v4()?;
        if let Some(source_ip) = source_ip {
            socket
                .bind(SocketAddrV4::new(source_ip, 0).into())
                .map_err(|error| {
                    format!("Cannot bind the TV connection to source_ip {source_ip}: {error}")
                })?;
        }
        let stream = timeout(REQUEST_TIMEOUT, socket.connect(address.into())).await??;
        let limits = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let (socket, _) = timeout(
            REQUEST_TIMEOUT,
            client_async_tls_with_config(
                format!("{scheme}://{address}{path}"),
                stream,
                Some(limits),
                Some(connector),
            ),
        )
        .await??;
        Ok(Self {
            socket,
            request_id: 0,
        })
    }

    pub async fn button(
        &mut self,
        address: SocketAddrV4,
        source_ip: Option<Ipv4Addr>,
        certificate: Option<Vec<u8>>,
        name: &str,
    ) -> Result<Value> {
        let response = self
            .request(Request::new(
                "",
                "com.webos.service.networkinput/getPointerInputSocket",
                None,
            ))
            .await?;
        if response.get("error").is_some() || response["returnValue"] == false {
            return Ok(response);
        }
        // Use the same configured TV and transport. Only the resource path
        // comes from the TV's response.
        let path = response["socketPath"]
            .as_str()
            .and_then(|url| url.find("/resources").map(|offset| &url[offset..]))
            .ok_or("TV returned an invalid pointer socket resource path")?;
        let mut pointer = Self::connect_path(address, source_ip, certificate, path).await?;
        let frame = Message::Text(format!("type:button\nname:{name}\n\n").into());
        timeout(REQUEST_TIMEOUT, pointer.socket.send(frame)).await??;
        Ok(json!({"info": "Button sent."}))
    }

    pub async fn register(&mut self, key: Option<&str>) -> Result<Option<String>> {
        let deadline = if key.is_some() {
            REQUEST_TIMEOUT
        } else {
            PAIR_TIMEOUT
        };
        timeout(deadline, async {
            self.send(&registration_frame(key)).await?;
            loop {
                let frame = self.receive(REGISTRATION_ID).await?;
                match frame["type"].as_str() {
                    Some("registered") => {
                        let new_key = frame["payload"]["client-key"]
                            .as_str()
                            .filter(|key| !key.is_empty() && key.len() <= 4096);
                        return match (key, new_key) {
                            (None, None) => {
                                Err("TV registered without returning a pairing key".into())
                            }
                            (_, new_key) => Ok(new_key.map(str::to_owned)),
                        };
                    }
                    Some("response")
                        if frame["payload"]["pairingType"].as_str() == Some("PROMPT") =>
                    {
                        if key.is_some() {
                            return Err(
                                "TV requires a fresh pairing. Use --pair to request one".into()
                            );
                        }
                    }
                    _ => {
                        return Err(if key.is_some() {
                            "TV rejected the saved pairing. Use --pair to request a fresh pairing"
                                .into()
                        } else {
                            "TV rejected or cancelled the pairing request".into()
                        });
                    }
                }
            }
        })
        .await?
    }

    pub async fn request(&mut self, mut request: Request) -> Result<Value> {
        self.request_id += 1;
        request.id = format!("request-{}", self.request_id);
        let exchange = timeout(REQUEST_TIMEOUT, async {
            self.send(&serde_json::to_value(&request)?).await?;
            let response = self.receive(&request.id).await?;
            match response["type"].as_str() {
                Some("response") if response["payload"].is_object() => {
                    Ok(response["payload"].clone())
                }
                Some("error") => Ok(json!({
                    "error": response["error"].as_str().unwrap_or("TV rejected the request")
                })),
                _ => Err("TV returned an invalid SSAP response".into()),
            }
        })
        .await;
        let result: Result<Value> = match exchange {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        };
        result.map_err(|cause| {
            ExchangeError {
                uri: request.uri,
                cause,
            }
            .into()
        })
    }

    /// No SSAP requests are pending when this is called.
    pub(crate) async fn idle_message(&mut self) -> Result<()> {
        match self.socket.next().await {
            Some(Ok(Message::Close(_))) | None => Err("TV closed its connection".into()),
            Some(Ok(Message::Ping(_))) => {
                timeout(REQUEST_TIMEOUT, self.socket.flush()).await??;
                Ok(())
            }
            Some(Err(error)) => Err(error.into()),
            Some(Ok(_)) => Ok(()),
        }
    }

    pub async fn luna(&mut self, request: Request) -> Result<Value> {
        let response = self.request(request).await?;
        if response.get("error").is_some() || response["returnValue"] == false {
            return Ok(response);
        }
        let alert = response["alertId"]
            .as_str()
            .and_then(|id| id.split_once('-').map(|(_, suffix)| suffix))
            .filter(|suffix| !suffix.is_empty())
            .ok_or("TV returned an invalid alert ID")?;
        let result = self
            .request(Request::new(
                "",
                "system.notifications/closeAlert",
                Some(json!({"alertId": format!("com.webos.service.apiadapter-{alert}")})),
            ))
            .await?;
        if result.get("error").is_some() || result["returnValue"] == false {
            return Ok(result);
        }
        Ok(json!({"info": "Cannot relay luna response."}))
    }

    async fn send(&mut self, frame: &Value) -> Result<()> {
        self.socket
            .send(Message::Text(serde_json::to_string(frame)?.into()))
            .await?;
        Ok(())
    }

    async fn receive(&mut self, id: &str) -> Result<Value> {
        while let Some(message) = self.socket.next().await {
            match message? {
                Message::Text(message) => {
                    let frame: Value = serde_json::from_str(message.as_str())?;
                    if frame["id"].as_str() == Some(id) {
                        return Ok(frame);
                    }
                }
                Message::Ping(_) => self.socket.flush().await?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        Err("TV disconnected before replying".into())
    }
}
