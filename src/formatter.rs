// formatter.rs — Mise en forme des transcriptions pour Slack
//
// Port direct de formatter.py — la logique est identique,
// seule la syntaxe change (Rust vs Python).

use chrono::{DateTime, Utc};
use chrono_tz::America::Montreal;

use crate::session::Segment;

/// Convertit un timestamp UTC en heure locale (Montréal/Québec)
fn to_local(dt: DateTime<Utc>) -> chrono::DateTime<chrono_tz::Tz> {
    dt.with_timezone(&Montreal)
}

const SLACK_MSG_LIMIT: usize = 3000; // limite conservative par message Slack

// ---------------------------------------------------------------------------
// Helpers internes
// ---------------------------------------------------------------------------

/// Formate un timestamp en secondes → "MM:SS" ou "HH:MM:SS"
fn fmt_ts(seconds: f64) -> String {
    let total = seconds as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Découpe un texte long en chunks respectant la limite Slack.
/// Coupe sur les retours à la ligne pour ne pas couper les lignes.
pub fn split_into_chunks(text: &str) -> Vec<String> {
    if text.len() <= SLACK_MSG_LIMIT {
        return vec![text.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut current_len = 0;

    for line in text.split('\n') {
        let line_len = line.len() + 1; // +1 pour le \n
        if current_len + line_len > SLACK_MSG_LIMIT && !current_lines.is_empty() {
            chunks.push(current_lines.join("\n"));
            current_lines.clear();
            current_len = 0;
        }
        current_lines.push(line);
        current_len += line_len;
    }
    if !current_lines.is_empty() {
        chunks.push(current_lines.join("\n"));
    }

    let total = chunks.len();
    if total == 1 {
        return chunks;
    }

    // Annoter chaque chunk avec sa position
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| format!("{chunk}\n_(partie {}/{total})_", i + 1))
        .collect()
}

// ---------------------------------------------------------------------------
// Formatage d'un batch périodique (appelé pendant l'enregistrement)
// ---------------------------------------------------------------------------

pub fn format_raw_batch(
    segments: &[Segment],
    batch_number: u32,
    batch_start: DateTime<Utc>,
    batch_end: DateTime<Utc>,
) -> Vec<String> {
    let start_str = to_local(batch_start).format("%H:%M");
    let end_str = to_local(batch_end).format("%H:%M");
    let header = format!("*[Batch {batch_number} — {start_str}→{end_str}]*\n");

    if segments.is_empty() {
        return vec![format!("{header}_(aucune prise de parole détectée)_")];
    }

    let mut lines = vec![header];
    for seg in segments {
        lines.push(format!("`{}` *{}*: {}", fmt_ts(seg.start), seg.user, seg.text));
    }

    split_into_chunks(&lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Formatage final — transcription brute complète (option "raw")
// ---------------------------------------------------------------------------

pub fn format_raw_full(
    all_segments: &[Segment],
    channel_name: &str,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
) -> Vec<String> {
    let duration = (end_time - start_time).num_seconds().max(0);
    let speakers: std::collections::BTreeSet<&str> =
        all_segments.iter().map(|s| s.user.as_str()).collect();

    let local_start = to_local(start_time);
    let local_end = to_local(end_time);
    let mut lines = vec![
        format!("WAR ROOM TRANSCRIPT — #{channel_name}"),
        format!("Date     : {}", local_start.format("%Y-%m-%d")),
        format!("Start    : {}", local_start.format("%H:%M:%S %Z")),
        format!("End      : {}", local_end.format("%H:%M:%S %Z")),
        format!("Duration : {}m {}s", duration / 60, duration % 60),
        format!(
            "Speakers : {}",
            if speakers.is_empty() {
                "aucun détecté".to_string()
            } else {
                speakers.into_iter().collect::<Vec<_>>().join(", ")
            }
        ),
        String::new(),
        "---".to_string(),
        String::new(),
    ];

    if all_segments.is_empty() {
        lines.push("(no speech detected)".to_string());
    } else {
        for seg in all_segments {
            lines.push(format!("[{}] {}: {}", fmt_ts(seg.start), seg.user, seg.text));
        }
    }

    split_into_chunks(&lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Formatage final — résumé structuré (option "structured")
// ---------------------------------------------------------------------------

pub fn format_structured(
    all_segments: &[Segment],
    channel_name: &str,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
) -> Vec<String> {
    let duration = (end_time - start_time).num_seconds().max(0);
    let mut participants: Vec<&str> = all_segments
        .iter()
        .map(|s| s.user.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    participants.sort();

    let local_start = to_local(start_time);
    let local_end = to_local(end_time);
    let mut lines = vec![
        format!("*Résumé War Room — #{channel_name}*"),
        format!("*Date :* {}", local_start.format("%Y-%m-%d")),
        format!(
            "*Durée :* {}m {}s  |  *Début :* {}  |  *Fin :* {}",
            duration / 60,
            duration % 60,
            local_start.format("%H:%M %Z"),
            local_end.format("%H:%M %Z"),
        ),
        format!(
            "*Participants :* {}",
            if participants.is_empty() {
                "aucun détecté".to_string()
            } else {
                participants.join(", ")
            }
        ),
        String::new(),
        "─────────────────────────".to_string(),
        "*Timeline complète*".to_string(),
        String::new(),
    ];

    if all_segments.is_empty() {
        lines.push("_(aucune prise de parole détectée)_".to_string());
    } else {
        for seg in all_segments {
            lines.push(format!("`{}` *{}*: {}", fmt_ts(seg.start), seg.user, seg.text));
        }
    }

    split_into_chunks(&lines.join("\n"))
}
