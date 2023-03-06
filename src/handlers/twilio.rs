use crate::audio;
use crate::deepgram_response;
use crate::message::Message;
use crate::state::State;
use crate::twilio_response;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    Extension,
};
use futures::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use std::{convert::From, sync::Arc};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub async fn twilio_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<State>) {
    // TODO: use this_sender to send audio to the caller's phone
    let (_this_sender, this_receiver) = socket.split();

    // prepare the connection request with the api key authentication
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

    tokio::spawn(handle_to_game(Arc::clone(&state), deepgram_reader));
    tokio::spawn(handle_from_twilio(this_receiver, deepgram_sender));
}

async fn handle_to_game(
    state: Arc<State>,
    mut deepgram_receiver: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) {
    println!("handle_to_game");
    let mut game_code: Option<String> = None;

    while let Some(Ok(msg)) = deepgram_receiver.next().await {
        let mut games = state.games.lock().await;
        if let Some(game_code) = game_code.as_ref() {
            if let Some(game_ws) = games.get_mut(game_code) {
                // send the message to the game
                let _ = game_ws.send(Message::from(msg.clone()).into()).await;
            } else {
                // this game existed, and no longer exists, so close the connection(s)?
                // or just make game "None" again
            }
        } else {
            // parse the deepgram result to see if we have a game connected with the spoken game code
            if let tungstenite::Message::Text(msg) = msg.clone() {
                let deepgram_response: Result<Vec<deepgram_response::StreamingResponse>, _> =
                    serde_json::from_str(&msg);

                match deepgram_response {
                    Ok(deepgram_response) => {
                        for result in deepgram_response {
                            for alternative in result.channel.alternatives {
                                for key in games.keys() {
                                    if alternative.transcript.contains(key) {
                                        game_code = Some(key.clone());
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

async fn handle_from_twilio(
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
