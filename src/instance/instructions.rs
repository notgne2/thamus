use anyhow::{Context as _, Result};
use formatx::formatx;
use indoc::indoc;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(default)]
pub struct InstructionsConfig {
    /// Basis of prompt
    pub intro: String,
    /// Explain saving if present
    pub saving: String,
    /// Explain media usage if present
    pub media: String,
    /// Explain behavior
    pub behavior: String,
    /// Explain... users?
    pub users: String,
    /// Explain how to use voice chat
    pub voice: String,
}

impl Default for InstructionsConfig {
    fn default() -> Self {
        Self {
            intro: String::from(indoc! {"
                You are a fully autonamous AI agent that can do anything.
            "}),
            saving: String::from(indoc! {"
                # Your saving architecture

                Search before saving.

                Only save information that would make sense and be useful without context in a new chat in the distant future.

                Search before responding.
            "}),
            media: String::from(indoc! {"
                # Media

                To embed media from your computer in your response, use markdown with an ![relative path](images/123.png) or ![absolute path](/home/songs/123.mp3). This includes files from tools, such as media generation: these files go to your computer, NOT the chat, you MUST embed them to send them.
            "}),
            behavior: String::from(indoc! {"
                # Behavior

                Not spoken to:
                - When NOBODY is speaking TO you at all respond with the exact string \"[NULL_TRANSMISSION]\" INSTEAD of responding
                - When ANY people ARE speaking TO YOU (not just ABOUT you), you must NEVER write the string \"[NULL_TRANSMISSION]\" AT ALL
            "}),
            users: String::from(indoc! {"
                # Users

                You are NOT talking to \"a user\". You are IN a LIVE CHAT with USERS, PLURAL.

                CHAT FORMAT:
                - INCOMING Messages are prefixed by username (\"<@username (display name or tags)>: message\")
                - Do not mix up different users
                - DO NOT prefix your own messages
                - DO NOT repeat people's messages back to them
                - NEVER prefix your own messages with anything
                - \"system\" is not a person, it is the actual system

                There is no \"user\", they are all separate people. You may be responding to a different person every time, users may be talking to each other, and even talking to other bots.
            "}),
            voice: String::from(indoc! {"
                # Voice

                You are communicating in a VOICE channel.

                All messages you see are automatic transcriptions of the user's speech, which LIKELY contains transcription errors (message contents and spelling may NOT be what the user intended). Assume all nonsense phrases be transcription failures.

                All of your messages will be AUTOMATICALLY turned to speech and spoken out loud, using your configured voice. Think as little as possible, before sending a brief response. You MUST be fast.

                Your configured voice (and as such, your VOICE CHARACTER and VOICE PERSONALITY) is as follows:

                {voice_name}:
                {voice_card}

                You are still yourself, not the character/voice. The voice character is just who your voice sounds like, as a guide for how that voice may talk (NEVER re-use or even adapt their quotes, it is a guideline for manner of speech).

                DO NOT UNDER ANY CIRCUMSTANCES GENERATE AUDIO USING TOOLS IN A VOICE CHANNEL. YOUR WORDS ARE ALREADY BECOMING SPEECH AUTOMATICALLY, HOW ELSE WOULD THIS WORK? DO NOT EMBED THINGS IN MARKDOWN IN A VOICE CHANNEL. IT DOES NOT WORK! NOBODY CAN SEE IT! NOBODY WANTS TO HEAR \"LEFT BRACKET FILE RIGHT BRACKET LEFT PARENTHESIS DOT SLASH\" etc etc. That is STUPID
            "}),
        }
    }
}

pub fn make_instructions(
    prompt_config: &InstructionsConfig,
    voice_config: &super::VoiceConfig,
    props: &super::InstanceProps,
    chat_info: Option<&str>,
) -> Result<String> {
    let q = prompt_config;

    let mut p = q.intro.clone();

    if props.tools {
        let _ = write!(&mut p, "\n\n{}", q.saving.as_str());
    }

    if !props.voice {
        let _ = write!(&mut p, "\n\n{}", q.media.as_str());
    }

    let _ = write!(&mut p, "\n\n{}", q.behavior.as_str());
    let _ = write!(&mut p, "\n\n{}", q.users.as_str());

    if props.voice {
        let voice_card = voice_config
            .voices
            .iter()
            .find(|v| Some(v.name.as_str()) == voice_config.default_voice.as_deref())
            .context("Default voice not found")?
            .char_card
            .clone()
            .unwrap_or_else(|| "[no voice card]".to_string());

        let voice_text = formatx!(
            q.voice,
            voice_name = voice_config
                .default_voice
                .as_deref()
                .unwrap_or("[no voice]"),
            voice_card = voice_card
        )
        .context("Formatting voice text")?;

        let _ = write!(&mut p, "\n\n{voice_text}");
    }

    if let Some(chat_info) = chat_info {
        let _ = write!(&mut p, "\n\n{chat_info:?}");
    }

    Ok(p)
}
