// main.rs — Point d'entrée du bot Warroom Scribe

mod commands;
mod formatter;
mod session;
mod slack;
mod transcriber;
mod voice_handler;

use std::{collections::HashMap, sync::Arc};

use poise::serenity_prelude as serenity;
use songbird::{Config, Songbird};
use songbird::driver::DecodeMode;
use whisper_rs::install_whisper_tracing_trampoline;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Données partagées entre toutes les commandes
// ---------------------------------------------------------------------------

pub struct Data {
    pub sessions: RwLock<HashMap<serenity::GuildId, Arc<session::RecordingSession>>>,
    pub songbird: Arc<Songbird>,
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

// ---------------------------------------------------------------------------
// Handler d'événements pour l'enregistrement des commandes slash au démarrage
// ---------------------------------------------------------------------------

struct ReadyHandler {
    slash_commands: Vec<serenity::CreateCommand<'static>>,
    /// Garantit qu'on n'enregistre les commandes qu'une seule fois.
    /// Si Ready est reçu plusieurs fois (reconnexion), on skip les suivants.
    registered: std::sync::atomic::AtomicBool,
}

#[serenity::async_trait]
impl serenity::EventHandler for ReadyHandler {
    async fn dispatch(&self, ctx: &serenity::Context, event: &serenity::FullEvent) {

        if let serenity::FullEvent::Ready { data_about_bot, .. } = event {
            tracing::info!("Bot connecté : {} (Ready reçu)", data_about_bot.user.name);

            // Ne pas ré-enregistrer si Ready est reçu plusieurs fois (reconnexion gateway)
            if self.registered.swap(true, std::sync::atomic::Ordering::SeqCst) {
                tracing::debug!("Ready reçu à nouveau — commandes déjà enregistrées, skip.");
                return;
            }

            // Si DISCORD_GUILD_ID est défini → enregistrement instantané sur ce guild (dev)
            // Sinon → enregistrement global (~1h de propagation, pour la prod)
            if let Ok(guild_id_str) = std::env::var("DISCORD_GUILD_ID") {
                let guild_id: u64 = guild_id_str.parse().expect("DISCORD_GUILD_ID doit être un entier");
                let guild_id = serenity::GuildId::new(guild_id);
                match guild_id.set_commands(&ctx.http, &self.slash_commands).await {
                    Ok(cmds) => tracing::info!("{} commande(s) enregistrée(s) sur le guild (instantané).", cmds.len()),
                    Err(e) => tracing::warn!("Impossible d'enregistrer les commandes sur le guild : {e}"),
                }
            } else {
                match serenity::Command::set_global_commands(&ctx.http, &self.slash_commands).await {
                    Ok(cmds) => tracing::info!("{} commande(s) slash enregistrée(s) globalement (~1h).", cmds.len()),
                    Err(e) => tracing::warn!("Impossible d'enregistrer les commandes : {e}"),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Point d'entrée
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // Rediriger les logs internes de whisper.cpp vers tracing (silencés par le filtre par défaut)
    install_whisper_tracing_trampoline();

    // Rustls nécessite qu'un CryptoProvider soit installé manuellement quand
    // plusieurs providers (aws-lc-rs et ring) sont compilés ensemble.
    // On choisit ring explicitement.
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap_or_else(|_| {
            // install_default() échoue si un provider est déjà installé — c'est ok
            tracing::debug!("CryptoProvider déjà installé");
        });

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warroom_scribe=debug,songbird=info".into()),
        )
        .init();

    let token = serenity::Token::from_env("DISCORD_BOT_TOKEN")
        .expect("DISCORD_BOT_TOKEN manquant dans .env");

    let intents = serenity::GatewayIntents::non_privileged()
        | serenity::GatewayIntents::GUILD_VOICE_STATES
        | serenity::GatewayIntents::GUILD_MEMBERS;

    // Manager Songbird (avec DAVE intégré via le fork tazz4843)
    // DecodeMode::Decode = déchiffre ET décode Opus→PCM (nécessaire pour decoded_voice)
    let songbird_config = Config::default().decode_mode(DecodeMode::Decode(Default::default()));
    let manager = Songbird::serenity_from_config(songbird_config);

    // Définir les commandes poise
    let poise_commands = vec![commands::warroom()];

    // Convertir en commandes serenity pour l'enregistrement (dans ReadyHandler)
    let slash_commands = poise::builtins::create_application_commands(&poise_commands);

    // Framework poise
    let framework = poise::Framework::new(poise::FrameworkOptions {
        commands: poise_commands,
        on_error: |err| Box::pin(async move {
            match err {
                poise::FrameworkError::Command { error, ctx, .. } => {
                    let msg = format!("Erreur dans `{}` : {error}", ctx.command().qualified_name);
                    tracing::error!("{msg}");
                    ctx.say(format!(":x: {msg}")).await.ok();
                }
                _ => tracing::error!("Erreur poise : {err}"),
            }
        }),
        // Discord efface les commandes guild quand le bot répond à une interaction.
        // On re-enregistre systématiquement après chaque commande comme contournement.
        post_command: |ctx| Box::pin(async move {
            // Discord efface les commandes guild lors d'une réponse d'interaction.
            // On re-enregistre via reqwest direct (bypass du rate-limiter serenity qui hang).
            let Some(guild_id) = ctx.guild_id() else { return };
            let Some(app_id) = ctx.serenity_context().http.application_id() else { return };
            let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") else { return };

            let cmds = vec![commands::warroom()];
            let slash_cmds = poise::builtins::create_application_commands(&cmds);
            let url = format!(
                "https://discord.com/api/v10/applications/{}/guilds/{}/commands",
                app_id.get(), guild_id.get()
            );

            // Fonction helper pour envoyer la requête PUT
            async fn put_commands(
                url: &str,
                token: &str,
                slash_cmds: &impl serde::Serialize,
            ) -> Option<reqwest::Response> {
                reqwest::Client::new()
                    .put(url)
                    .header("Authorization", format!("Bot {token}"))
                    .json(slash_cmds)
                    .send()
                    .await
                    .ok()
            }

            // Délai court : laisser Discord finir d'effacer avant de re-enregistrer.
            // Sans délai, on peut enregistrer AVANT que Discord efface → race condition.
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;

            let resp = match put_commands(&url, &token, &slash_cmds).await {
                Some(r) => r,
                None => { tracing::warn!("[post_command] Erreur reqwest"); return; }
            };

            if resp.status().is_success() {
                tracing::info!("[post_command] Re-enregistré: HTTP {}", resp.status());
                // Deuxième passe 4s plus tard : couvre un éventuel effacement différé de Discord
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                    if let Some(r) = put_commands(&url, &token, &slash_cmds).await {
                        tracing::debug!("[post_command] Confirmé (pass 2): HTTP {}", r.status());
                    }
                });
                return;
            }

            // 429 : lire retry_after et re-schedules dans un background task
            if resp.status().as_u16() == 429 {
                let retry_after = resp.json::<serde_json::Value>().await
                    .ok()
                    .and_then(|v| v["retry_after"].as_f64())
                    .unwrap_or(10.0);
                tracing::info!("[post_command] Rate limited, retry dans {retry_after:.1}s");
                let wait = std::time::Duration::from_secs_f64(retry_after + 1.0);
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    match put_commands(&url, &token, &slash_cmds).await {
                        Some(r) => tracing::info!("[post_command] Retry: HTTP {}", r.status()),
                        None => tracing::warn!("[post_command] Retry erreur"),
                    }
                });
            } else {
                tracing::warn!("[post_command] HTTP inattendu: {}", resp.status());
            }
        }),
        ..Default::default()
    });

    // Les données partagées entre les commandes
    let data = Arc::new(Data {
        sessions: RwLock::new(HashMap::new()),
        songbird: Arc::clone(&manager),
    });

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(Box::new(framework))
        .voice_manager(Arc::clone(&manager) as Arc<dyn serenity::VoiceGatewayManager>)
        .data(data)
        // EventHandler séparé : enregistre les commandes slash au Ready
        .event_handler(Arc::new(ReadyHandler {
            slash_commands,
            registered: std::sync::atomic::AtomicBool::new(false),
        }))
        .await
        .expect("Erreur lors de la création du client Discord");

    tracing::info!(
        "Warroom Scribe démarré. Modèle Whisper : {}",
        std::env::var("WHISPER_MODEL_PATH").unwrap_or_else(|_| "non configuré".into())
    );

    client.start().await.expect("Erreur lors du démarrage du bot");
}
