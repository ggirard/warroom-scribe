// slack.rs — Appels à l'API Slack via reqwest (HTTP client async)
//
// Utilise l'API Web Slack directement (pas de SDK) — juste deux endpoints :
//   POST /chat.postMessage  → poster un message (ou créer un thread)
//
// La réponse Slack a toujours la forme : {"ok": true/false, "ts": "...", ...}

use std::sync::OnceLock;

use reqwest::Client;
use serde::Deserialize;

// Client HTTP partagé — reqwest::Client est thread-safe et réutilisable
static HTTP: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    HTTP.get_or_init(Client::new)
}

fn slack_token() -> String {
    std::env::var("SLACK_BOT_TOKEN").expect("SLACK_BOT_TOKEN manquant dans .env")
}

// Réponse partielle de l'API Slack (on ne désérialise que ce dont on a besoin)
#[derive(Deserialize)]
struct SlackResponse {
    ok: bool,
    ts: Option<String>,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// API publique
// ---------------------------------------------------------------------------

/// Poster le message d'ouverture et créer le thread Slack.
/// Retourne le `thread_ts` (timestamp du message parent) pour y répondre ensuite.
pub async fn create_thread(channel: &str, text: &str) -> Result<String, crate::Error> {
    let resp = post_message(channel, None, text).await?;

    resp.ts.ok_or_else(|| {
        format!(
            "Slack n'a pas retourné de ts. Erreur : {}",
            resp.error.unwrap_or_default()
        )
        .into()
    })
}

/// Poster un ou plusieurs chunks de texte dans un thread Slack existant.
pub async fn post_to_thread(
    channel: &str,
    thread_ts: &str,
    chunks: &[String],
) -> Result<(), crate::Error> {
    for chunk in chunks {
        post_message(channel, Some(thread_ts), chunk).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fonction interne
// ---------------------------------------------------------------------------

async fn post_message(
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
) -> Result<SlackResponse, crate::Error> {
    // Construire le body JSON avec serde_json::json! (macro pratique)
    let mut body = serde_json::json!({
        "channel": channel,
        "text": text,
        "mrkdwn": true,
        "unfurl_links": false,
    });

    // Ajouter thread_ts si on répond dans un thread
    if let Some(ts) = thread_ts {
        body["thread_ts"] = serde_json::Value::String(ts.to_string());
    }

    let resp = client()
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(slack_token()) // Authorization: Bearer xoxb-...
        .json(&body)
        .send()
        .await?
        .json::<SlackResponse>()
        .await?;

    if !resp.ok {
        return Err(format!(
            "Slack API erreur : {}",
            resp.error.unwrap_or_else(|| "inconnu".into())
        )
        .into());
    }

    Ok(resp)
}
