use crate::audio;
use crate::deepgram_response;
use crate::message::Message;
use crate::state::State;
use crate::twilio_response;
use aws_sdk_polly::model::{OutputFormat, VoiceId};
use aws_sdk_polly::Client;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    Extension,
};
use base64::{engine::general_purpose, Engine};
use futures::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use std::{convert::From, sync::Arc};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub async fn twilio_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<State>) {
    let (this_sender, this_receiver) = socket.split();
    let (twilio_tx, twilio_rx) = crossbeam_channel::unbounded();

    // prepare the deepgram connection request with the api key authentication
    let builder = http::Request::builder()
        .method(http::Method::GET)
        .uri(&state.deepgram_url);
    let builder = builder.header("Authorization", format!("Token {}", state.api_key));
    let request = builder
        .body(())
        .expect("Failed to build a connection request to Deepgram.");

    // connect to deepgram
    let (deepgram_socket, _) = connect_async(request)
        .await
        .expect("Failed to connect to Deepgram.");
    let (deepgram_sender, deepgram_reader) = deepgram_socket.split();

    tokio::spawn(handle_to_game_rx(
        Arc::clone(&state),
        deepgram_reader,
        twilio_tx,
    ));
    tokio::spawn(handle_from_twilio_ws(this_receiver, deepgram_sender));
    tokio::spawn(handle_from_twilio_tx(twilio_rx, this_sender));
}

/// when the game handler sends a message here via the twilio_tx,
/// obtain TTS audio for the message and forward it to twilio via
/// the twilio sender ws handle
async fn handle_from_twilio_tx(
    twilio_rx: crossbeam_channel::Receiver<Message>,
    _twilio_sender: SplitSink<WebSocket, axum::extract::ws::Message>,
) {
    while let Ok(message) = twilio_rx.recv() {
        if let Message::Text(message) = message {
            let shared_config = aws_config::from_env().load().await;
            let client = Client::new(&shared_config);

            if let Ok(polly_response) = client
                .synthesize_speech()
                .output_format(OutputFormat::Pcm)
                .sample_rate("8000")
                .text(message)
                .voice_id(VoiceId::Joanna)
                .send()
                .await
            {
                if let Ok(pcm) = polly_response
                    .audio_stream
                    .collect()
                    .await
                    .and_then(|aggregated_bytes| Ok(aggregated_bytes.to_vec()))
                {
                    let mut i16_samples = Vec::new();
                    for i in 0..(pcm.len() / 2) {
                        let mut i16_sample = pcm[i * 2] as i16;
                        i16_sample |= ((pcm[i * 2 + 1]) as i16) << 8;
                        i16_samples.push(i16_sample);
                    }

                    let mut mulaw_samples = Vec::new();
                    for sample in i16_samples {
                        mulaw_samples.push(audio::linear_to_ulaw(sample));
                    }

                    // remove this soon, it is just for testing that the TTS and mulaw conversion are working
                    let out_file = "test.mulaw";

                    let mut file = tokio::fs::File::create(out_file)
                        .await
                        .expect("failed to create file");

                    file.write_all_buf(&mut mulaw_samples.as_slice())
                        .await
                        .expect("failed to write to file");

                    // base64 encode the mulaw, wrap it in a Twilio media message, and send it to Twilio
                    let _base64_encoded_mulaw = general_purpose::STANDARD.encode(&mulaw_samples);
                    /*
                    {
                      "event": "media",
                      "streamSid": "MZ18ad3ab5a668481ce02b83e7395059f0",
                      "media": {
                        "payload": "a3242sadfasfa423242... (a base64 encoded string of 8000/mulaw)"
                      }
                    }
                     */
                    //let _ = twilio_sender.send(message.into()).await;
                }
            }
        }
    }
}

/// when we receive messages from deepgram, check to see what the user
/// said, and if they say an active game code, patch them into the game ws handler
/// via the GameTwilioTxs object - if they are patched, start sending
/// deepgram ASR results to the game_rx (via the game_tx) so that the game ws handler
/// can forward the results to the game via its game_sender ws handler
///
/// this function also takes the twilio_tx to populate the GameTwilioTxs object
/// once the user says a game code
async fn handle_to_game_rx(
    state: Arc<State>,
    mut deepgram_receiver: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    twilio_tx: crossbeam_channel::Sender<Message>,
) {
    let mut game_code: Option<String> = None;

    while let Some(Ok(msg)) = deepgram_receiver.next().await {
        let mut games = state.games.lock().await;
        if let Some(game_code) = game_code.as_ref() {
            if let Some(game_twilio_tx) = games.get_mut(game_code) {
                // send the message to the game
                let _ = game_twilio_tx
                    .game_tx
                    .send(Message::from(msg.clone()).into());
            } else {
                // this game existed, and no longer exists, so close the connection(s)?
                // or just make game "None" again
            }
        } else {
            // parse the deepgram result to see if we have a game connected with the spoken game code
            if let tungstenite::Message::Text(msg) = msg.clone() {
                let deepgram_response: Result<deepgram_response::StreamingResponse, _> =
                    serde_json::from_str(&msg);
                match deepgram_response {
                    Ok(deepgram_response) => {
                        for alternative in deepgram_response.channel.alternatives {
                            for key in games.keys() {
                                if alternative.transcript.contains(key) {
                                    game_code = Some(key.clone());
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }

                // if game_code was just populated, then we can finally finish connecting our twilio and game handlers
                if let Some(game_code) = game_code.clone() {
                    if let Some(game_twilio_tx) = games.get_mut(&game_code) {
                        game_twilio_tx.twilio_tx = Some(twilio_tx.clone());
                    }
                }
            }
        }
    }
}

/// when we receive a message from twilio via the this_receiver ws handle,
/// process the media and forward it directly to deepgram via
/// the deepgram_sender ws handle
async fn handle_from_twilio_ws(
    mut this_receiver: SplitStream<WebSocket>,
    mut deepgram_sender: SplitSink<
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::Message,
    >,
) {
    let mut buffer_data = audio::BufferData {
        inbound_buffer: Vec::new(),
        inbound_last_timestamp: 0,
    };

    while let Some(Ok(msg)) = this_receiver.next().await {
        let msg = Message::from(msg);
        if let Message::Text(msg) = msg {
            let event: Result<twilio_response::Event, _> = serde_json::from_str(&msg);
            if let Ok(event) = event {
                match event.event_type {
                    twilio_response::EventType::Start(_start) => {}
                    twilio_response::EventType::Media(media) => {
                        if let Some(audio) = audio::process_twilio_media(media, &mut buffer_data) {
                            // send the audio on to deepgram
                            if deepgram_sender
                                .send(Message::Binary(audio).into())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}
