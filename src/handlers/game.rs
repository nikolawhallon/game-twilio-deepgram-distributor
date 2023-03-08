use crate::state::State;
use crate::{message::Message, state::GameTwilioTxs};
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    Extension,
};
use futures::stream::{SplitSink, SplitStream, StreamExt};
use futures::SinkExt;
use rand::Rng;
use std::sync::Arc;

pub async fn game_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<State>) {
    let (mut game_sender, game_reader) = socket.split();
    let (game_tx, game_rx) = crossbeam_channel::unbounded();

    // tell the game the phone number to call
    game_sender
        .send(Message::Text(state.twilio_phone_number.clone()).into())
        .await
        .expect("Failed to send the phone number to the game.");

    // generate a unique game code to allow users calling in to connect to this particular game
    let mut game_code = rand::thread_rng().gen_range(0..100).to_string();

    // we add this manual scoping so that we drop the games lock after this logic
    {
        let mut games = state.games.lock().await;

        loop {
            if games.contains_key(&game_code) {
                // the game code we generated was not unique, try again
                game_code = rand::thread_rng().gen_range(0..100).to_string();
                continue;
            } else {
                // tell the game what game code we are assigning it
                game_sender
                    .send(Message::Text(game_code.clone()).into())
                    .await
                    .expect("Failed to send the game code to the game.");

                // now that we have a game code, we will populate our state.games with
                // a game_twilio_txs object associated with this game code
                // the game_tx will be populated, but the twilio_tx will not be populated
                // until someone streaming through the twilio handler says the game code
                let game_twilio_txs = GameTwilioTxs {
                    game_tx,
                    twilio_tx: None,
                };

                games.insert(game_code.clone(), game_twilio_txs);
                break;
            }
        }
    }

    tokio::spawn(handle_from_game_tx(game_rx, game_sender));
    tokio::spawn(handle_from_game_ws(
        game_code.clone(),
        Arc::clone(&state),
        game_reader,
    ));
}

/// when the twilio handler sends a message here via the game_tx,
/// forward it to the game via the game_sender ws handle
async fn handle_from_game_tx(
    game_rx: crossbeam_channel::Receiver<Message>,
    mut game_sender: SplitSink<WebSocket, axum::extract::ws::Message>,
) {
    while let Ok(message) = game_rx.recv() {
        let _ = game_sender.send(message.into()).await;
    }
}

/// when the game sends a message here (arriving via our game_reader ws handle),
/// send it to our twilio handler via the twilio_tx - the twilio handler
/// will then interpret the message (make TTS audio) and send a message to twilio
/// (ultimately arriving as audio on the connected phone)
async fn handle_from_game_ws(
    game_code: String,
    state: Arc<State>,
    mut game_reader: SplitStream<WebSocket>,
) {
    while let Some(Ok(msg)) = game_reader.next().await {
        match msg {
            axum::extract::ws::Message::Close(_) => {
                let mut games = state.games.lock().await;
                games.remove(&game_code);
            }
            axum::extract::ws::Message::Text(_) => {
                // got text
                // send it to the relevent twilio_tx
                let mut games = state.games.lock().await;
                if let Some(game_twilio_tx) = games.get_mut(&game_code) {
                    if let Some(twilio_tx) = &game_twilio_tx.twilio_tx {
                        let _ = twilio_tx.send(Message::from(msg.clone()).into());
                    }
                }
            }
            _ => {
                // ignore
            }
        }
    }

    // when this ws handler is finished, make sure we clean up this games entry in state.games
    let mut games = state.games.lock().await;
    games.remove(&game_code);
}
