use crate::message::Message;
use crate::state::State;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    Extension,
};
use futures::stream::StreamExt;
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
    let (mut game_sender, mut game_reader) = socket.split();

    // tell the game the phone number to call
    game_sender
        .send(Message::Text(state.twilio_phone_number.clone()).into())
        .await
        .expect("Failed to send the phone number to the game.");

    let mut game_code = rand::thread_rng().gen_range(0..100).to_string();

    // we add this manual scoping so that we drop the games lock after this logic
    {
        let mut games = state.games.lock().await;

        loop {
            if games.contains_key(&game_code) {
                game_code = rand::thread_rng().gen_range(0..100).to_string();
                continue;
            } else {
                // tell the game what game code we are assigning it
                game_sender
                    .send(Message::Text(game_code.clone()).into())
                    .await
                    .expect("Failed to send the game code to the game.");

                games.insert(game_code.clone(), game_sender);
                break;
            }
        }
    }

    while let Some(Ok(msg)) = game_reader.next().await {
        if let Message::Close(_) = Message::from(msg) {
            let mut games = state.games.lock().await;
            games.remove(&game_code);
        }
    }

    let mut games = state.games.lock().await;
    games.remove(&game_code);
}
