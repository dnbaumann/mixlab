use std::io;
use std::mem;
use std::sync::Arc;
use std::thread;

use bytes::Bytes;
use derive_more::From;
use faad2::Decoder;
use futures::executor::block_on;
use rml_rtmp::time::RtmpTimestamp;
use rml_rtmp::handshake::HandshakeError;
use rml_rtmp::sessions::{ServerSession, ServerSessionResult, ServerSessionError, ServerSessionEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::codec::avc::{self, DecoderConfigurationRecord, Bitstream, AvcPacket, AvcPacketType};
use crate::listen::PeekTcpStream;
use crate::source::{Registry, ConnectError, SourceRecv, SourceSend, ListenError};

mod incoming;
mod packet;

use packet::AudioPacket;

lazy_static::lazy_static! {
    static ref MOUNTPOINTS: Registry = {
        let reg = Registry::new();
        mem::forget(reg.listen("my_stream_endpoint"));
        reg
    };
}

pub fn listen(mountpoint: &str) -> Result<SourceRecv, ListenError> {
    MOUNTPOINTS.listen(mountpoint)
}

#[derive(From, Debug)]
pub enum RtmpError {
    Io(io::Error),
    Handshake(HandshakeError),
    Session(ServerSessionError),
    SourceConnect(ConnectError),
    SourceSend,
}

pub async fn accept(mut stream: PeekTcpStream) -> Result<(), RtmpError> {
    let mut buff = vec![0u8; 4096];

    let (_, remaining_bytes) = incoming::handshake(&mut stream, &mut buff).await?;
    let mut session = incoming::setup_session(&mut stream).await?;
    let publish = incoming::handle_new_client(&mut stream, &mut session, remaining_bytes, &mut buff).await?;

    let source = match publish {
        Some(publish) => {
            println!("rtmp: client wants to publish on {:?} with stream_key {:?}",
                publish.app_name, publish.stream_key);

            // TODO handle stream keys

            let source = MOUNTPOINTS.connect(&publish.app_name)?;

            incoming::accept_publish(&mut stream, &mut session, &publish).await?;

            source
        }
        None => { return Ok(()); }
    };

    let mut ctx = ReceiveContext {
        stream,
        session,
        source,
        audio_codec: None,
        video_dcr: None,
    };

    thread::spawn(move || {
        run_receive_thread(&mut ctx, buff)
    });

    Ok(())
}

struct ReceiveContext {
    stream: PeekTcpStream,
    session: ServerSession,
    source: SourceSend,
    audio_codec: Option<faad2::Decoder>,
    video_dcr: Option<Arc<avc::DecoderConfigurationRecord>>,
}

// fn run_decode_thread(audio_rx: Receiver<(Bytes, RtmpTimestamp)>, video_rx: Receiver<(Bytes, RtmpTimestamp)>, mut source: SourceSend) {
//     enum Packet {
//         Audio(AudioPacket),
//         Video(Result<VideoPacket, VideoPacketError>),
//     }

//     let aac_packets = audio_rx.map(|(packet, _)| AudioPacket::parse(packet));
//     let avc_packets = video_rx.map(|(packet, _)| VideoPacket::parse(packet));
//     let mut packets = stream::select(
//         aac_packets.map(Packet::Audio),
//         avc_packets.map(Packet::Video),
//     );

//     use std::io::Write;
//     let mut video_dump = std::fs::File::create("dump.h264").unwrap();

//     while let Some(packet) = block_on(packets.next()) {
//         match packet {
//             Packet::Audio(AudioPacket::AacSequenceHeader(bytes)) => {
//                 if audio_codec.is_some() {
//                     eprintln!("rtmp: received second aac sequence header?");
//                 }

//                 // TODO - validate user input before passing it to faad2
//                 audio_codec = Some(Decoder::new(&bytes).expect("Decoder::new"));
//             }
//             Packet::Audio(AudioPacket::AacRawData(bytes)) => {
//                 if let Some(codec) = &mut audio_codec {
//                     let decode_info = codec.decode(&bytes).expect("codec.decode");
//                     match source.write(decode_info.samples) {
//                         Ok(_) => {}
//                         Err(_) => { break; }
//                     }
//                 } else {
//                     eprintln!("rtmp: received aac data packet before sequence header, dropping");
//                 }
//             }
//             Packet::Audio(AudioPacket::Unknown(_)) => {
//                 eprintln!("rtmp: received unknown audio packet, dropping");
//             }
//             Packet::Video(Ok(mut packet)) => {
//             }
//             Packet::Video(Err(e)) => {
//                 eprintln!("rtmp: received unknown video packet: {:?}", e);
//             }
//         }
//     }
// }

fn run_receive_thread(ctx: &mut ReceiveContext, mut buff: Vec<u8>) -> Result<(), RtmpError> {
    loop {
        match block_on(ctx.stream.read(&mut buff))? {
            0 => {
                return Ok(());
            }
            bytes => {
                let actions = ctx.session.handle_input(&buff[0..bytes])?;
                handle_session_results(ctx, actions)?;
            }
        }
    }
}

fn handle_session_results(
    ctx: &mut ReceiveContext,
    actions: Vec<ServerSessionResult>,
) -> Result<(), RtmpError> {
    for action in actions {
        match action {
            ServerSessionResult::OutboundResponse(packet) => {
                block_on(ctx.stream.write_all(&packet.bytes))?;
            }
            ServerSessionResult::RaisedEvent(ev) => {
                handle_event(ctx, ev)?;
            }
            ServerSessionResult::UnhandleableMessageReceived(msg) => {
                println!("rtmp: UnhandleableMessageReceived: {:?}", msg);
            }
        }
    }

    Ok(())
}

fn handle_event(
    ctx: &mut ReceiveContext,
    event: ServerSessionEvent,
) -> Result<(), RtmpError> {
    match event {
        ServerSessionEvent::AudioDataReceived { app_name: _, stream_key: _, data, timestamp } => {
            receive_audio_packet(ctx, data, timestamp)?;
            Ok(())
        }
        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
            receive_video_packet(ctx, data, timestamp)?;
            Ok(())
        }
        _ => {
            println!("unknown event received: {:?}", event);
            Ok(())
        }
    }
}

fn receive_audio_packet(
    ctx: &mut ReceiveContext,
    data: Bytes,
    _timestamp: RtmpTimestamp,
) -> Result<(), RtmpError> {
    let packet = AudioPacket::parse(data);

    match packet {
        AudioPacket::AacSequenceHeader(bytes) => {
            if ctx.audio_codec.is_some() {
                eprintln!("rtmp: received second aac sequence header?");
            }

            println!("bytes: {:?}", bytes);

            // TODO - validate user input before passing it to faad2
            ctx.audio_codec = Some(Decoder::new(&bytes).expect("Decoder::new"));
        }
        AudioPacket::AacRawData(bytes) => {
            if let Some(codec) = &mut ctx.audio_codec {
                let decode_info = codec.decode(&bytes).expect("codec.decode");

                ctx.source.write(decode_info.samples)
                    .map_err(|()| RtmpError::SourceSend)?;
            } else {
                eprintln!("rtmp: received aac data packet before sequence header, dropping");
            }
        }
        AudioPacket::Unknown(_) => {
            eprintln!("rtmp: received unknown audio packet, dropping");
        }
    }

    Ok(())
}

fn receive_video_packet(
    ctx: &mut ReceiveContext,
    data: Bytes,
    _timestamp: RtmpTimestamp,
) -> Result<(), RtmpError> {
    let mut packet = match AvcPacket::parse(data) {
        Ok(packet) => packet,
        Err(e) => {
            println!("rtmp: could not parse video packet: {:?}", e);
            return Ok(());
        }
    };

    if let AvcPacketType::SequenceHeader = packet.packet_type {
        match DecoderConfigurationRecord::parse(&mut packet.data) {
            Ok(dcr) => {
                if ctx.video_dcr.is_some() {
                    eprintln!("rtmp: received second avc sequence header?");
                }
                eprintln!("rtmp: received avc dcr: {:?}", dcr);
                ctx.video_dcr = Some(Arc::new(dcr));
            }
            Err(e) => {
                eprintln!("rtmp: could not read avc dcr: {:?}", e);
            }
        }
    }

    // println!("packet timestamp: {:?}", packet.timestamp);

    if let Some(dcr) = ctx.video_dcr.clone() {
        match Bitstream::parse(packet.data, dcr) {
            Ok(bitstream) => {
                // dump bit stream:
                {
                    use std::io::Write;

                    let mut video_dump = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("dump.h264")
                        .unwrap();

                    video_dump.write_all(&bitstream.try_as_bytes().unwrap()).unwrap();
                }

                // do stuff!
            }
            Err(e) => {
                eprintln!("rtmp: could not read avc bitstream: {:?}", e);
            }
        }
    } else {
        eprintln!("rtmp: cannot read avc frame without dcr");
    }

    // println!("frame_type: {:?}, avc_packet_type: {:?}, composition_time: {:?}",
    //     packet.frame_type, packet.avc_packet_type, packet.composition_time);

    // println!("data: (len = {:8}) {:x?}", packet.data.len(), &packet.data[0..32]);

    // match packet.avc_packet_type {
    //     AvcPacketType::SequenceHeader => {}
    //     AvcPacketType::EndOfSequence => {}
    //     _ => {
    //         video_dump.write_all(&packet.data).unwrap();
    //     }
    // }

    Ok(())
}
