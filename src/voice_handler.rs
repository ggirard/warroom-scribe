// voice_handler.rs — Handler Songbird pour la réception audio Discord
//
// DAVE est transparent : le fork tazz4843/songbird gère le déchiffrement
// en interne. On reçoit du PCM propre dans VoiceData.decoded_voice.

use std::sync::Arc;

use songbird::events::{Event, EventContext, EventHandler};

use crate::session::RecordingSession;

#[derive(Clone)]
pub struct Handler {
    session: Arc<RecordingSession>,
}

impl Handler {
    pub fn new(session: Arc<RecordingSession>) -> Self {
        Self { session }
    }
}

#[async_trait::async_trait]
impl EventHandler for Handler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            // VoiceTick : ~toutes les 20ms — PCM déchiffré par DAVE, prêt à l'emploi
            EventContext::VoiceTick(tick) => {
                for (ssrc, voice_data) in &tick.speaking {
                    if let Some(decoded) = &voice_data.decoded_voice {
                        self.session.push_audio(*ssrc, decoded).await;
                    }
                }
            }

            // SpeakingStateUpdate : mappé SSRC ↔ UserId
            // Avec DAVE E2EE, user_id est souvent None — on log pour diagnostiquer
            EventContext::SpeakingStateUpdate(update) => {
                tracing::debug!("SpeakingStateUpdate: ssrc={} user_id={:?}", update.ssrc, update.user_id);
                if let Some(user_id) = update.user_id {
                    // display_name = fallback seulement si pas dans initial_user_names
                    self.session
                        .register_ssrc(update.ssrc, user_id.0, format!("User#{}", user_id.0))
                        .await;
                }
            }

            EventContext::ClientDisconnect(dc) => {
                tracing::debug!("Utilisateur {} a quitté le channel", dc.user_id.0);
            }

            _ => {}
        }

        None
    }
}
