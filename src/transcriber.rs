// transcriber.rs — Transcription audio locale via whisper-rs (wrape whisper.cpp)
//
// Pipeline audio Discord → Whisper :
//   i16 stéréo 48kHz (Discord) → f32 mono 16kHz (Whisper)
//
// whisper-rs utilise deux objets :
//   WhisperContext = le modèle chargé en mémoire (lourd, partagé, thread-safe)
//   WhisperState   = l'état d'une transcription (léger, créé par appel)
//
// Comme la transcription est CPU-intensive (blocking), on utilise `spawn_blocking`
// pour ne pas bloquer le runtime async tokio.

use std::{collections::HashMap, sync::OnceLock};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::session::Segment;

// ---------------------------------------------------------------------------
// Singleton du modèle Whisper — chargé une seule fois au premier appel
// ---------------------------------------------------------------------------

// `OnceLock` est l'équivalent Rust de Python's `global _model = None` pattern.
// Il garantit que l'initialisation n'arrive qu'une seule fois, même en concurrence.
static WHISPER_CTX: OnceLock<WhisperContext> = OnceLock::new();

fn get_context() -> &'static WhisperContext {
    WHISPER_CTX.get_or_init(|| {
        let model_path = std::env::var("WHISPER_MODEL_PATH")
            .expect("WHISPER_MODEL_PATH manquant dans .env (chemin vers le fichier ggml-*.bin)");

        tracing::info!("Chargement du modèle Whisper depuis '{model_path}'...");

        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.use_gpu(true);

        let ctx = WhisperContext::new_with_params(&model_path, ctx_params)
            .expect("Impossible de charger le modèle Whisper — vérifie WHISPER_MODEL_PATH");

        tracing::info!("Modèle Whisper chargé.");
        ctx
    })
}

// ---------------------------------------------------------------------------
// Conversion audio : Discord PCM → format Whisper
// ---------------------------------------------------------------------------

/// Convertit les samples Discord en format attendu par Whisper.
///
/// Entrée  : i16 stéréo 48kHz  (format Discord/Opus)
/// Sortie  : f32 mono   16kHz  (format Whisper)
///
/// Les étapes :
///  1. i16 → f32 (normalisation : diviser par 32768.0)
///  2. Stéréo → mono (moyenne des deux canaux)
///  3. 48kHz → 16kHz (décimation par 3 — ratio exact, parfait pour la parole)
fn pcm_to_whisper(samples: &[i16]) -> Vec<f32> {
    // Étape 1 + 2 : convertir les paires stéréo en mono f32
    let mono_48k: Vec<f32> = samples
        .chunks(2) // prendre par paires (gauche, droite)
        .map(|pair| {
            let left = pair[0] as f32 / 32768.0;
            let right = pair.get(1).copied().unwrap_or(0) as f32 / 32768.0;
            (left + right) * 0.5 // moyenne pour le mono
        })
        .collect();

    // Étape 3 : décimation 48kHz → 16kHz par moyennage de groupes de 3 samples.
    // Bien meilleur que step_by(3) : filtre passe-bas simple qui réduit l'aliasing
    // (les fréquences > 8kHz dans la parole humaine sont rares mais causent des artefacts).
    mono_48k.chunks(3)
        .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
        .collect()
}

// ---------------------------------------------------------------------------
// Filtre d'hallucinations Whisper
// ---------------------------------------------------------------------------

/// Textes que Whisper génère souvent sur du silence ou du bruit en français/anglais.
/// Ces phrases viennent de son corpus d'entraînement (sous-titres, vidéos YouTube, etc.)
const WHISPER_HALLUCINATIONS: &[&str] = &[
    "sous-titrage société radio-canada",
    "sous-titres réalisés par",
    "merci d'avoir regardé",
    "abonnez-vous à la chaîne",
    "n'oubliez pas de vous abonner",
    "transcribed by",
    "subtitles by",
    "www.zeoranger.co.uk",
];

fn is_hallucination(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    if WHISPER_HALLUCINATIONS.iter().any(|h| lower.contains(h)) {
        return true;
    }
    // Détecter les répétitions excessives (ex: "oui, oui, oui..." ou "1, 2, 3, 4...")
    let words: Vec<&str> = lower.split_whitespace().collect();
    if words.len() >= 10 {
        let unique: std::collections::HashSet<&str> = words.iter().copied().collect();
        // Si moins de 20% de mots uniques → hallucination répétitive
        if unique.len() * 5 < words.len() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Transcription d'un utilisateur
// ---------------------------------------------------------------------------

/// Transcrit les samples audio d'un utilisateur.
/// Retourne les segments avec timestamps, ou Vec::new() si rien détecté.
fn transcribe_user_sync(username: &str, samples: &[i16], start_offset_secs: f64) -> Vec<Segment> {
    // Minimum 0.5s d'audio à 16kHz pour éviter les faux positifs Whisper
    const MIN_SAMPLES_16K: usize = 8000; // 0.5s × 16000 Hz

    let audio = pcm_to_whisper(samples);
    if audio.len() < MIN_SAMPLES_16K {
        return Vec::new();
    }

    let ctx = get_context();

    // Créer un état de transcription (léger, créé à chaque appel)
    let mut state = match ctx.create_state() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Impossible de créer l'état Whisper : {e}");
            return Vec::new();
        }
    };

    // Configurer les paramètres de transcription
    // BeamSearch(5) > Greedy : explore plus de chemins → meilleure précision (~20-30% d'erreur en moins)
    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: 1.0,
    });
    params.set_language(Some("fr")); // forcer le français
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_single_segment(false);
    // Seuils de qualité
    params.set_no_speech_thold(0.5);         // défaut 0.6 ; 0.5 = légèrement plus sensible
    params.set_entropy_thold(2.4);           // rejeter les sorties trop aléatoires
    params.set_logprob_thold(-1.0);          // rejeter les segments à faible confiance
// Contexte initial : guide le modèle vers le vocabulaire attendu
    params.set_initial_prompt(
        "Réunion d'équipe informatique en français québécois. War room d'incident. \
         Stack Java, cloud souverain. Vocabulaire : microservices, Kubernetes, pod, pipeline, \
         déploiement, rollback, logs, monitoring, alertes, base de données, API, certificat, \
         environnement, production, staging, ticket, Datadog, Grafana, Jenkins."
    );

    if let Err(e) = state.full(params, &audio) {
        tracing::error!("Erreur transcription Whisper pour {username}: {e}");
        return Vec::new();
    }

    let n_segments = match state.full_n_segments() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("Erreur lecture segments pour {username}: {e}");
            return Vec::new();
        }
    };

    let mut segments = Vec::new();
    for i in 0..n_segments {
        let text = match state.full_get_segment_text(i) {
            Ok(t) => t.trim().to_string(),
            Err(_) => continue,
        };

        if text.is_empty() || is_hallucination(&text) {
            if !text.is_empty() {
                tracing::debug!("Segment filtré (hallucination): {:?}", text);
            }
            continue;
        }

        // Les timestamps Whisper sont en centisecondes → convertir en secondes
        // Ajuster les timestamps avec l'offset réel de début de parole dans le batch
        let start = state.full_get_segment_t0(i).unwrap_or(0) as f64 / 100.0 + start_offset_secs;
        let end = state.full_get_segment_t1(i).unwrap_or(0) as f64 / 100.0 + start_offset_secs;

        segments.push(Segment {
            user: username.to_string(),
            start,
            end,
            text,
        });
    }

    segments
}

// ---------------------------------------------------------------------------
// API publique — appelée depuis session.rs
// ---------------------------------------------------------------------------

/// Transcrit tous les buffers audio d'un batch.
///
/// `buffers`    : HashMap<user_id, Vec<i16>>  — audio brut par utilisateur
/// `user_names` : HashMap<user_id, String>    — noms d'affichage
///
/// Retourne les segments triés par timestamp de début.
pub async fn transcribe_audio_buffers(
    buffers: &HashMap<u64, Vec<i16>>,
    user_names: &HashMap<u64, String>,
    audio_offsets: &HashMap<u64, f64>,
) -> Vec<Segment> {
    // Préparer les tâches de transcription par utilisateur
    // Chaque transcription tourne dans `spawn_blocking` (thread pool séparé)
    // car whisper.cpp est synchrone et CPU-intensive.
    let mut handles = Vec::new();

    for (&user_id, samples) in buffers {
        let username = user_names
            .get(&user_id)
            .cloned()
            .unwrap_or_else(|| format!("User#{user_id}"));
        let samples = samples.clone(); // cloner pour déplacer dans la tâche

        // spawn_blocking = exécuter du code synchrone (blocking) dans un thread pool
        // sans bloquer le runtime async — équivalent de loop.run_in_executor() en Python
        let offset = audio_offsets.get(&user_id).copied().unwrap_or(0.0);
        let handle = tokio::task::spawn_blocking(move || {
            transcribe_user_sync(&username, &samples, offset)
        });
        handles.push(handle);
    }

    // Attendre tous les résultats
    let mut all_segments = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(segments) => all_segments.extend(segments),
            Err(e) => tracing::error!("Erreur tâche de transcription : {e}"),
        }
    }

    // Trier par timestamp de début (comme dans la version Python)
    all_segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    all_segments
}
