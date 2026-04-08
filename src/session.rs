// session.rs — Gestion du cycle de vie d'un enregistrement
//
// Équivalent de session.py en Python, mais avec les patterns Rust :
// - Arc<T> pour partager des données entre tâches async (comme Python asyncio.Task)
// - Mutex<T> pour protéger les données mutables (comme asyncio.Lock)
// - tokio::spawn pour les tâches en arrière-plan (comme asyncio.create_task)

use std::{collections::{HashMap, HashSet}, sync::Arc};

use chrono::{DateTime, Utc};
use poise::serenity_prelude::GuildId;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Types de données
// ---------------------------------------------------------------------------

/// Un segment de transcription d'un utilisateur.
/// `#[derive(Debug, Clone)]` génère automatiquement l'affichage debug et la copie.
#[derive(Debug, Clone)]
pub struct Segment {
    pub user: String,
    pub start: f64, // secondes depuis le début du batch
    pub end: f64,
    pub text: String,
}

// ---------------------------------------------------------------------------
// État interne de la session (protégé par Mutex)
// ---------------------------------------------------------------------------

struct SessionInner {
    /// Buffers audio par userId (PCM i16, 48kHz, stéréo)
    /// HashMap<user_id, Vec<i16>> — s'accumule entre les flushes
    audio_buffers: HashMap<u64, Vec<i16>>,

    /// Mapping SSRC (identifiant audio Discord) → user_id Discord
    /// Mis à jour par les événements SpeakingStateUpdate
    ssrc_to_user: HashMap<u32, u64>,

    /// Noms d'affichage des utilisateurs (user_id → display_name)
    user_names: HashMap<u64, String>,

    /// Tous les segments accumulés depuis le début (pour la sortie finale)
    all_segments: Vec<Segment>,

    /// Compteur de batches traités
    batch_number: u32,

    /// Timestamp de début du batch en cours
    batch_start: DateTime<Utc>,

    /// IDs de users créés comme placeholder (ssrc as u64) — à résoudre vers les vrais users
    placeholder_ids: HashSet<u64>,

    /// Offset en secondes depuis batch_start pour le premier audio de chaque user dans ce batch
    /// Utilisé pour corriger les timestamps Whisper (chaque buffer est compressé sans silences)
    audio_start_offsets: HashMap<u64, f64>,
}

// ---------------------------------------------------------------------------
// RecordingSession — structure principale
// ---------------------------------------------------------------------------

pub struct RecordingSession {
    pub guild_id: GuildId,
    pub voice_channel_name: String,
    pub slack_channel: String,
    pub slack_thread_ts: String,
    pub start_time: DateTime<Utc>,
    pub flush_interval_secs: u64,

    // L'état mutable est caché dans un Mutex — seul moyen de le modifier.
    inner: Mutex<SessionInner>,

    // Handle vers la tâche de flush périodique
    flush_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    // Signal d'arrêt gracieux : interrompt le sleep mais attend la fin du flush en cours
    flush_stop_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl RecordingSession {
    pub fn new(
        guild_id: GuildId,
        voice_channel_name: String,
        slack_channel: String,
        slack_thread_ts: String,
        flush_interval_secs: u64,
        initial_user_names: HashMap<u64, String>,
    ) -> Self {
        let now = Utc::now();
        tracing::info!("RecordingSession créée avec {} user(s): {:?}", initial_user_names.len(), initial_user_names);
        Self {
            guild_id,
            voice_channel_name,
            slack_channel,
            slack_thread_ts,
            start_time: now,
            flush_interval_secs,
            inner: Mutex::new(SessionInner {
                audio_buffers: HashMap::new(),
                ssrc_to_user: HashMap::new(),
                user_names: initial_user_names,
                all_segments: Vec::new(),
                batch_number: 0,
                batch_start: now,
                placeholder_ids: HashSet::new(),
                audio_start_offsets: HashMap::new(),
            }),
            flush_task: Mutex::new(None),
            flush_stop_tx: Mutex::new(None),
        }
    }

    // ------------------------------------------------------------------
    // API publique
    // ------------------------------------------------------------------

    /// Démarrer la boucle de flush périodique en arrière-plan.
    /// Prend `self: &Arc<Self>` pour cloner l'Arc dans la tâche spawned.
    pub async fn start(self: &Arc<Self>) {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        *self.flush_stop_tx.lock().await = Some(stop_tx);

        let session = Arc::clone(self);
        let handle = tokio::spawn(async move {
            session.periodic_flush_loop(stop_rx).await;
        });
        *self.flush_task.lock().await = Some(handle);
    }

    /// Arrêter l'enregistrement : arrêt gracieux du flush périodique, flush final.
    /// Retourne (end_time, all_segments) pour la mise en forme finale.
    pub async fn stop(self: &Arc<Self>) -> (DateTime<Utc>, Vec<Segment>) {
        // Envoyer le signal d'arrêt : interrompt le sleep, mais pas un flush en cours
        if let Some(tx) = self.flush_stop_tx.lock().await.take() {
            tx.send(()).ok();
        }

        // Attendre que le flush en cours se termine naturellement (ne pas abort()!)
        if let Some(handle) = self.flush_task.lock().await.take() {
            handle.await.ok();
        }

        let end_time = Utc::now();

        // Flush du dernier batch
        self.flush_batch(true).await;

        let inner = self.inner.lock().await;
        (end_time, inner.all_segments.clone())
    }

    /// Durée formatée depuis le début de l'enregistrement
    pub fn duration_str(&self) -> String {
        let secs = (Utc::now() - self.start_time).num_seconds().max(0);
        format!("{}m {}s", secs / 60, secs % 60)
    }

    /// Nombre de batches traités (lecture non-bloquante)
    pub fn batch_count(&self) -> u32 {
        self.inner.try_lock().map(|g| g.batch_number).unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // Méthodes appelées par voice_handler.rs
    // ------------------------------------------------------------------

    /// Ajouter des samples PCM pour un SSRC donné.
    /// Le SSRC est mappé en user_id via ssrc_to_user.
    pub async fn push_audio(&self, ssrc: u32, samples: &[i16]) {
        let mut inner = self.inner.lock().await;
        let user_id = match inner.ssrc_to_user.get(&ssrc).copied() {
            Some(id) => id,
            None => {
                // SpeakingStateUpdate peut être manqué si l'user parlait déjà quand le bot
                // a rejoint. On essaie d'abord l'auto-mapping (un seul user sans SSRC connu),
                // sinon on crée un placeholder pour ne pas perdre l'audio.
                let unmapped_users: Vec<u64> = inner.user_names.keys()
                    .filter(|uid| !inner.ssrc_to_user.values().any(|v| v == *uid))
                    .copied()
                    .collect();
                if unmapped_users.len() == 1 {
                    let uid = unmapped_users[0];
                    inner.ssrc_to_user.insert(ssrc, uid);
                    tracing::info!("Auto-mapped SSRC {} → user {} (single-user fallback)", ssrc, uid);
                    uid
                } else {
                    // Fallback : placeholder basé sur le SSRC.
                    // Sera résolu vers un vrai user avant le prochain flush.
                    let placeholder_uid = ssrc as u64;
                    if !inner.user_names.contains_key(&placeholder_uid) {
                        inner.user_names.insert(placeholder_uid, format!("User#{ssrc}"));
                        inner.placeholder_ids.insert(placeholder_uid);
                        tracing::info!("SSRC {} → placeholder créé (sera résolu au flush)", ssrc);
                    }
                    inner.ssrc_to_user.insert(ssrc, placeholder_uid);
                    placeholder_uid
                }
            }
        };
        inner.audio_buffers.entry(user_id).or_default().extend_from_slice(samples);

        // Enregistrer le premier timestamp de parole dans ce batch (pour corriger les timestamps Whisper)
        let batch_start = inner.batch_start;
        inner.audio_start_offsets.entry(user_id).or_insert_with(|| {
            (Utc::now() - batch_start).num_milliseconds() as f64 / 1000.0
        });
    }

    /// Mettre à jour le mapping SSRC → user_id (appelé par SpeakingStateUpdate)
    pub async fn register_ssrc(&self, ssrc: u32, user_id: u64, display_name: String) {
        let mut inner = self.inner.lock().await;
        let old_uid = inner.ssrc_to_user.get(&ssrc).copied();
        inner.ssrc_to_user.insert(ssrc, user_id);
        inner.user_names.entry(user_id).or_insert(display_name);

        // Si ce SSRC était sur un placeholder, migrer le buffer audio vers le vrai user
        if let Some(placeholder_uid) = old_uid {
            if placeholder_uid != user_id && placeholder_uid == ssrc as u64 {
                if let Some(buffer) = inner.audio_buffers.remove(&placeholder_uid) {
                    tracing::info!(
                        "Migré buffer placeholder SSRC {} ({} samples) → user {}",
                        ssrc, buffer.len(), user_id
                    );
                    inner.audio_buffers.entry(user_id).or_default().extend(buffer);
                    inner.user_names.remove(&placeholder_uid);
                    inner.placeholder_ids.remove(&placeholder_uid);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Logique interne
    // ------------------------------------------------------------------

    async fn periodic_flush_loop(
        self: &Arc<Self>,
        mut stop_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let interval = std::time::Duration::from_secs(self.flush_interval_secs);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    self.flush_batch(false).await;
                    // Vérifier si stop() a été appelé pendant ce flush
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                }
                _ = &mut stop_rx => {
                    // Signal reçu pendant le sleep — sortir sans démarrer un nouveau batch
                    break;
                }
            }
        }
    }

    async fn flush_batch(&self, is_final: bool) {
        // Prendre un snapshot de l'état actuel, réinitialiser les buffers
        let (buffers, user_names, audio_offsets, batch_num, batch_start, batch_end) = {
            let mut inner = self.inner.lock().await;

            // Résoudre les placeholders SSRC → vrais users avant de prendre le snapshot.
            // Avec DAVE E2EE, SpeakingStateUpdate ne fournit pas user_id, donc on approxime :
            // on trie les SSRCs placeholder et les vrais users non-mappés, puis on les zippe.
            // (Plus fiable que "User#82", même si l'ordre peut être inexact dans de rares cas.)
            {
                let mut placeholder_ssrcs: Vec<u32> = inner.ssrc_to_user.iter()
                    .filter(|(_, &uid)| inner.placeholder_ids.contains(&uid))
                    .map(|(&ssrc, _)| ssrc)
                    .collect();

                let mut unmapped_real: Vec<u64> = inner.user_names.keys()
                    .filter(|uid| !inner.placeholder_ids.contains(uid))
                    .filter(|uid| !inner.ssrc_to_user.values().any(|v| v == *uid))
                    .copied()
                    .collect();

                if !placeholder_ssrcs.is_empty() && !unmapped_real.is_empty() {
                    placeholder_ssrcs.sort();
                    unmapped_real.sort();
                    for (&ssrc, &real_uid) in placeholder_ssrcs.iter().zip(unmapped_real.iter()) {
                        let placeholder_uid = ssrc as u64;
                        if let Some(buf) = inner.audio_buffers.remove(&placeholder_uid) {
                            inner.audio_buffers.entry(real_uid).or_default().extend(buf);
                        }
                        if let Some(offset) = inner.audio_start_offsets.remove(&placeholder_uid) {
                            inner.audio_start_offsets.entry(real_uid).or_insert(offset);
                        }
                        inner.ssrc_to_user.insert(ssrc, real_uid);
                        inner.user_names.remove(&placeholder_uid);
                        inner.placeholder_ids.remove(&placeholder_uid);
                        tracing::info!(
                            "Résolu SSRC {} → user {} (approximation numérique)",
                            ssrc, real_uid
                        );
                    }
                }
            }

            // std::mem::take() vide le HashMap et retourne son contenu
            // C'est l'équivalent de Python : old = d; d = {}
            let buffers = std::mem::take(&mut inner.audio_buffers);
            let audio_offsets = std::mem::take(&mut inner.audio_start_offsets);
            let user_names = inner.user_names.clone();

            inner.batch_number += 1;
            let batch_num = inner.batch_number;
            let batch_start = inner.batch_start;
            let batch_end = Utc::now();
            inner.batch_start = batch_end;

            (buffers, user_names, audio_offsets, batch_num, batch_start, batch_end)
        }; // <-- le Mutex est relâché ici (fin du scope)

        if buffers.is_empty() {
            tracing::debug!("Batch {batch_num} : aucun audio à traiter");
            return;
        }

        tracing::info!("Traitement du batch {batch_num} ({} utilisateurs)...", buffers.len());

        // Transcription (peut prendre quelques secondes — tourne dans spawn_blocking)
        let segments = crate::transcriber::transcribe_audio_buffers(&buffers, &user_names, &audio_offsets).await;

        // Calculer l'offset depuis le début de la war room et stocker les segments
        let batch_offset = (batch_start - self.start_time).num_milliseconds() as f64 / 1000.0;
        {
            let mut inner = self.inner.lock().await;
            for seg in &segments {
                inner.all_segments.push(Segment {
                    start: seg.start + batch_offset,
                    end: seg.end + batch_offset,
                    ..seg.clone() // `..` copie les autres champs (user, text)
                });
            }
        }

        // Formater et poster sur Slack
        let chunks = crate::formatter::format_raw_batch(&segments, batch_num, batch_start, batch_end);
        if let Err(e) = crate::slack::post_to_thread(
            &self.slack_channel,
            &self.slack_thread_ts,
            &chunks,
        )
        .await
        {
            tracing::error!("Erreur post Slack batch {batch_num}: {e}");
        }

        tracing::info!("Batch {batch_num} traité ({} segments)", segments.len());
        let _ = is_final; // non utilisé pour l'instant
    }
}
