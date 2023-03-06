use axum::extract::ws::{Message, WebSocket};
use futures::lock::Mutex;
use futures::stream::SplitSink;
use std::collections::HashMap;

pub struct State {
    pub deepgram_url: String,
    pub api_key: String,
    pub twilio_phone_number: String,
    pub games: Mutex<HashMap<String, SplitSink<WebSocket, Message>>>,
}
