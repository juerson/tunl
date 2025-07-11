use crate::common::{
    KDFSALT_CONST_AEAD_RESP_HEADER_IV, KDFSALT_CONST_AEAD_RESP_HEADER_KEY,
    KDFSALT_CONST_AEAD_RESP_HEADER_LEN_IV, KDFSALT_CONST_AEAD_RESP_HEADER_LEN_KEY,
    KDFSALT_CONST_VMESS_HEADER_PAYLOAD_AEAD_IV, KDFSALT_CONST_VMESS_HEADER_PAYLOAD_AEAD_KEY,
    KDFSALT_CONST_VMESS_HEADER_PAYLOAD_LENGTH_AEAD_IV,
    KDFSALT_CONST_VMESS_HEADER_PAYLOAD_LENGTH_AEAD_KEY, hash,
};
use crate::config::Config;
use aes::cipher::KeyInit;
use aes_gcm::{
    Aes128Gcm,
    aead::{Aead, Payload},
};
use md5::{Digest, Md5};
use sha2::Sha256;
use std::{
    io::Cursor,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::copy_bidirectional;

use bytes::{BufMut, BytesMut};
use futures_util::Stream;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use worker::*;

pin_project! {
    pub struct VmessStream<'a> {
        pub config: Config,
        pub ws: &'a WebSocket,
        pub buffer: BytesMut,
        #[pin]
        pub events: EventStream<'a>,
    }
}

impl<'a> VmessStream<'a> {
    pub fn new(config: Config, ws: &'a WebSocket, events: EventStream<'a>) -> Self {
        let buffer = BytesMut::new();

        Self {
            config,
            ws,
            buffer,
            events,
        }
    }

    async fn aead_decrypt(&mut self) -> Result<Vec<u8>> {
        let key = crate::md5!(
            &self.config.uuid.as_bytes(),
            b"c48619fe-8f02-49e0-b9e9-edf763e17e21"
        );

        // +-------------------+-------------------+-------------------+
        // |     Auth ID       |   Header Length   |       Nonce       |
        // +-------------------+-------------------+-------------------+
        // |     16 Bytes      |     18 Bytes      |      8 Bytes      |
        // +-------------------+-------------------+-------------------+
        let mut auth_id = [0u8; 16];
        self.read_exact(&mut auth_id).await?;
        let mut len = [0u8; 18];
        self.read_exact(&mut len).await?;
        let mut nonce = [0u8; 8];
        self.read_exact(&mut nonce).await?;

        // https://github.com/v2fly/v2ray-core/blob/master/proxy/vmess/aead/kdf.go
        let header_length = {
            let header_length_key = &hash::kdf(
                &key,
                &[
                    KDFSALT_CONST_VMESS_HEADER_PAYLOAD_LENGTH_AEAD_KEY,
                    &auth_id,
                    &nonce,
                ],
            )[..16];
            let header_length_nonce = &hash::kdf(
                &key,
                &[
                    KDFSALT_CONST_VMESS_HEADER_PAYLOAD_LENGTH_AEAD_IV,
                    &auth_id,
                    &nonce,
                ],
            )[..12];

            let payload = Payload {
                msg: &len,
                aad: &auth_id,
            };

            let len = Aes128Gcm::new(header_length_key.into())
                .decrypt(header_length_nonce.into(), payload)
                .map_err(|e| Error::RustError(e.to_string()))?;

            ((len[0] as u16) << 8) | (len[1] as u16)
        };

        // 16 bytes padding
        let mut cmd = vec![0u8; (header_length + 16) as _];
        self.read_exact(&mut cmd).await?;

        let header_payload = {
            let payload_key = &hash::kdf(
                &key,
                &[
                    KDFSALT_CONST_VMESS_HEADER_PAYLOAD_AEAD_KEY,
                    &auth_id,
                    &nonce,
                ],
            )[..16];
            let payload_nonce = &hash::kdf(
                &key,
                &[KDFSALT_CONST_VMESS_HEADER_PAYLOAD_AEAD_IV, &auth_id, &nonce],
            )[..12];

            let payload = Payload {
                msg: &cmd,
                aad: &auth_id,
            };

            Aes128Gcm::new(payload_key.into())
                .decrypt(payload_nonce.into(), payload)
                .map_err(|e| Error::RustError(e.to_string()))?
        };

        Ok(header_payload)
    }

    pub async fn process(&mut self) -> Result<()> {
        let mut buf = Cursor::new(self.aead_decrypt().await?);

        // https://xtls.github.io/en/development/protocols/vmess.html#command-section
        //
        // +---------+--------------------+---------------------+-------------------------------+---------+----------+-------------------+----------+---------+---------+--------------+---------+--------------+----------+
        // | 1 Byte  |      16 Bytes      |      16 Bytes       |            1 Byte             | 1 Byte  |  4 bits  |      4 bits       |  1 Byte  | 1 Byte  | 2 Bytes |    1 Byte    | N Bytes |   P Bytes    | 4 Bytes  |
        // +---------+--------------------+---------------------+-------------------------------+---------+----------+-------------------+----------+---------+---------+--------------+---------+--------------+----------+
        // | Version | Data Encryption IV | Data Encryption Key | Response Authentication Value | Options | Reserved | Encryption Method | Reserved | Command | Port    | Address Type | Address | Random Value | Checksum |
        // +---------+--------------------+---------------------+-------------------------------+---------+----------+-------------------+----------+---------+---------+--------------+---------+--------------+----------+

        let version = buf.read_u8().await?;
        if version != 1 {
            return Err(Error::RustError("invalid version".to_string()));
        }

        let mut iv = [0u8; 16];
        buf.read_exact(&mut iv).await?;
        let mut key = [0u8; 16];
        buf.read_exact(&mut key).await?;

        // ignore options for now
        let mut options = [0u8; 4];
        buf.read_exact(&mut options).await?;

        let cmd = buf.read_u8().await?;
        let is_tcp = cmd == 0x01;

        let port = buf.read_u16().await?;
        let addr = crate::common::parse_addr(&mut buf).await?;

        console_log!("connecting to upstream {}:{} [is_tcp={is_tcp}]", addr, port);

        let key = &crate::sha256!(&key)[..16];
        let iv = &crate::sha256!(&iv)[..16];

        // https://github.com/v2ray/v2ray-core/blob/master/proxy/vmess/encoding/client.go#L196
        let length_key = &hash::kdf(&key, &[KDFSALT_CONST_AEAD_RESP_HEADER_LEN_KEY])[..16];
        let length_iv = &hash::kdf(&iv, &[KDFSALT_CONST_AEAD_RESP_HEADER_LEN_IV])[..12];
        let length = Aes128Gcm::new(length_key.into())
            // 4 bytes header: https://github.com/v2ray/v2ray-core/blob/master/proxy/vmess/encoding/client.go#L238
            .encrypt(length_iv.into(), &4u16.to_be_bytes()[..])
            .map_err(|e| Error::RustError(e.to_string()))?;
        self.write(&length).await?;

        let payload_key = &hash::kdf(&key, &[KDFSALT_CONST_AEAD_RESP_HEADER_KEY])[..16];
        let payload_iv = &hash::kdf(&iv, &[KDFSALT_CONST_AEAD_RESP_HEADER_IV])[..12];
        let header = {
            let header = [
                options[0], // https://github.com/v2ray/v2ray-core/blob/master/proxy/vmess/encoding/client.go#L242
                0x00, 0x00, 0x00,
            ];
            Aes128Gcm::new(payload_key.into())
                .encrypt(payload_iv.into(), &header[..])
                .map_err(|e| Error::RustError(e.to_string()))?
        };
        self.write(&header).await?;

        if is_tcp {
            let mut errors = vec![];
            let proxy_addr: Vec<String> = self.config.proxy_ip.clone();

            for target in [vec![addr.clone()], proxy_addr.clone()].concat() {
                let (remote_host, remote_port) = if proxy_addr.contains(&target) {
                    match target.rsplit_once(':') {
                        Some((host, port_str)) => {
                            (host.to_string(), port_str.parse::<u16>().unwrap_or(port))
                        }
                        None => (target.clone(), port),
                    }
                } else {
                    (target.clone(), port)
                };

                match Socket::builder().connect(remote_host, remote_port) {
                    Ok(mut remote) => {
                        if remote.opened().await.is_err() {
                            errors.push("socket not open".into());
                            continue;
                        }

                        // start forwarding
                        if let Err(e) = copy_bidirectional(self, &mut remote).await {
                            errors.push(e.to_string());
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        errors.push(e.to_string());
                        continue;
                    }
                }
            }

            Err(Error::RustError(format!(
                "all tcp attempts failed: {errors:?}"
            )))
        } else {
            // cloudflare worker doesn't support udp but we can handle some special cases
            // for example if request is dns over udp we can instead use the request and
            // handle it using a DoH.

            // DNS:
            let mut buff = vec![0u8; 65535];
            let n = self.read(&mut buff).await?;
            let data = &buff[..n];
            if crate::dns::doh(data).await.is_ok() {
                self.write(&data).await?;
            }
            Ok(())
        }
    }
}

impl<'a> AsyncRead for VmessStream<'a> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<tokio::io::Result<()>> {
        let mut this = self.project();

        loop {
            let size = std::cmp::min(this.buffer.len(), buf.remaining());
            if size > 0 {
                buf.put_slice(&this.buffer.split_to(size));
                return Poll::Ready(Ok(()));
            }

            match this.events.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(WebsocketEvent::Message(msg)))) => {
                    msg.bytes().iter().for_each(|x| this.buffer.put_slice(&x));
                }
                Poll::Pending => return Poll::Pending,
                _ => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl<'a> AsyncWrite for VmessStream<'a> {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<tokio::io::Result<usize>> {
        return Poll::Ready(
            self.ws
                .send_with_bytes(buf)
                .map(|_| buf.len())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string())),
        );
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<tokio::io::Result<()>> {
        unimplemented!()
    }
}
