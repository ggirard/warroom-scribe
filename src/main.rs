// main.rs — Point d'entrée du bot Warroom Scribe

mod commands;
mod formatter;
mod session;
mod slack;
mod transcriber;
mod voice_handler;

use std::{collections::HashMap, sync::Arc};

use poise::serenity_prelude as serenity;
use songbird::{Config, SerenityInit};
use songbird::driver::DecodeMode;
use whisper_rs::install_whisper_tracing_trampoline;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Données partagées entre toutes les commandes
// ---------------------------------------------------------------------------

pub struct Data {
    pub sessions: RwLock<HashMap<serenity::GuildId, Arc<session::RecordingSession>>>,
    pub songbird: Arc<songbird::Songbird>,
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Context<'a> = poise::Context<'a, Data, Error>;

// ---------------------------------------------------------------------------
// Point d'entrée
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // Rediriger les logs internes de whisper.cpp vers tracing
    install_whisper_tracing_trampoline();

    // Rustls nécessite qu'un CryptoProvider soit installé manuellement quand
    // plusieurs providers (aws-lc-rs et ring) sont compilés ensemble.
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap_or_else(|_| {
            tracing::debug!("CryptoProvider déjà installé");
        });

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warroom_scribe=debug,songbird=info".into()),
        )
        .init();

    let token = std::env::var("DISCORD_BOT_TOKEN")
        .expect("DISCORD_BOT_TOKEN manquant dans .env");

    let intents = serenity::GatewayIntents::non_privileged()
        | serenity::GatewayIntents::GUILD_VOICE_STATES
        | serenity::GatewayIntents::GUILD_MEMBERS;

    // Manager Songbird avec décodage DAVE/E2EE + Opus→PCM
    let songbird_config = Config::default().decode_mode(DecodeMode::Decode(Default::default()));
    let manager = songbird::Songbird::serenity_from_config(songbird_config);
    let manager_for_data = Arc::clone(&manager);

    let framework = poise::Framework::new(
        poise::FrameworkOptions {
            commands: vec![commands::warroom()],
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
            // Re-enregistre les commandes guild après chaque interaction.
            // Discord efface parfois les commandes guild quand le bot répond — workaround.
            post_command: |ctx| Box::pin(async move {
                let Some(guild_id) = ctx.guild_id() else { return };
                let Some(app_id) = ctx.serenity_context().http.application_id() else { return };
                let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") else { return };

                let cmds = vec![commands::warroom()];
                let slash_cmds = poise::builtins::create_application_commands(&cmds);
                let url = format!(
                    "https://discord.com/api/v10/applications/{}/guilds/{}/commands",
                    app_id.get(), guild_id.get()
                );

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

                tokio::time::sleep(std::time::Duration::from_millis(800)).await;

                let resp = match put_commands(&url, &token, &slash_cmds).await {
                    Some(r) => r,
                    None => { tracing::warn!("[post_command] Erreur reqwest"); return; }
                };

                if resp.status().is_success() {
                    tracing::info!("[post_command] Re-enregistré: HTTP {}", resp.status());
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                        if let Some(r) = put_commands(&url, &token, &slash_cmds).await {
                            tracing::debug!("[post_command] Confirmé (pass 2): HTTP {}", r.status());
                        }
                    });
                    return;
                }

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
        },
        move |ctx, ready, framework| {
            let manager_for_data = manager_for_data;
            Box::pin(async move {
                tracing::info!("Bot connecté : {} (Ready reçu)", ready.user.name);

                // Si DISCORD_GUILD_ID défini → enregistrement instantané sur ce guild (dev)
                // Sinon → enregistrement global (~1h de propagation, pour la prod)
                if let Ok(guild_id_str) = std::env::var("DISCORD_GUILD_ID") {
                    let guild_id: u64 = guild_id_str.parse().expect("DISCORD_GUILD_ID doit être un entier");
                    let guild_id = serenity::GuildId::new(guild_id);
                    match poise::builtins::register_in_guild(ctx, &framework.options().commands, guild_id).await {
                        Ok(()) => tracing::info!("Commandes enregistrées sur le guild (instantané)."),
                        Err(e) => tracing::warn!("Impossible d'enregistrer les commandes sur le guild : {e}"),
                    }
                } else {
                    match poise::builtins::register_globally(ctx, &framework.options().commands).await {
                        Ok(()) => tracing::info!("Commandes slash enregistrées globalement (~1h)."),
                        Err(e) => tracing::warn!("Impossible d'enregistrer les commandes : {e}"),
                    }
                }

                Ok(Data {
                    sessions: RwLock::new(HashMap::new()),
                    songbird: manager_for_data,
                })
            })
        },
    );

    let mut client = serenity::ClientBuilder::new(&token, intents)
        .framework(framework)
        .register_songbird_with(manager)
        .await
        .expect("Erreur lors de la création du client Discord");

    tracing::info!(
        "Warroom Scribe démarré. Modèle Whisper : {}",
        std::env::var("WHISPER_MODEL_PATH").unwrap_or_else(|_| "non configuré".into())
    );

    client.start().await.expect("Erreur lors du démarrage du bot");
}
