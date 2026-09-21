//! A peer may send any message up to the `max-message-size` this endpoint advertises.
//!
//! Inbound reassembly used to be bounded by the negotiated *send* limit instead, the smaller of
//! the two peers' limits. Against a peer that omits the attribute ([RFC 8841 §6.1]: assume
//! 64 KiB) but can send more, every inbound message over 64 KiB failed with `ErrShortBuffer`
//! and was lost.
//!
//! [RFC 8841 §6.1]: https://datatracker.ietf.org/doc/html/rfc8841#section-6.1

use anyhow::Result;
use bytes::BytesMut;
use rtc::data_channel::RTCDataChannelInit;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::setting_engine::{
    SctpMaxMessageSize, SettingEngineBuilder,
};
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::{RTCMessage, TaggedRTCMessage};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCDtlsRole, RTCIceCandidate,
};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// Over the 64 KiB assumed for the peer, within the 256 KiB this endpoint advertises.
const MESSAGE_SIZE: usize = 128 * 1024;

const NEGOTIATED_ID: u16 = 1;
const TEST_TIMEOUT: Duration = Duration::from_secs(20);

struct Peer {
    pc: RTCPeerConnection,
    socket: UdpSocket,
    addr: SocketAddr,
    buf: Vec<u8>,
}

impl Peer {
    async fn new(role: RTCDtlsRole) -> Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let addr = socket.local_addr()?;

        let mut pc = RTCPeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_setting_engine(
                SettingEngineBuilder::new()
                    .with_answering_dtls_role(role)
                    .with_sctp_max_message_size(SctpMaxMessageSize::Bounded(
                        SctpMaxMessageSize::MAX_MESSAGE_SIZE,
                    ))
                    .build(),
            )
            .build(Instant::now())?;

        let candidate = CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: addr.ip().to_string(),
                port: addr.port(),
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()?;
        pc.add_local_candidate(RTCIceCandidate::from(&candidate).to_json()?)?;

        Ok(Self {
            pc,
            socket,
            addr,
            buf: vec![0u8; 2048],
        })
    }

    fn handle_datagram(&mut self, n: usize, peer_addr: SocketAddr) {
        self.pc
            .handle_read(TaggedBytesMut {
                now: Instant::now(),
                transport: TransportContext {
                    local_addr: self.addr,
                    peer_addr,
                    ecn: None,
                    transport_protocol: TransportProtocol::UDP,
                },
                message: BytesMut::from(&self.buf[..n]),
            })
            .ok();
    }
}

/// Flushes both peers, then waits for one datagram or the next timer.
async fn pump(a: &mut Peer, b: &mut Peer) -> Result<()> {
    for p in [&mut *a, &mut *b] {
        while let Some(msg) = p.pc.poll_write() {
            p.socket
                .send_to(&msg.message, msg.transport.peer_addr)
                .await?;
        }
    }

    let fallback = Instant::now() + Duration::from_secs(1);
    let next =
        a.pc.poll_timeout()
            .unwrap_or(fallback)
            .min(b.pc.poll_timeout().unwrap_or(fallback));
    let delay = next
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(5));

    tokio::select! {
        _ = tokio::time::sleep(delay) => {
            a.pc.handle_timeout(Instant::now()).ok();
            b.pc.handle_timeout(Instant::now()).ok();
        }
        Ok((n, peer_addr)) = a.socket.recv_from(&mut a.buf) => a.handle_datagram(n, peer_addr),
        Ok((n, peer_addr)) = b.socket.recv_from(&mut b.buf) => b.handle_datagram(n, peer_addr),
    }
    Ok(())
}

#[tokio::test]
async fn message_within_the_advertised_limit_is_delivered() -> Result<()> {
    let mut offer = Peer::new(RTCDtlsRole::Server).await?;
    let mut answer = Peer::new(RTCDtlsRole::Client).await?;

    let init = RTCDataChannelInit {
        ordered: true,
        negotiated: Some(NEGOTIATED_ID),
        ..Default::default()
    };
    let offer_dc = offer
        .pc
        .create_data_channel("size", Some(init.clone()))?
        .id();
    let answer_dc = answer.pc.create_data_channel("size", Some(init))?.id();

    let sdp = offer.pc.create_offer(None)?;
    offer
        .pc
        .set_local_description(Instant::now(), sdp.clone())?;
    answer.pc.set_remote_description(Instant::now(), sdp)?;
    let sdp = answer.pc.create_answer(None)?;
    answer
        .pc
        .set_local_description(Instant::now(), sdp.clone())?;

    // The offerer sees an answerer that names no limit, so it must assume 64 KiB for sending.
    let sdp = RTCSessionDescription::answer(
        sdp.sdp
            .lines()
            .filter(|line| !line.starts_with("a=max-message-size"))
            .map(|line| format!("{line}\r\n"))
            .collect(),
    )?;
    offer.pc.set_remote_description(Instant::now(), sdp)?;

    let mut connected = false;
    let mut open = false;
    let mut sent = false;
    let mut received = None;

    let start = Instant::now();
    while start.elapsed() < TEST_TIMEOUT && received.is_none() {
        pump(&mut offer, &mut answer).await?;

        while let Some(event) = answer.pc.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(
                    RTCPeerConnectionState::Connected,
                ) => connected = true,
                RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(id))
                    if id == answer_dc =>
                {
                    open = true
                }
                _ => {}
            }
        }
        while offer.pc.poll_event().is_some() {}
        while answer.pc.poll_read().is_some() {}
        while let Some(TaggedRTCMessage { message, .. }) = offer.pc.poll_read() {
            if let RTCMessage::DataChannelMessage(id, msg) = message
                && id == offer_dc
            {
                received = Some(msg.data.len());
            }
        }

        if connected && open && !sent {
            let mut dc = answer.pc.data_channel(answer_dc).expect("channel is open");
            dc.send(Instant::now(), BytesMut::zeroed(MESSAGE_SIZE))?;
            sent = true;
        }
    }

    assert!(sent, "peers never opened the channel");
    assert_eq!(
        offer.pc.sctp().and_then(|sctp| sctp.max_message_size()),
        Some(64 * 1024),
        "test precondition: the offerer's send limit is the assumed 64 KiB"
    );
    assert_eq!(
        received,
        Some(MESSAGE_SIZE),
        "a message within the advertised max-message-size must be delivered"
    );

    offer.pc.close()?;
    answer.pc.close()?;
    Ok(())
}
