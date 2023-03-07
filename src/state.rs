use crate::message::Message;
use futures::lock::Mutex;
use std::collections::HashMap;

pub struct State {
    pub deepgram_url: String,
    pub api_key: String,
    pub twilio_phone_number: String,
    pub games: Mutex<HashMap<String, GameTwilioTxs>>,
}

pub struct GameTwilioTxs {
    pub game_tx: crossbeam_channel::Sender<Message>,
    pub twilio_tx: Option<crossbeam_channel::Sender<Message>>,
}
