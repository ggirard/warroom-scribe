// commands.rs — Commandes slash /warroom start | stop | status

use std::{collections::HashMap, sync::Arc};

use poise::serenity_prelude as serenity;
use songbird::CoreEvent;

use crate::{session::RecordingSession, slack, voice_handler::Handler, Context, Error};

// Enum pour le choix du format de sortie — poise génère les choices Discord automatiquement
#[derive(poise::ChoiceParameter, Debug, Clone, Copy)]
pub enum OutputFormat {
    #[name = "raw"]
    Raw,
    #[name = "structured"]
    Structured,
    #[name = "both"]
    Both,
}

/// Warroom Scribe — enregistrement et transcription vocale vers Slack
#[poise::command(
    slash_command,
    subcommands("start", "stop", "status"),
    subcommand_required
)]
pub async fn warroom(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

// ---------------------------------------------------------------------------
// /warroom start
// ---------------------------------------------------------------------------

/// Rejoindre ton channel vocal et démarrer l'enregistrement
#[poise::command(slash_command, guild_only)]
pub async fn start(
    ctx: Context<'_>,
    #[description = "Channel Slack cible (ex: #warroom)"] slack_channel: Option<String>,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();

    // Stocker le data dans une variable pour éviter le "temporary dropped" de Rust
    let data = ctx.data();

    {
        let sessions = data.sessions.read().await;
        if sessions.contains_key(&guild_id) {
            ctx.say("Un enregistrement est déjà en cours. Utilise `/warroom stop` pour le terminer.")
                .await?;
            return Ok(());
        }
    }

    // Trouver le voice channel et les noms des membres depuis le cache serenity
    // serenity 0.12 : voice_states et members sont des HashMap → iter() retourne des (&key, &val)
    let (voice_channel_id, initial_user_names): (Option<serenity::ChannelId>, HashMap<u64, String>) = {
        let guild = ctx.guild().ok_or("Guild non trouvé dans le cache")?;
        let author_id = ctx.author().id;
        let channel_id = guild
            .voice_states
            .get(&author_id)
            .and_then(|vs| vs.channel_id);

        // Membres depuis le cache (disponible immédiatement, peut être incomplet)
        let names_from_cache: HashMap<u64, String> = guild
            .members
            .iter()
            .map(|(id, member)| (id.get(), member.display_name().to_string()))
            .collect();

        (channel_id, names_from_cache)
    };

    // Compléter avec l'API HTTP pour les membres absents du cache
    let mut initial_user_names = initial_user_names;
    match ctx.http().get_guild_members(guild_id, None, None).await {
        Ok(members) => {
            for member in members {
                initial_user_names
                    .entry(member.user.id.get())
                    .or_insert_with(|| member.display_name().to_string());
            }
            tracing::info!("{} membre(s) chargés pour la session", initial_user_names.len());
        }
        Err(e) => tracing::warn!("Impossible de fetcher les membres via HTTP : {e}"),
    }

    let Some(channel_id) = voice_channel_id else {
        ctx.say("Tu dois être dans un channel vocal pour démarrer l'enregistrement.")
            .await?;
        return Ok(());
    };

    let target_slack = slack_channel.unwrap_or_else(|| {
        std::env::var("SLACK_DEFAULT_CHANNEL").unwrap_or_else(|_| "#warroom".to_string())
    });
    let flush_interval_secs: u64 = std::env::var("FLUSH_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    // Nom du channel vocal (depuis le cache guild)
    let channel_name = ctx
        .guild()
        .and_then(|g| g.channels.get(&channel_id).map(|c| c.name.clone()))
        .unwrap_or_else(|| channel_id.get().to_string());

    // Répondre immédiatement à l'interaction (évite ctx.defer() qui efface les commandes guild)
    ctx.say(format!(":hourglass: Connexion à **{channel_name}** en cours..."))
        .await?;

    // Connexion au voice channel via Songbird
    let call_lock = match data.songbird.join(guild_id, channel_id).await {
        Ok(call) => call,
        Err(e) => {
            ctx.say(format!(
                ":x: Impossible de rejoindre le channel vocal : `{e}`\n\
                 Si tu vois `4017`, Discord requiert le protocole DAVE E2EE."
            ))
            .await?;
            return Ok(());
        }
    };

    // Créer le thread Slack d'ouverture
    let now_local = chrono::Utc::now().with_timezone(&chrono_tz::America::Montreal);
    let now_fmt = now_local.format("%Y-%m-%d %H:%M %Z");
    let thread_ts = slack::create_thread(
        &target_slack,
        &format!(
            ":red_circle: *War room démarrée* — `#{channel_name}` — {now_fmt}\n\
             Les batches de transcription brute seront postés ici toutes les {} min.",
            flush_interval_secs / 60
        ),
    )
    .await?;

    // Créer la session d'enregistrement
    let session = Arc::new(RecordingSession::new(
        guild_id,
        channel_name.clone(),
        target_slack.clone(),
        thread_ts,
        flush_interval_secs,
        initial_user_names,
    ));

    // Enregistrer le handler audio dans Songbird
    {
        let mut call = call_lock.lock().await;
        let handler = Handler::new(Arc::clone(&session));
        call.add_global_event(CoreEvent::VoiceTick.into(), handler.clone());
        call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), handler.clone());
        call.add_global_event(CoreEvent::ClientDisconnect.into(), handler);
    }

    session.start().await;

    {
        let mut sessions = data.sessions.write().await;
        sessions.insert(guild_id, Arc::clone(&session));
    }

    ctx.say(format!(
        ":red_circle: Enregistrement démarré dans **{channel_name}**.\n\
         Thread Slack ouvert dans `{target_slack}`. Utilise `/warroom stop` pour terminer."
    ))
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// /warroom stop
// ---------------------------------------------------------------------------

/// Arrêter l'enregistrement et poster la transcription dans Slack
#[poise::command(slash_command, guild_only)]
pub async fn stop(
    ctx: Context<'_>,
    #[description = "Format de sortie"] output: Option<OutputFormat>,
) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let data = ctx.data();

    let session = {
        let mut sessions = data.sessions.write().await;
        sessions.remove(&guild_id)
    };

    let Some(session) = session else {
        ctx.say("Aucun enregistrement actif.").await?;
        return Ok(());
    };

    // Déconnecter immédiatement du voice channel — n'a pas besoin d'attendre la transcription
    data.songbird.remove(guild_id).await.ok();

    let output = output.unwrap_or(OutputFormat::Structured);
    ctx.say(format!(
        ":stop_button: Déconnecté de **{}**. Transcription en cours en arrière-plan...",
        session.voice_channel_name
    ))
    .await?;

    // Lancer le traitement (flush + transcription + post Slack) en background
    let http = ctx.serenity_context().http.clone();
    let channel_id = ctx.channel_id();

    tokio::spawn(async move {
        let (end_time, all_segments) = session.stop().await;

        if matches!(output, OutputFormat::Raw | OutputFormat::Both) {
            let chunks = crate::formatter::format_raw_full(
                &all_segments,
                &session.voice_channel_name,
                session.start_time,
                end_time,
            );
            if let Err(e) = slack::post_to_thread(
                &session.slack_channel, &session.slack_thread_ts, &chunks,
            ).await {
                tracing::error!("Erreur post Slack final (raw): {e}");
            }
        }

        if matches!(output, OutputFormat::Structured | OutputFormat::Both) {
            let chunks = crate::formatter::format_structured(
                &all_segments,
                &session.voice_channel_name,
                session.start_time,
                end_time,
            );
            if let Err(e) = slack::post_to_thread(
                &session.slack_channel, &session.slack_thread_ts, &chunks,
            ).await {
                tracing::error!("Erreur post Slack final (structured): {e}");
            }
        }

        let output_name = match output {
            OutputFormat::Raw => "raw",
            OutputFormat::Structured => "structured",
            OutputFormat::Both => "both",
        };

        channel_id.say(
            &http,
            format!(
                ":white_check_mark: Transcription **{output_name}** terminée — postée dans `{}`.",
                session.slack_channel
            ),
        ).await.ok();
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// /warroom status
// ---------------------------------------------------------------------------

/// Vérifier le statut de l'enregistrement en cours
#[poise::command(slash_command, guild_only)]
pub async fn status(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let data = ctx.data();

    let sessions = data.sessions.read().await;
    let Some(session) = sessions.get(&guild_id) else {
        ctx.say("Aucun enregistrement actif.").await?;
        return Ok(());
    };

    ctx.say(format!(
        ":red_circle: Enregistrement actif dans **{}** depuis **{}** ({} batch(s) traité(s)).",
        session.voice_channel_name,
        session.duration_str(),
        session.batch_count(),
    ))
    .await?;

    Ok(())
}
