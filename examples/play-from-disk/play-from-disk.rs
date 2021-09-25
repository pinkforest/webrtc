use anyhow::Result;
use clap::{App, AppSettings, Arg};
use interceptor::registry::Registry;
use media::io::ivf_reader::IVFReader;
use media::io::ogg_reader::OggReader;
use media::Sample;
use std::fs::File;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::Duration;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::error::Error;
use webrtc::media::rtp::rtp_codec::RTCRtpCodecCapability;
use webrtc::media::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::media::track::track_local::TrackLocal;
use webrtc::peer::configuration::RTCConfiguration;
use webrtc::peer::ice::ice_connection_state::RTCIceConnectionState;
use webrtc::peer::ice::ice_server::RTCIceServer;
use webrtc::peer::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer::sdp::session_description::RTCSessionDescription;

#[tokio::main]
async fn main() -> Result<()> {
    let mut app = App::new("play-from-disk")
        .version("0.1.0")
        .author("Rain Liu <yliu@webrtc.rs>")
        .about("An example of play-from-disk.")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandsNegateReqs)
        .arg(
            Arg::with_name("FULLHELP")
                .help("Prints more detailed help information")
                .long("fullhelp"),
        )
        .arg(
            Arg::with_name("debug")
                .long("debug")
                .short("d")
                .help("Prints debug log information"),
        )
        .arg(
            Arg::with_name("video")
                .required_unless("FULLHELP")
                .takes_value(true)
                .short("v")
                .long("video")
                .help("Video file to be streaming."),
        )
        .arg(
            Arg::with_name("audio")
                .takes_value(true)
                .short("a")
                .long("audio")
                .help("Audio file to be streaming."),
        );

    let matches = app.clone().get_matches();

    if matches.is_present("FULLHELP") {
        app.print_long_help().unwrap();
        std::process::exit(0);
    }

    let debug = matches.is_present("debug");
    if debug {
        env_logger::Builder::new()
            .format(|buf, record| {
                writeln!(
                    buf,
                    "{}:{} [{}] {} - {}",
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    record.level(),
                    chrono::Local::now().format("%H:%M:%S.%6f"),
                    record.args()
                )
            })
            .filter(None, log::LevelFilter::Trace)
            .init();
    }

    let video_file = matches.value_of("video");
    let audio_file = matches.value_of("audio");

    if let Some(video_path) = &video_file {
        if !Path::new(video_path).exists() {
            return Err(Error::new(format!("video file: '{}' not exist", video_path)).into());
        }
    }
    if let Some(audio_path) = &audio_file {
        if !Path::new(audio_path).exists() {
            return Err(Error::new(format!("audio file: '{}' not exist", audio_path)).into());
        }
    }

    // Everything below is the WebRTC-rs API! Thanks for using it ❤️.

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    m.register_default_codecs()?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    let notify_tx = Arc::new(Notify::new());
    let notify_video = notify_tx.clone();
    let notify_audio = notify_tx.clone();

    if let Some(video_file) = video_file {
        // Create a video track
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "webrtc-rs".to_owned(),
        ));

        // Add this newly created track to the PeerConnection
        let rtp_sender = peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // Read incoming RTCP packets
        // Before these packets are returned they are processed by interceptors. For things
        // like NACK this needs to be called.
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
            Result::<()>::Ok(())
        });

        let video_file_name = video_file.to_owned();
        tokio::spawn(async move {
            // Open a IVF file and start reading using our IVFReader
            let file = File::open(video_file_name)?;
            let reader = BufReader::new(file);
            let (mut ivf, header) = IVFReader::new(reader)?;

            // Wait for connection established
            let _ = notify_video.notified().await;

            println!("play video from disk file output.ivf");

            // Send our video file frame at a time. Pace our sending so we send it at the same speed it should be played back as.
            // This isn't required since the video is timestamped, but we will such much higher loss if we send all at once.
            let sleep_time = Duration::from_millis(
                ((1000 * header.timebase_numerator) / header.timebase_denominator) as u64,
            );
            loop {
                let frame = match ivf.parse_next_frame() {
                    Ok((frame, _)) => frame,
                    Err(err) => {
                        println!("All video frames parsed and sent: {}", err);
                        break;
                    }
                };

                tokio::time::sleep(sleep_time).await;

                video_track
                    .write_sample(&Sample {
                        data: frame.freeze(),
                        duration: Duration::from_secs(1),
                        ..Default::default()
                    })
                    .await?;
            }

            Result::<()>::Ok(())
        });
    }

    if let Some(audio_file) = audio_file {
        // Create a audio track
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "webrtc-rs".to_owned(),
        ));

        // Add this newly created track to the PeerConnection
        let rtp_sender = peer_connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // Read incoming RTCP packets
        // Before these packets are returned they are processed by interceptors. For things
        // like NACK this needs to be called.
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
            Result::<()>::Ok(())
        });

        let audio_file_name = audio_file.to_owned();
        tokio::spawn(async move {
            // Open a IVF file and start reading using our IVFReader
            let file = File::open(audio_file_name)?;
            let reader = BufReader::new(file);
            // Open on oggfile in non-checksum mode.
            let (mut ogg, _) = OggReader::new(reader, true)?;

            // Wait for connection established
            let _ = notify_audio.notified().await;

            println!("play audio from disk file output.ogg");

            // Keep track of last granule, the difference is the amount of samples in the buffer
            let mut last_granule: u64 = 0;
            while let Ok((page_data, page_header)) = ogg.parse_next_page() {
                // The amount of samples is the difference between the last and current timestamp
                let sample_count = page_header.granule_position - last_granule;
                last_granule = page_header.granule_position;
                let sample_duration = Duration::from_millis(sample_count * 1000 / 48000);

                audio_track
                    .write_sample(&Sample {
                        data: page_data.freeze(),
                        duration: sample_duration,
                        ..Default::default()
                    })
                    .await?;

                tokio::time::sleep(sample_duration).await;
            }

            Result::<()>::Ok(())
        });
    }

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_ice_connection_state_change(Box::new(move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {}", connection_state);
            if connection_state == RTCIceConnectionState::Connected {
                notify_tx.notify_waiters();
            }
            Box::pin(async {})
        }))
        .await;

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            print!("Peer Connection State has changed: {}\n", s);

            if s == RTCPeerConnectionState::Failed {
                // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                println!("Peer Connection has gone to failed exiting");
                std::process::exit(0);
            }

            Box::pin(async {})
        }))
        .await;

    // Wait for the offer to be pasted
    let line = signal::must_read_stdin()?;
    let desc_data = signal::decode(line.as_str())?;
    let offer = serde_json::from_str::<RTCSessionDescription>(&desc_data)?;

    // Set the remote SessionDescription
    peer_connection.set_remote_description(offer).await?;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Output the answer in base64 so we can paste it in browser
    if let Some(local_desc) = peer_connection.local_description().await {
        let json_str = serde_json::to_string(&local_desc)?;
        let b64 = signal::encode(&json_str);
        println!("{}", b64);
    } else {
        println!("generate local_description failed!");
    }

    println!("Press ctlr-c to stop");
    tokio::signal::ctrl_c().await.unwrap();

    peer_connection.close().await?;

    Ok(())
}
